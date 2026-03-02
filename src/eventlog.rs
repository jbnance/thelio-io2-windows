// src/eventlog.rs — Windows Event Log logger for service mode
//
// When the daemon runs as a Windows service there is no console, so
// `simple_logger` output is lost.  This module provides a `log::Log`
// implementation that writes to the **Windows Event Log**, making
// messages visible in Event Viewer under:
//
//   Windows Logs > Application > Source: thelio-io2

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;

use log::{Level, LevelFilter, Log, Metadata, Record};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::EventLog::{
    DeregisterEventSource, RegisterEventSourceW, ReportEventW, EVENTLOG_ERROR_TYPE,
    EVENTLOG_INFORMATION_TYPE, EVENTLOG_WARNING_TYPE,
};
use windows::core::PCWSTR;

/// Event source name — matches the Windows service name.
const SOURCE_NAME: &str = "thelio-io2";

/// A logger that writes to the Windows Event Log via `ReportEventW`.
struct EventLogger {
    handle: HANDLE,
}

// SAFETY: The HANDLE from RegisterEventSourceW is thread-safe for
// ReportEventW calls — the Event Log API is documented as thread-safe.
unsafe impl Send for EventLogger {}
unsafe impl Sync for EventLogger {}

impl Log for EventLogger {
    fn enabled(&self, _metadata: &Metadata) -> bool {
        // Filtering is handled by `log::set_max_level`; accept everything here.
        true
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let event_type = match record.level() {
            Level::Error => EVENTLOG_ERROR_TYPE,
            Level::Warn => EVENTLOG_WARNING_TYPE,
            Level::Info | Level::Debug | Level::Trace => EVENTLOG_INFORMATION_TYPE,
        };

        // Format: "[module::path] message"  (similar to simple_logger output).
        let message = match record.module_path() {
            Some(module) => format!("[{}] {}", module, record.args()),
            None => format!("{}", record.args()),
        };

        // Convert to a null-terminated wide string for the Windows API.
        let wide: Vec<u16> = OsStr::new(&message)
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let string_ptr = PCWSTR(wide.as_ptr());

        // ReportEventW expects a slice of PCWSTR for the "strings" parameter.
        let strings = [string_ptr];

        unsafe {
            let _ = ReportEventW(
                self.handle,
                event_type,
                0,     // category
                0,     // event ID
                None,  // user SID
                0,     // raw data size (no binary data attached)
                Some(&strings),
                None,  // raw data pointer
            );
        }
    }

    fn flush(&self) {
        // Event log writes are immediate; nothing to flush.
    }
}

impl Drop for EventLogger {
    fn drop(&mut self) {
        if !self.handle.is_invalid() {
            unsafe {
                let _ = DeregisterEventSource(self.handle);
            }
        }
    }
}

/// Initialise the Windows Event Log logger as the global `log` backend.
///
/// This is the service-mode counterpart of `simple_logger::init_with_level`.
pub fn init(level: Level) {
    let wide_source: Vec<u16> = OsStr::new(SOURCE_NAME)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let handle = unsafe { RegisterEventSourceW(None, PCWSTR(wide_source.as_ptr())) };

    match handle {
        Ok(h) => {
            let logger = EventLogger { handle: h };
            if log::set_boxed_logger(Box::new(logger)).is_ok() {
                log::set_max_level(level.to_level_filter());
            }
        }
        Err(e) => {
            // Fall back to simple_logger if we cannot open the event source.
            // Log a warning to stderr so the failure is not completely silent.
            eprintln!(
                "Warning: could not open Windows Event Log source '{}': {e}; \
                 falling back to simple_logger",
                SOURCE_NAME
            );
            simple_logger::init_with_level(level).unwrap_or_default();
        }
    }
}

/// Convert a `log::Level` to the corresponding `log::LevelFilter`.
trait ToLevelFilter {
    fn to_level_filter(self) -> LevelFilter;
}

impl ToLevelFilter for Level {
    fn to_level_filter(self) -> LevelFilter {
        match self {
            Level::Error => LevelFilter::Error,
            Level::Warn => LevelFilter::Warn,
            Level::Info => LevelFilter::Info,
            Level::Debug => LevelFilter::Debug,
            Level::Trace => LevelFilter::Trace,
        }
    }
}
