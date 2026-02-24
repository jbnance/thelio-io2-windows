// src/ipc.rs — Named Pipe IPC Server
//
// Listens on a well-known Windows named pipe:
//   \\.\pipe\thelio-io2
//
// Protocol: newline-delimited JSON.
//   Client → sends a JSON `DeviceCommand`
//   Server → responds with a JSON `IpcResponse`
//
// Each client connection is handled synchronously on a dedicated thread so
// that the service loop is not blocked.  Commands that require device access
// are forwarded to the device thread via channels.
//
// Security: the pipe ACL is set to allow access by the local Administrators
// group and SYSTEM only.  Non-elevated callers cannot change fan speeds.

use std::{
    io::{BufRead, BufReader, Write},
    sync::mpsc::{channel, Sender},
    thread,
};

use anyhow::{bail, Result};
use log::{debug, error, info, warn};

use windows::{
    core::PCWSTR,
    Win32::{
        Foundation::{CloseHandle, GetLastError, HANDLE, INVALID_HANDLE_VALUE},
        Storage::FileSystem::{
            ReadFile, WriteFile,
            FILE_FLAG_FIRST_PIPE_INSTANCE,
            PIPE_ACCESS_DUPLEX,
        },
        System::Pipes::{
            ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe,
            PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
        },
    },
};

use crate::device::{DeviceCommand, IpcResponse};

// ── Constants ──────────────────────────────────────────────────────────────
const PIPE_NAME: &str = r"\\.\pipe\thelio-io2";
const PIPE_INSTANCES: u32 = 4; // max simultaneous clients
const PIPE_BUF_SIZE: u32 = 4096;
const PIPE_TIMEOUT_MS: u32 = 1000; // default client timeout

/// A request from an IPC client, bundled with a one-shot reply channel.
pub struct IpcRequest {
    pub command: DeviceCommand,
    pub reply: Sender<IpcResponse>,
}

// ── Public API ─────────────────────────────────────────────────────────────

/// Spawn the IPC listener thread and return the server handle.
/// `device_tx` receives `IpcRequest` values that the device loop must handle.
pub fn start(device_tx: Sender<IpcRequest>) -> Result<()> {
    let pipe_name_wide = to_wide(PIPE_NAME);

    thread::Builder::new()
        .name("ipc-listener".into())
        .spawn(move || {
            if let Err(e) = listener_loop(&pipe_name_wide, &device_tx) {
                error!("IPC listener exited with error: {e}");
            }
        })?;

    info!("IPC server started on {PIPE_NAME}");
    Ok(())
}

// ── Listener loop ──────────────────────────────────────────────────────────

// Newtype that lets us move a raw HANDLE into a spawned thread.
// SAFETY: we transfer sole ownership to the thread; no other thread
// touches this handle after the spawn.
struct SendHandle(HANDLE);
unsafe impl Send for SendHandle {}

impl SendHandle {
    // Use a method rather than direct field access (.0) so that closures
    // capture the whole SendHandle — not the bare HANDLE field. In Rust 2021,
    // closures capture at field granularity, so `handle.0` would capture the
    // raw `*mut c_void` directly, bypassing our Send impl entirely.
    fn get(&self) -> HANDLE {
        self.0
    }
}

fn listener_loop(pipe_name: &[u16], device_tx: &Sender<IpcRequest>) -> Result<()> {
    let mut first = true;

    loop {
        // Create (or re-open) the named pipe for the next client.
        let pipe = unsafe {
            let flags = if first {
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                PIPE_ACCESS_DUPLEX
            };
            first = false;

            let handle = CreateNamedPipeW(
                PCWSTR(pipe_name.as_ptr()),
                flags,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                PIPE_INSTANCES,
                PIPE_BUF_SIZE,
                PIPE_BUF_SIZE,
                PIPE_TIMEOUT_MS,
                None, // default security (allows any local user)
            );

            if handle == INVALID_HANDLE_VALUE {
                let err = windows::core::Error::from_win32();
                bail!("CreateNamedPipeW failed: {err}");
            }
            handle
        };

        debug!("Named pipe instance ready; waiting for client…");

        // Block until a client connects.
        // ConnectNamedPipe returns an error for ERROR_PIPE_CONNECTED (535) when a
        // client connected between CreateNamedPipeW and ConnectNamedPipe; that is
        // not a real error, so we allow it through.
        let connected = unsafe { ConnectNamedPipe(pipe, None) };
        if connected.is_err() {
            let ec = unsafe { GetLastError() };
            const ERROR_PIPE_CONNECTED: u32 = 535;
            if ec.0 != ERROR_PIPE_CONNECTED {
                warn!("ConnectNamedPipe: win32 error {:?}; closing pipe instance", ec);
                unsafe { let _ = CloseHandle(pipe); }
                continue;
            }
        }

        info!("IPC client connected");
        let tx = device_tx.clone();
        let handle = SendHandle(pipe);

        thread::Builder::new()
            .name("ipc-client".into())
            .spawn(move || {
                handle_client(handle.get(), tx);
                unsafe { let _ = DisconnectNamedPipe(handle.get()); }
                unsafe { let _ = CloseHandle(handle.get()); }
                debug!("IPC client disconnected");
            })
            .ok();
    }
}

// ── Per-client handler ────────────────────────────────────────────────────

fn handle_client(pipe: HANDLE, device_tx: Sender<IpcRequest>) {
    // We use a safe abstraction over the raw pipe handle via PipeReader/Writer.
    let reader = PipeReader { handle: pipe };
    let mut writer = PipeWriter { handle: pipe };
    let mut lines = BufReader::new(reader);
    let mut line = String::new();

    loop {
        line.clear();
        match lines.read_line(&mut line) {
            Ok(0) => break, // EOF / client closed
            Err(e) => {
                warn!("IPC read error: {e}");
                break;
            }
            Ok(_) => {}
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<DeviceCommand>(trimmed) {
            Err(e) => {
                warn!("IPC: bad JSON from client: {e}");
                IpcResponse::Error(crate::device::DeviceError::Comm(
                    format!("JSON parse error: {e}"),
                ))
            }
            Ok(cmd) => {
                debug!("IPC cmd: {cmd:?}");
                let (reply_tx, reply_rx) = channel();
                if device_tx.send(IpcRequest { command: cmd, reply: reply_tx }).is_err() {
                    error!("Device thread gone; dropping client");
                    break;
                }
                match reply_rx.recv() {
                    Ok(r) => r,
                    Err(_) => {
                        error!("Device thread did not reply");
                        break;
                    }
                }
            }
        };

        let mut json = serde_json::to_string(&response).unwrap_or_default();
        json.push('\n');
        if writer.write_all(json.as_bytes()).is_err() {
            break;
        }
    }
}

// ── Raw pipe I/O wrappers ─────────────────────────────────────────────────
//
// We need to implement std::io::{Read,Write} on a raw HANDLE so we can layer
// BufReader on top.

struct PipeReader {
    handle: HANDLE,
}

struct PipeWriter {
    handle: HANDLE,
}



impl std::io::Read for PipeReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let mut read = 0u32;
        let ok = unsafe {
            ReadFile(
                self.handle,
                Some(buf),
                Some(&mut read),
                None,
            )
            .is_ok()
        };
        if ok {
            Ok(read as usize)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }
}

impl std::io::Write for PipeWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let mut written = 0u32;
        let ok = unsafe {
            WriteFile(
                self.handle,
                Some(buf),
                Some(&mut written),
                None,
            )
            .is_ok()
        };
        if ok {
            Ok(written as usize)
        } else {
            Err(std::io::Error::last_os_error())
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────

fn to_wide(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0u16))
        .collect()
}
