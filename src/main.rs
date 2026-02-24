// src/main.rs — Thelio Io 2 Windows Service
//
// This is the top-level entry point.  The binary can be run in two modes:
//
//   1. As a Windows service (normal deployment):
//        sc create thelio-io2 binPath= "C:\path\to\thelio-io2-daemon.exe"
//        sc start thelio-io2
//
//   2. As a foreground console process (development / debugging):
//        thelio-io2-daemon.exe --console
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
//   └────────────────┬─────────────────────────┘
//                    │
//         ┌──────────┴──────────┐
//         │                     │
//   ┌─────▼──────┐    ┌────────▼────────┐
//   │ IPC Server │    │  Power Monitor  │
//   │ (thread)   │    │  (callback)     │
//   └────────────┘    └─────────────────┘

#![windows_subsystem = "windows"]

mod device;
mod ipc;
mod power;
mod thelio_io;

use std::{
    env,
    sync::mpsc::{channel, Receiver, TryRecvError},
    thread,
    time::{Duration, Instant},
};

use anyhow::Result;
use log::{error, info, warn};
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
    ipc::IpcRequest,
    power::PowerEvent,
};

// ── Service name ───────────────────────────────────────────────────────────
const SERVICE_NAME: &str = "thelio-io2";
const SERVICE_TYPE: ServiceType = ServiceType::OWN_PROCESS;

// How often to attempt device reconnection when not connected.
const RECONNECT_INTERVAL: Duration = Duration::from_secs(5);

// ── Entry point ───────────────────────────────────────────────────────────

fn main() -> Result<()> {
    // Use simple console logging.  In production a Windows Event Log sink
    // would be preferred; this is straightforward to add later.
    simple_logger::init_with_level(log::Level::Info)
        .unwrap_or_default();

    let args: Vec<String> = env::args().collect();
    if args.iter().any(|a| a == "--console") {
        info!("Running in console mode (--console)");
        return run_console();
    }

    // Start the Windows service dispatcher.  This call blocks until the
    // SCM dispatches to our service_main.
    service_dispatcher::start(SERVICE_NAME, ffi_service_main)
        .map_err(|e| anyhow::anyhow!("service_dispatcher::start failed: {e}"))?;

    Ok(())
}

// ── Console (debug) mode ───────────────────────────────────────────────────

fn run_console() -> Result<()> {
    info!("=== System76 Io Daemon (console mode) ===");
    info!("Note: power suspend/resume events are not monitored in console mode.");

    let (_power_tx, power_rx) = channel::<PowerEvent>();
    let (device_tx, device_rx) = channel::<IpcRequest>();

    // Start the IPC server.
    ipc::start(device_tx)?;

    // Run the device loop on the main thread.
    device_loop(device_rx, power_rx, None);

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

    // Channels for cross-thread communication.
    let (power_tx, power_rx) = channel::<PowerEvent>();
    let (device_tx, device_rx) = channel::<IpcRequest>();
    let (stop_tx, stop_rx) = channel::<()>();

    // Register our service control handler with the SCM.
    // Power events (suspend/resume) are delivered here as ServiceControl::PowerEvent,
    // so no separate Win32 power registration is needed.
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
    device_loop(device_rx, power_rx, Some(stop_rx));

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
// handle and processes IPC requests and power events in a single loop,
// with automatic reconnection on failure.

fn device_loop(
    ipc_rx: Receiver<IpcRequest>,
    power_rx: Receiver<PowerEvent>,
    stop_rx: Option<Receiver<()>>,
) {
    let mut device: Option<Box<dyn Device>> = None;
    let mut last_reconnect = Instant::now()
        - RECONNECT_INTERVAL; // try immediately on first iteration

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
                }
                None => {
                    // No device present; we'll retry later.
                }
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
                }
                Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => break,
            }
        }

        // ── IPC requests ───────────────────────────────────────────────────
        // Process up to ~10 requests per cycle to avoid starving other work.
        for _ in 0..10 {
            match ipc_rx.try_recv() {
                Ok(req) => {
                    let response = handle_request(&mut device, req.command);
                    let _ = req.reply.send(response);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    info!("IPC channel disconnected; exiting");
                    return;
                }
            }
        }

        // Brief sleep to avoid spinning 100% CPU.
        thread::sleep(Duration::from_millis(20));
    }
}

// ── Request dispatch ───────────────────────────────────────────────────────

fn handle_request(device: &mut Option<Box<dyn Device>>, cmd: DeviceCommand) -> IpcResponse {
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

        DeviceCommand::SetPwm { channel, pwm } => match device {
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
        },

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
