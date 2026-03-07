// src/main.rs — Thelio Io 2 Windows Service
//
// This is the top-level entry point.  The binary can be run in two modes:
//
//   1. As a Windows service (normal deployment):
//        sc create thelio-io2 binPath= "C:\path\to\thelio-io2-daemon.exe"
//        sc start thelio-io2
//
//   2. As a foreground console process (development / debugging):
//        thelio-io2-daemon.exe --console [--profile <quiet|balanced|performance|manual>]
//
// Architecture:
//   ┌──────────────────────────────────────────┐
//   │  Windows Service Control Manager (SCM)   │
//   └────────────────┬─────────────────────────┘
//                    │ service_main()
//   ┌────────────────▼─────────────────────────┐
//   │           Service Loop Thread             │
//   │  - device discovery & reconnect           │
//   │  - handles IpcRequest from IPC server     │
//   │  - handles PowerEvent from power module   │
//   │  - thermal polling + fan curve control    │
//   └────────────────┬─────────────────────────┘
//                    │
//         ┌──────────┼──────────┐
//         │          │          │
//   ┌─────▼──────┐ ┌▼────────┐ ┌▼──────────────────────────┐
//   │ IPC Server │ │ Power   │ │ Thermal Source             │
//   │ (thread)   │ │ Monitor │ │ HTTP (LHM web server) or   │
//   └────────────┘ └─────────┘ │ Library (lhm-helper.exe)   │
//                               └────────────────────────────┘

mod device;
mod eventlog;
mod fan_curve;
mod ipc;
mod power;
mod thermal;
mod thermal_lib;
mod thelio_io;

use std::{
    env,
    path::PathBuf,
    sync::{
        mpsc::{channel, Receiver, TryRecvError},
        OnceLock,
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use log::{debug, error, info, warn};
use parking_lot::Mutex;
use windows_service::{
    define_windows_service,
    service::{
        ServiceControl, ServiceControlAccept, ServiceExitCode, ServiceState, ServiceStatus,
        ServiceType,
    },
    service_control_handler::{self, ServiceControlHandlerResult},
    service_dispatcher,
};

use crate::{
    device::{Device, DeviceCommand, DeviceError, IpcResponse},
    fan_curve::{Profile, TempHysteresis},
    ipc::IpcRequest,
    power::PowerEvent,
    thermal::{LhmConfig, LhmMode, ThermalReading, ThermalSource},
};

// ── Service name ───────────────────────────────────────────────────────────
const SERVICE_NAME: &str = "thelio-io2";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

// How often to attempt device reconnection when not connected.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

// How often to poll temperature and adjust fan speeds.
const THERMAL_POLL_INTERVAL: Duration = Duration::from_secs(2);

// How often to log a periodic status summary at INFO level.
// Individual poll results are logged at DEBUG level.
const STATUS_LOG_INTERVAL: Duration = Duration::from_secs(30);

// How often to retry connecting to LHM when the thermal reader is unavailable.
const LHM_RETRY_INTERVAL: Duration = Duration::from_secs(30);

// After this many consecutive HTTP failures, declare LHM disconnected and
// switch to manual mode until the connection is restored.
const MAX_CONSECUTIVE_HTTP_FAILURES: u32 = 5;

// ── Shared state for profile (accessible from SCM handler thread) ────────
// The SCM handler runs on a different thread, so we use a static Mutex for
// the initial profile setting.  The actual mutable profile state lives in
// the device loop.
static INITIAL_PROFILE: Mutex<Profile> = Mutex::new(Profile::Balanced);

/// Stop-signal sender for console mode; used by the console control handler
/// (which runs on a separate OS thread) to tell the device loop to exit.
static CONSOLE_STOP_TX: OnceLock<std::sync::mpsc::Sender<()>> = OnceLock::new();

/// LHM connection configuration, parsed once in main() and read by device_loop().
static LHM_CONFIG: OnceLock<LhmConfig> = OnceLock::new();

/// Which temperature reading backend to use (http or library).
static LHM_MODE: OnceLock<LhmMode> = OnceLock::new();

/// Path to the lhm-helper.exe sidecar (used in library mode).
static LHM_HELPER_PATH: OnceLock<PathBuf> = OnceLock::new();

// ── Entry point ───────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    // Determine mode and log level before initializing the logger.
    let log_level = parse_log_level_arg(&args);
    let console_mode = args.iter().any(|a| a == "--console");

    // Use stdout logger in console mode, Windows Event Log in service mode.
    if console_mode {
        simple_logger::init_with_level(log_level).unwrap_or_default();
    } else {
        eventlog::init(log_level);
    }

    // Parse --profile argument (applies to both console and service mode).
    let profile = parse_profile_arg(&args);
    *INITIAL_PROFILE.lock() = profile;
    info!("Initial profile: {profile}");

    // Parse LHM mode and connection settings (applies to both console and service mode).
    let lhm_mode = parse_lhm_mode(&args);
    let lhm_config = parse_lhm_config(&args);
    let helper_path = parse_lhm_helper_path(&args);
    info!("LHM mode: {}", if lhm_mode == LhmMode::Http { "http" } else { "library" });
    if lhm_mode == LhmMode::Http {
        info!("LHM URL: {}", lhm_config.url);
    } else {
        info!("LHM helper: {}", helper_path.display());
    }
    LHM_CONFIG.set(lhm_config).ok();
    LHM_MODE.set(lhm_mode).ok();
    LHM_HELPER_PATH.set(helper_path).ok();

    if console_mode {
        info!("Running in console mode (--console)");
        return run_console(profile);
    }

    // Start the Windows service dispatcher.
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| anyhow::anyhow!("service_dispatcher::start failed: {e}"))?;

    Ok(())
}

/// Parse --log-level <level> from args, defaulting to Info.
/// Accepts: error, warn, info, debug, trace (case-insensitive).
fn parse_log_level_arg(args: &[String]) -> log::Level {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--log-level" {
            if let Some(name) = args.get(i + 1) {
                match name.to_lowercase().as_str() {
                    "error" => return log::Level::Error,
                    "warn" => return log::Level::Warn,
                    "info" => return log::Level::Info,
                    "debug" => return log::Level::Debug,
                    "trace" => return log::Level::Trace,
                    _ => {
                        eprintln!("Unknown log level '{name}'; using info");
                    }
                }
            }
        }
    }
    log::Level::Info
}

/// Parse --profile <name> from args, defaulting to Balanced.
fn parse_profile_arg(args: &[String]) -> Profile {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--profile" {
            if let Some(name) = args.get(i + 1) {
                if let Some(p) = Profile::from_str_loose(name) {
                    return p;
                } else {
                    warn!("Unknown profile '{name}'; using balanced");
                }
            }
        }
    }
    Profile::Balanced
}

/// Parse LHM connection settings from CLI args.
///
/// Flags:
///   --lhm-url <url>         Base URL (default: http://localhost:8085)
///   --lhm-user <username>   HTTP Basic Auth username (optional)
///   --lhm-password <pass>   HTTP Basic Auth password (optional)
fn parse_lhm_config(args: &[String]) -> LhmConfig {
    let mut config = LhmConfig::default();

    for (i, arg) in args.iter().enumerate() {
        match arg.as_str() {
            "--lhm-url" => {
                if let Some(url) = args.get(i + 1) {
                    config.url = url.clone();
                }
            }
            "--lhm-user" => {
                if let Some(user) = args.get(i + 1) {
                    config.username = Some(user.clone());
                }
            }
            "--lhm-password" => {
                if let Some(pass) = args.get(i + 1) {
                    config.password = Some(pass.clone());
                }
            }
            _ => {}
        }
    }

    config
}

/// Parse --lhm-mode <http|library> from args, defaulting to Http.
fn parse_lhm_mode(args: &[String]) -> LhmMode {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--lhm-mode" {
            if let Some(mode) = args.get(i + 1) {
                match mode.to_lowercase().as_str() {
                    "http" => return LhmMode::Http,
                    "library" | "lib" => return LhmMode::Library,
                    _ => {
                        eprintln!("Unknown --lhm-mode '{mode}'; using http");
                    }
                }
            }
        }
    }
    LhmMode::Http
}

/// Parse --lhm-helper-path <path> from args.
///
/// Defaults to `lhm-helper.exe` in the same directory as the daemon executable.
fn parse_lhm_helper_path(args: &[String]) -> PathBuf {
    for (i, arg) in args.iter().enumerate() {
        if arg == "--lhm-helper-path" {
            if let Some(path) = args.get(i + 1) {
                return PathBuf::from(path);
            }
        }
    }
    // Default: same directory as the current executable.
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|p| p.join("lhm-helper.exe")))
        .unwrap_or_else(|| PathBuf::from("lhm-helper.exe"))
}

// ── Console (debug) mode ───────────────────────────────────────────────────

fn run_console(profile: Profile) -> Result<()> {
    info!("=== System76 Io Daemon (console mode) ===");
    info!("Profile: {profile}");
    info!("Press Ctrl+C to stop.");

    // In console mode we don't monitor power events, but the device loop
    // still expects a Receiver<PowerEvent>.  The sender is intentionally
    // unused (prefixed with `_`).
    let (_power_tx, power_rx) = channel::<PowerEvent>();
    let (device_tx, device_rx) = channel::<IpcRequest>();
    let (stop_tx, stop_rx) = channel::<()>();

    // Register a console control handler so Ctrl+C and console-close
    // trigger a clean shutdown through the device loop's stop channel.
    CONSOLE_STOP_TX.set(stop_tx).ok();
    unsafe {
        use windows::Win32::System::Console::SetConsoleCtrlHandler;
        let _ = SetConsoleCtrlHandler(Some(console_ctrl_handler), true);
    }

    // Start the IPC server.
    ipc::start(device_tx)?;

    // Run the device loop on the main thread — blocks until stop signal.
    device_loop(device_rx, power_rx, Some(stop_rx), profile);

    info!("Console mode exiting");
    Ok(())
}

/// Called by Windows on Ctrl+C, Ctrl+Break, or console window close.
/// Runs on a separate OS thread created by the system.
unsafe extern "system" fn console_ctrl_handler(ctrl_type: u32) -> windows::core::BOOL {
    // CTRL_C_EVENT = 0, CTRL_BREAK_EVENT = 1, CTRL_CLOSE_EVENT = 2
    if ctrl_type <= 2 {
        if let Some(tx) = CONSOLE_STOP_TX.get() {
            let _ = tx.send(());
        }
        windows::core::BOOL(1) // handled — don't terminate immediately
    } else {
        windows::core::BOOL(0) // let the system handle it
    }
}

// ── Windows service plumbing ───────────────────────────────────────────────

define_windows_service!(ffi_service_main, service_main);

fn service_main(_args: Vec<std::ffi::OsString>) {
    if let Err(e) = run_service() {
        error!("Service error: {e}");
    }
}

fn run_service() -> Result<()> {
    info!("=== System76 Io Service starting ===");

    let profile = *INITIAL_PROFILE.lock();
    info!("Service profile: {profile}");

    // Channels for cross-thread communication.
    let (power_tx, power_rx) = channel::<PowerEvent>();
    let (device_tx, device_rx) = channel::<IpcRequest>();
    let (stop_tx, stop_rx) = channel::<()>();

    // Register our service control handler with the SCM.
    let status_handle = service_control_handler::register(SERVICE_NAME, {
        let stop_tx = stop_tx.clone();
        let power_tx = power_tx.clone();
        move |ctrl| match ctrl {
            ServiceControl::Stop | ServiceControl::Shutdown => {
                info!("SCM requested stop/shutdown");
                let _ = stop_tx.send(());
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::PowerEvent(param) => {
                use windows_service::service::PowerEventParam;
                match param {
                    PowerEventParam::Suspend => {
                        info!("SCM power event: suspending");
                        let _ = power_tx.send(PowerEvent::Suspending);
                    }
                    PowerEventParam::ResumeSuspend | PowerEventParam::ResumeAutomatic => {
                        info!("SCM power event: resumed");
                        let _ = power_tx.send(PowerEvent::Resumed);
                    }
                    _ => {}
                }
                ServiceControlHandlerResult::NoError
            }
            ServiceControl::Interrogate => ServiceControlHandlerResult::NoError,
            _ => ServiceControlHandlerResult::NotImplemented,
        }
    })?;

    // Report: Running
    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Running,
        controls_accepted: ServiceControlAccept::STOP
            | ServiceControlAccept::SHUTDOWN
            | ServiceControlAccept::POWER_EVENT,
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    // Start the IPC server.
    ipc::start(device_tx)?;

    // Run the device loop.  Blocks until the service is told to stop.
    device_loop(device_rx, power_rx, Some(stop_rx), profile);

    // Report: Stopped
    status_handle.set_service_status(ServiceStatus {
        service_type: SERVICE_TYPE,
        current_state: ServiceState::Stopped,
        controls_accepted: ServiceControlAccept::empty(),
        exit_code: ServiceExitCode::Win32(0),
        checkpoint: 0,
        wait_hint: Duration::default(),
        process_id: None,
    })?;

    info!("Service stopped cleanly");
    Ok(())
}

// ── Device loop ────────────────────────────────────────────────────────────
//
// This is the heart of the daemon.  It owns the single `Box<dyn Device>`
// handle and processes IPC requests, power events, and automatic fan
// control in a single loop.

fn device_loop(
    ipc_rx: Receiver<IpcRequest>,
    power_rx: Receiver<PowerEvent>,
    stop_rx: Option<Receiver<()>>,
    initial_profile: Profile,
) {
    let mut device: Option<Box<dyn Device>> = None;
    let mut last_reconnect = Instant::now() - RECONNECT_INTERVAL;

    // ── Thermal / fan control state ──────────────────────────────────────
    let mut current_profile = initial_profile;
    let lhm_config = LHM_CONFIG.get().expect("LHM_CONFIG not initialized");
    let lhm_mode = *LHM_MODE.get().expect("LHM_MODE not initialized");
    let helper_path = LHM_HELPER_PATH.get().expect("LHM_HELPER_PATH not initialized");
    let mut thermal_reader = thermal::try_init_source(lhm_mode, lhm_config, helper_path);
    let mut hysteresis = TempHysteresis::new(2.0);
    let mut last_temp_poll = Instant::now() - THERMAL_POLL_INTERVAL;
    let mut last_reading: Option<ThermalReading> = None;
    let mut last_pwm: Option<u8> = None;
    let mut last_status_log = Instant::now() - STATUS_LOG_INTERVAL;

    // LHM retry state: when the thermal reader is unavailable, we periodically
    // attempt to reconnect.  `desired_profile` remembers what profile the user
    // (or startup config) requested so we can restore it when LHM returns.
    let mut desired_profile = initial_profile;
    let mut last_lhm_retry = Instant::now();
    let mut consecutive_http_failures: u32 = 0;

    // ── Daily max temperature tracking ──────────────────────────────────
    let mut daily_max_cpu_c: Option<f64> = None;
    let mut daily_max_gpu_c: Option<f64> = None;
    let mut daily_max_c: Option<f64> = None;
    let mut current_day_key = local_day_key();

    if thermal_reader.is_none() && !matches!(current_profile, Profile::Manual) {
        warn!(
            "No thermal reader available; switching profile to manual. \
             Fan speeds must be set via the client. \
             Will retry LHM connection every {} seconds.",
            LHM_RETRY_INTERVAL.as_secs(),
        );
        current_profile = Profile::Manual;
    }

    loop {
        // ── Stop signal ────────────────────────────────────────────────────
        if let Some(ref rx) = stop_rx {
            match rx.try_recv() {
                Ok(_) | Err(TryRecvError::Disconnected) => {
                    info!("Stop signal received; exiting device loop");
                    return;
                }
                Err(TryRecvError::Empty) => {}
            }
        }

        // ── Device reconnection ────────────────────────────────────────────
        if device.is_none() && last_reconnect.elapsed() >= RECONNECT_INTERVAL {
            last_reconnect = Instant::now();
            if let Some(d) = try_open_device() {
                info!("Connected to: {}", d.name());
                device = Some(d);
                // Immediately apply current fan profile on reconnect
                last_temp_poll = Instant::now() - THERMAL_POLL_INTERVAL;
            }
        }

        // ── Power events ───────────────────────────────────────────────────
        loop {
            match power_rx.try_recv() {
                Ok(PowerEvent::Suspending) => {
                    info!("System suspending — notifying device");
                    if let Some(ref mut d) = device {
                        if let Err(e) = d.notify_suspend() {
                            warn!("notify_suspend failed: {e}");
                            device = None;
                        }
                    }
                }
                Ok(PowerEvent::Resumed) => {
                    info!("System resumed — notifying device");
                    if let Some(ref mut d) = device {
                        if let Err(e) = d.notify_resume() {
                            warn!("notify_resume failed: {e}; will reconnect");
                            device = None;
                        }
                    }
                    // Force an immediate thermal poll on resume
                    last_temp_poll = Instant::now() - THERMAL_POLL_INTERVAL;
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }

        // ── IPC requests ───────────────────────────────────────────────────
        // Process up to MAX_IPC_BATCH requests per loop iteration to keep
        // the loop responsive to power events and thermal polling.
        const MAX_IPC_BATCH: usize = 10;
        for _ in 0..MAX_IPC_BATCH {
            match ipc_rx.try_recv() {
                Ok(req) => {
                    let response = handle_request(
                        &mut device,
                        req.command,
                        &mut current_profile,
                        &mut desired_profile,
                        &last_reading,
                        &thermal_reader,
                        DailyMaxTemps {
                            cpu_c: daily_max_cpu_c,
                            gpu_c: daily_max_gpu_c,
                            max_c: daily_max_c,
                        },
                    );
                    let _ = req.reply.send(response);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    info!("IPC channel disconnected; exiting");
                    return;
                }
            }
        }

        // ── LHM reconnection ──────────────────────────────────────────────
        // If the thermal reader is unavailable, periodically attempt to
        // reconnect.  This handles both "LHM was not running at daemon
        // startup" and "LHM went down while the daemon was running".
        if thermal_reader.is_none()
            && last_lhm_retry.elapsed() >= LHM_RETRY_INTERVAL
        {
            last_lhm_retry = Instant::now();
            debug!("Retrying LHM connection...");
            match thermal::try_init_source(lhm_mode, lhm_config, helper_path) {
                Some(reader) => {
                    info!(
                        "LHM connection established — resuming automatic fan control"
                    );
                    thermal_reader = Some(reader);
                    consecutive_http_failures = 0;

                    // Restore the user's desired profile if we had forced
                    // manual mode due to a missing thermal reader.
                    if matches!(current_profile, Profile::Manual)
                        && !matches!(desired_profile, Profile::Manual)
                    {
                        current_profile = desired_profile;
                        info!("Restored profile: {current_profile}");
                    }
                }
                None => {
                    warn!(
                        "LHM retry failed; next attempt in {} seconds",
                        LHM_RETRY_INTERVAL.as_secs()
                    );
                }
            }
        }

        // ── Thermal polling + automatic fan control ────────────────────────
        if last_temp_poll.elapsed() >= THERMAL_POLL_INTERVAL {
            last_temp_poll = Instant::now();

            if let Some(curve) = current_profile.curve() {
                if let Some(ref mut reader) = thermal_reader {
                    match reader.read_temps() {
                        Ok(reading) => {
                            consecutive_http_failures = 0;
                            let eff_temp = hysteresis.update(reading.max_c);
                            let pwm = curve.duty_pwm(eff_temp);

                            // Log every poll at DEBUG level.
                            debug!(
                                "Poll: {} (eff {:.1}°C) → PWM {pwm} ({:.0}%) [{}]",
                                reading.summary(),
                                eff_temp,
                                pwm as f64 / 255.0 * 100.0,
                                current_profile,
                            );

                            // Log at INFO when the PWM target changes.
                            let pwm_changed = last_pwm != Some(pwm);
                            if pwm_changed {
                                info!(
                                    "Fan speed change: {} → PWM {} ({:.0}%) [{}]",
                                    reading.summary(),
                                    pwm,
                                    pwm as f64 / 255.0 * 100.0,
                                    current_profile,
                                );
                            }

                            // ── Daily max update ──────────────────
                            let now_day = local_day_key();
                            if now_day != current_day_key {
                                info!(
                                    "Midnight rollover — resetting daily max temps (was {})",
                                    daily_max_c
                                        .map_or("n/a".into(), |t| format!("{t:.1}°C")),
                                );
                                daily_max_cpu_c = None;
                                daily_max_gpu_c = None;
                                daily_max_c = None;
                                current_day_key = now_day;
                            }
                            if let Some(cpu) = reading.cpu_c {
                                daily_max_cpu_c = Some(
                                    daily_max_cpu_c.map_or(cpu, |prev: f64| prev.max(cpu)),
                                );
                            }
                            if let Some(gpu) = reading.gpu_c {
                                daily_max_gpu_c = Some(
                                    daily_max_gpu_c.map_or(gpu, |prev: f64| prev.max(gpu)),
                                );
                            }
                            daily_max_c = Some(
                                daily_max_c.map_or(reading.max_c, |prev: f64| {
                                    prev.max(reading.max_c)
                                }),
                            );

                            last_reading = Some(reading);

                            if let Some(ref mut d) = device {
                                let fan_count = d.fan_count();
                                for ch in 0..fan_count {
                                    if let Err(e) = d.write_pwm(ch, pwm) {
                                        warn!("Failed to set PWM ch{ch}={pwm}: {e}");
                                        if matches!(
                                            e,
                                            DeviceError::Comm(_) | DeviceError::Timeout
                                        ) {
                                            device = None;
                                            break;
                                        }
                                    }
                                }
                                if device.is_some() {
                                    last_pwm = Some(pwm);
                                }
                            }
                        }
                        Err(e) => {
                            consecutive_http_failures += 1;
                            warn!(
                                "Temperature read failed ({consecutive_http_failures}/\
                                 {MAX_CONSECUTIVE_HTTP_FAILURES}): {e}"
                            );

                            if consecutive_http_failures >= MAX_CONSECUTIVE_HTTP_FAILURES {
                                warn!(
                                    "LHM connection lost after {} consecutive failures; \
                                     switching to manual mode. Will retry every {} seconds.",
                                    consecutive_http_failures,
                                    LHM_RETRY_INTERVAL.as_secs(),
                                );
                                thermal_reader = None;
                                last_lhm_retry = Instant::now();
                                if !matches!(current_profile, Profile::Manual) {
                                    current_profile = Profile::Manual;
                                }
                            }
                        }
                    }
                }
            }
        }

        // ── Periodic status summary (INFO level, every 30 s) ──────────────
        if last_status_log.elapsed() >= STATUS_LOG_INTERVAL {
            last_status_log = Instant::now();
            let dev_status = if device.is_some() { "connected" } else { "not connected" };
            let temp_str = last_reading
                .as_ref()
                .map(|r| r.summary())
                .unwrap_or_else(|| "unavailable".into());
            match last_pwm {
                Some(p) => info!(
                    "Status: device {dev_status}, profile={}, {temp_str}, PWM={p} ({:.0}%)",
                    current_profile,
                    p as f64 / 255.0 * 100.0,
                ),
                None => info!(
                    "Status: device {dev_status}, profile={}, {temp_str}, PWM=pending",
                    current_profile,
                ),
            }
        }

        // Brief sleep to avoid spinning 100% CPU.  20 ms gives ~50 Hz loop
        // frequency, which is fast enough for responsive IPC handling while
        // consuming negligible CPU.
        thread::sleep(Duration::from_millis(20));
    }
}

// ── Request dispatch ───────────────────────────────────────────────────────

/// Tracks the highest temperatures observed since local midnight.
#[derive(Clone, Copy, Default)]
struct DailyMaxTemps {
    cpu_c: Option<f64>,
    gpu_c: Option<f64>,
    max_c: Option<f64>,
}

fn handle_request(
    device: &mut Option<Box<dyn Device>>,
    cmd: DeviceCommand,
    current_profile: &mut Profile,
    desired_profile: &mut Profile,
    last_reading: &Option<ThermalReading>,
    thermal_reader: &Option<ThermalSource>,
    daily_max: DailyMaxTemps,
) -> IpcResponse {
    match cmd {
        DeviceCommand::ReadState => match device {
            None => IpcResponse::Error(DeviceError::NotConnected),
            Some(d) => match d.read_state() {
                Ok(state) => IpcResponse::State(state),
                Err(e) => {
                    warn!("read_state failed: {e}; dropping device");
                    *device = None;
                    IpcResponse::Error(e)
                }
            },
        },

        DeviceCommand::SetPwm { channel, pwm } => {
            // Manual PWM override: switch to Manual profile
            if !matches!(current_profile, Profile::Manual) {
                info!(
                    "Manual PWM override (ch{channel}={pwm}); switching from {} to manual",
                    current_profile
                );
                *current_profile = Profile::Manual;
            }
            match device {
                None => IpcResponse::Error(DeviceError::NotConnected),
                Some(d) => match d.write_pwm(channel, pwm) {
                    Ok(()) => IpcResponse::Ok,
                    Err(e) => {
                        warn!("write_pwm({channel}, {pwm}) failed: {e}");
                        if matches!(e, DeviceError::Comm(_) | DeviceError::Timeout) {
                            *device = None;
                        }
                        IpcResponse::Error(e)
                    }
                },
            }
        }

        DeviceCommand::NotifySuspend => {
            if let Some(ref mut d) = device {
                let _ = d.notify_suspend();
            }
            IpcResponse::Ok
        }

        DeviceCommand::NotifyResume => {
            if let Some(ref mut d) = device {
                let _ = d.notify_resume();
            }
            IpcResponse::Ok
        }

        DeviceCommand::SetProfile { profile } => {
            match Profile::from_str_loose(&profile) {
                Some(p) => {
                    // Don't allow non-manual profile if we have no thermal reader
                    if !matches!(p, Profile::Manual) && thermal_reader.is_none() {
                        // Record the desired profile so it can be restored
                        // when LHM reconnects.
                        *desired_profile = p;
                        return IpcResponse::Error(DeviceError::Comm(
                            "Cannot set automatic profile: thermal reader unavailable. \
                             Profile will activate when LHM reconnects."
                                .into(),
                        ));
                    }
                    info!("Profile changed: {} → {}", current_profile, p);
                    *current_profile = p;
                    *desired_profile = p;
                    IpcResponse::ProfileInfo {
                        profile: p.to_string(),
                        cpu_temp_c: last_reading.as_ref().and_then(|r| r.cpu_c),
                        gpu_temp_c: last_reading.as_ref().and_then(|r| r.gpu_c),
                        temp_c: last_reading.as_ref().map(|r| r.max_c),
                        cpu_max_today_c: daily_max.cpu_c,
                        gpu_max_today_c: daily_max.gpu_c,
                        max_today_c: daily_max.max_c,
                    }
                }
                None => IpcResponse::Error(DeviceError::Comm(format!(
                    "Unknown profile '{profile}'. Valid: quiet, balanced, performance, manual"
                ))),
            }
        }

        DeviceCommand::GetProfile => IpcResponse::ProfileInfo {
            profile: current_profile.to_string(),
            cpu_temp_c: last_reading.as_ref().and_then(|r| r.cpu_c),
            gpu_temp_c: last_reading.as_ref().and_then(|r| r.gpu_c),
            temp_c: last_reading.as_ref().map(|r| r.max_c),
            cpu_max_today_c: daily_max.cpu_c,
            gpu_max_today_c: daily_max.gpu_c,
            max_today_c: daily_max.max_c,
        },
    }
}

// ── Device discovery ────────────────────────────────────────────────────────

/// Try to open the Thelio Io 2 if it is connected.
fn try_open_device() -> Option<Box<dyn Device>> {
    match thelio_io::open() {
        Ok(Some(d)) => Some(Box::new(d)),
        Ok(None) => None,
        Err(e) => {
            warn!("Error probing Thelio Io 2: {e}");
            None
        }
    }
}

/// Return `(year, month, day)` from the local system clock.
/// Used to detect midnight rollover for daily-max temperature reset.
fn local_day_key() -> (u16, u16, u16) {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let st = unsafe { GetLocalTime() };
    (st.wYear, st.wMonth, st.wDay)
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: build an args vec from a string slice.
    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    // ── parse_log_level_arg ─────────────────────────────────────────────

    #[test]
    fn log_level_default_is_info() {
        assert_eq!(parse_log_level_arg(&args(&["daemon"])), log::Level::Info);
    }

    #[test]
    fn log_level_debug() {
        assert_eq!(
            parse_log_level_arg(&args(&["daemon", "--log-level", "debug"])),
            log::Level::Debug,
        );
    }

    #[test]
    fn log_level_case_insensitive() {
        assert_eq!(
            parse_log_level_arg(&args(&["daemon", "--log-level", "WARN"])),
            log::Level::Warn,
        );
    }

    #[test]
    fn log_level_error() {
        assert_eq!(
            parse_log_level_arg(&args(&["daemon", "--log-level", "error"])),
            log::Level::Error,
        );
    }

    #[test]
    fn log_level_trace() {
        assert_eq!(
            parse_log_level_arg(&args(&["daemon", "--log-level", "trace"])),
            log::Level::Trace,
        );
    }

    #[test]
    fn log_level_unknown_falls_back_to_info() {
        assert_eq!(
            parse_log_level_arg(&args(&["daemon", "--log-level", "verbose"])),
            log::Level::Info,
        );
    }

    #[test]
    fn log_level_missing_value_falls_back_to_info() {
        assert_eq!(
            parse_log_level_arg(&args(&["daemon", "--log-level"])),
            log::Level::Info,
        );
    }

    // ── parse_profile_arg ───────────────────────────────────────────────

    #[test]
    fn profile_default_is_balanced() {
        assert_eq!(parse_profile_arg(&args(&["daemon"])), Profile::Balanced);
    }

    #[test]
    fn profile_quiet() {
        assert_eq!(
            parse_profile_arg(&args(&["daemon", "--profile", "quiet"])),
            Profile::Quiet,
        );
    }

    #[test]
    fn profile_performance() {
        assert_eq!(
            parse_profile_arg(&args(&["daemon", "--profile", "performance"])),
            Profile::Performance,
        );
    }

    #[test]
    fn profile_manual() {
        assert_eq!(
            parse_profile_arg(&args(&["daemon", "--profile", "manual"])),
            Profile::Manual,
        );
    }

    #[test]
    fn profile_unknown_falls_back_to_balanced() {
        assert_eq!(
            parse_profile_arg(&args(&["daemon", "--profile", "turbo"])),
            Profile::Balanced,
        );
    }

    // ── parse_lhm_config ────────────────────────────────────────────────

    #[test]
    fn lhm_config_defaults() {
        let config = parse_lhm_config(&args(&["daemon"]));
        assert_eq!(config.url, "http://localhost:8085");
        assert!(config.username.is_none());
        assert!(config.password.is_none());
    }

    #[test]
    fn lhm_config_custom_url() {
        let config = parse_lhm_config(&args(&[
            "daemon",
            "--lhm-url",
            "http://192.168.1.10:9090",
        ]));
        assert_eq!(config.url, "http://192.168.1.10:9090");
    }

    #[test]
    fn lhm_config_with_auth() {
        let config = parse_lhm_config(&args(&[
            "daemon",
            "--lhm-user",
            "admin",
            "--lhm-password",
            "secret",
        ]));
        assert_eq!(config.username.as_deref(), Some("admin"));
        assert_eq!(config.password.as_deref(), Some("secret"));
    }

    #[test]
    fn lhm_config_all_options() {
        let config = parse_lhm_config(&args(&[
            "daemon",
            "--lhm-url",
            "http://myhost:8080",
            "--lhm-user",
            "user1",
            "--lhm-password",
            "pass1",
        ]));
        assert_eq!(config.url, "http://myhost:8080");
        assert_eq!(config.username.as_deref(), Some("user1"));
        assert_eq!(config.password.as_deref(), Some("pass1"));
    }
}
