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
//   ┌─────▼──────┐ ┌▼────────┐ ┌▼──────────────┐
//   │ IPC Server │ │ Power   │ │ Thermal Reader │
//   │ (thread)   │ │ Monitor │ │ (WMI)          │
//   └────────────┘ └─────────┘ └────────────────┘

#![windows_subsystem = "windows"]

mod device;
mod fan_curve;
mod ipc;
mod power;
mod thermal;
mod thelio_io;

use std::{
    env,
    sync::mpsc::{channel, Receiver, TryRecvError},
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
    thermal::{ThermalReader, ThermalReading},
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

// ── Shared state for profile (accessible from SCM handler thread) ────────
// The SCM handler runs on a different thread, so we use a static Mutex for
// the initial profile setting.  The actual mutable profile state lives in
// the device loop.
static INITIAL_PROFILE: Mutex<Profile> = Mutex::new(Profile::Balanced);

// ── Entry point ───────────────────────────────────────────────────────────

fn main() -> Result<()> {
    let args: Vec<String> = env::args().collect();

    // Parse --log-level before initializing the logger.
    let log_level = parse_log_level_arg(&args);
    simple_logger::init_with_level(log_level).unwrap_or_default();

    // Parse --profile argument (applies to both console and service mode).
    let profile = parse_profile_arg(&args);
    *INITIAL_PROFILE.lock() = profile;
    info!("Initial profile: {profile}");

    if args.iter().any(|a| a == "--console") {
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

// ── Console (debug) mode ───────────────────────────────────────────────────

fn run_console(profile: Profile) -> Result<()> {
    info!("=== System76 Io Daemon (console mode) ===");
    info!("Profile: {profile}");
    info!("Note: power suspend/resume events are not monitored in console mode.");

    let (_power_tx, power_rx) = channel::<PowerEvent>();
    let (device_tx, device_rx) = channel::<IpcRequest>();

    // Start the IPC server.
    ipc::start(device_tx)?;

    // Run the device loop on the main thread.
    device_loop(device_rx, power_rx, None, profile);

    Ok(())
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
    let thermal_reader = thermal::try_init();
    let mut hysteresis = TempHysteresis::new(2.0);
    let mut last_temp_poll = Instant::now() - THERMAL_POLL_INTERVAL;
    let mut last_reading: Option<ThermalReading> = None;
    let mut last_pwm: Option<u8> = None;
    let mut last_status_log = Instant::now() - STATUS_LOG_INTERVAL;

    if thermal_reader.is_none() && !matches!(current_profile, Profile::Manual) {
        warn!(
            "No thermal reader available; switching profile to manual. \
             Fan speeds must be set via the client."
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
            match try_open_device() {
                Some(d) => {
                    info!("Connected to: {}", d.name());
                    device = Some(d);
                    // Immediately apply current fan profile on reconnect
                    last_temp_poll = Instant::now() - THERMAL_POLL_INTERVAL;
                }
                None => {}
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
        for _ in 0..10 {
            match ipc_rx.try_recv() {
                Ok(req) => {
                    let response = handle_request(
                        &mut device,
                        req.command,
                        &mut current_profile,
                        &last_reading,
                        &thermal_reader,
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

        // ── Thermal polling + automatic fan control ────────────────────────
        if last_temp_poll.elapsed() >= THERMAL_POLL_INTERVAL {
            last_temp_poll = Instant::now();

            if let Some(curve) = current_profile.curve() {
                if let Some(ref reader) = thermal_reader {
                    match reader.read_temps() {
                        Ok(reading) => {
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
                            warn!("Temperature read failed: {e}");
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

        // Brief sleep to avoid spinning 100% CPU.
        thread::sleep(Duration::from_millis(20));
    }
}

// ── Request dispatch ───────────────────────────────────────────────────────

fn handle_request(
    device: &mut Option<Box<dyn Device>>,
    cmd: DeviceCommand,
    current_profile: &mut Profile,
    last_reading: &Option<ThermalReading>,
    thermal_reader: &Option<ThermalReader>,
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
                        return IpcResponse::Error(DeviceError::Comm(
                            "Cannot set automatic profile: thermal reader unavailable".into(),
                        ));
                    }
                    info!("Profile changed: {} → {}", current_profile, p);
                    *current_profile = p;
                    IpcResponse::ProfileInfo {
                        profile: p.to_string(),
                        cpu_temp_c: last_reading.as_ref().and_then(|r| r.cpu_c),
                        gpu_temp_c: last_reading.as_ref().and_then(|r| r.gpu_c),
                        temp_c: last_reading.as_ref().map(|r| r.max_c),
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
