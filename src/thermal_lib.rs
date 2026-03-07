// src/thermal_lib.rs — Temperature reading via LibreHardwareMonitorLib helper process
//
// This module provides a `LibThermalReader` that spawns the `lhm-helper.exe`
// sidecar process (a .NET app wrapping LibreHardwareMonitorLib) and reads
// temperature sensor data via a JSON line protocol over stdin/stdout.
//
// This eliminates the need for the full LibreHardwareMonitor GUI application
// and its HTTP web server — the library accesses hardware sensors directly.
//
// Protocol:
//   Startup  → helper writes: {"status":"ready"}
//   Read     ← daemon writes: "read\n" to helper stdin
//   Response → helper writes: {"sensors":[...]} as a JSON line
//   Shutdown ← daemon writes: "exit\n" or closes stdin

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use log::{debug, info, warn};
use serde::Deserialize;

use crate::thermal::{is_sane_celsius, fold_max, ThermalError, ThermalReading};

// ── Helper JSON protocol types ────────────────────────────────────────────

/// A temperature sensor reading from the lhm-helper sidecar.
#[derive(Debug, Deserialize)]
struct HelperSensor {
    /// Sensor identifier, e.g. `/amdcpu/0/temperature/0`.
    id: String,
    /// Human-readable name, e.g. `"CPU Package"`.
    name: String,
    /// Temperature in °C.
    value: f64,
    /// Hardware classification: `"cpu"`, `"gpu"`, or `"other"`.
    hardware: String,
}

/// Response envelope from the helper when a read is requested.
#[derive(Debug, Deserialize)]
struct HelperSensorResponse {
    sensors: Vec<HelperSensor>,
}

/// Status/error message from the helper.
#[derive(Debug, Deserialize)]
struct HelperStatus {
    status: String,
    error: Option<String>,
}

/// Union type for parsing helper output — either a sensor response or a status message.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum HelperMessage {
    Sensors(HelperSensorResponse),
    Status(HelperStatus),
}

// ── LibThermalReader ──────────────────────────────────────────────────────

/// Reads CPU and GPU temperatures via the `lhm-helper` sidecar process.
///
/// The helper wraps LibreHardwareMonitorLib and communicates via stdin/stdout
/// using a simple line-based JSON protocol.
pub struct LibThermalReader {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl LibThermalReader {
    /// Spawn the lhm-helper process and wait for it to signal readiness.
    pub fn new(helper_path: &Path) -> Result<Self, ThermalError> {
        info!("Spawning lhm-helper: {}", helper_path.display());

        let mut child = Command::new(helper_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| ThermalError::Http(format!(
                "Failed to spawn lhm-helper at {}: {e}", helper_path.display()
            )))?;

        let stdin = child.stdin.take()
            .ok_or_else(|| ThermalError::Http("Failed to open lhm-helper stdin".into()))?;
        let stdout = child.stdout.take()
            .ok_or_else(|| ThermalError::Http("Failed to open lhm-helper stdout".into()))?;

        let mut reader = Self {
            child,
            stdin,
            stdout: BufReader::new(stdout),
        };

        // Wait for the "ready" signal from the helper.
        reader.wait_for_ready()?;

        Ok(reader)
    }

    /// Read and parse the first line from the helper, expecting `{"status":"ready"}`.
    fn wait_for_ready(&mut self) -> Result<(), ThermalError> {
        let line = self.read_line()?;

        match serde_json::from_str::<HelperStatus>(&line) {
            Ok(status) if status.status == "ready" => {
                info!("lhm-helper is ready");
                Ok(())
            }
            Ok(status) if status.status == "error" => {
                let msg = status.error.unwrap_or_else(|| "unknown error".into());
                Err(ThermalError::Http(format!("lhm-helper initialization failed: {msg}")))
            }
            _ => Err(ThermalError::Http(format!(
                "Unexpected lhm-helper startup message: {line}"
            ))),
        }
    }

    /// Read a single line from the helper's stdout.
    fn read_line(&mut self) -> Result<String, ThermalError> {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .map_err(|e| ThermalError::Http(format!("Failed to read from lhm-helper: {e}")))?;

        if line.is_empty() {
            return Err(ThermalError::Http(
                "lhm-helper process exited unexpectedly".into(),
            ));
        }

        Ok(line)
    }

    /// Send a "read" command and parse the sensor response.
    fn request_sensors(&mut self) -> Result<Vec<HelperSensor>, ThermalError> {
        // Send the read command.
        self.stdin
            .write_all(b"read\n")
            .map_err(|e| ThermalError::Http(format!("Failed to write to lhm-helper: {e}")))?;
        self.stdin
            .flush()
            .map_err(|e| ThermalError::Http(format!("Failed to flush lhm-helper stdin: {e}")))?;

        // Read the response line.
        let line = self.read_line()?;

        // Parse — could be a sensor response or an error status.
        match serde_json::from_str::<HelperMessage>(&line) {
            Ok(HelperMessage::Sensors(resp)) => Ok(resp.sensors),
            Ok(HelperMessage::Status(status)) if status.status == "error" => {
                let msg = status.error.unwrap_or_else(|| "unknown error".into());
                Err(ThermalError::Http(format!("lhm-helper read error: {msg}")))
            }
            Ok(HelperMessage::Status(_)) => Err(ThermalError::Http(
                "Unexpected status message from lhm-helper during read".into(),
            )),
            Err(e) => Err(ThermalError::Json(e)),
        }
    }

    /// Read temperatures from the lhm-helper and return a `ThermalReading`.
    ///
    /// The helper provides pre-classified sensors (cpu/gpu), so we don't need
    /// to parse sensor IDs like the HTTP backend does.
    pub fn read_temps(&mut self) -> Result<ThermalReading, ThermalError> {
        let sensors = self.request_sensors()?;

        // Separate CPU and GPU sensors, filtering for sane values.
        let cpu_temps = sensors
            .iter()
            .filter(|s| s.hardware == "cpu" && is_sane_celsius(s.value))
            .map(|s| {
                debug!("LHM lib CPU sensor: {} ({}) = {:.1}°C", s.name, s.id, s.value);
                s.value
            });

        let gpu_temps = sensors
            .iter()
            .filter(|s| s.hardware == "gpu" && is_sane_celsius(s.value))
            .map(|s| {
                debug!("LHM lib GPU sensor: {} ({}) = {:.1}°C", s.name, s.id, s.value);
                s.value
            });

        let cpu = fold_max(cpu_temps);
        let gpu = fold_max(gpu_temps);

        let max_c = match (cpu, gpu) {
            (Some(c), Some(g)) => c.max(g),
            (Some(c), None) => c,
            (None, Some(g)) => g,
            (None, None) => return Err(ThermalError::NoSources),
        };

        let reading = ThermalReading {
            cpu_c: cpu,
            gpu_c: gpu,
            max_c,
        };

        debug!("Thermal (lib): {}", reading.summary());
        Ok(reading)
    }

    /// Send "exit" to the helper and wait for it to shut down gracefully.
    fn shutdown(&mut self) {
        let _ = self.stdin.write_all(b"exit\n");
        let _ = self.stdin.flush();

        // Give the helper a moment to clean up.
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            _ => {
                std::thread::sleep(Duration::from_millis(500));
                if self.child.try_wait().ok().flatten().is_none() {
                    warn!("lhm-helper did not exit gracefully, killing");
                    let _ = self.child.kill();
                }
            }
        }
    }
}

impl Drop for LibThermalReader {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Try to create a LibThermalReader and perform a test read.
///
/// Returns `None` (with warnings logged) if the helper cannot be started
/// or returns no temperature data.
pub fn try_init_lib(helper_path: &Path) -> Option<LibThermalReader> {
    info!("Initializing temperature reader via LibreHardwareMonitorLib");

    let mut reader = match LibThermalReader::new(helper_path) {
        Ok(r) => r,
        Err(e) => {
            warn!("Failed to start lhm-helper: {e}");
            warn!("Ensure lhm-helper.exe is present and the daemon is running as administrator");
            warn!("Automatic fan control will be unavailable");
            return None;
        }
    };

    // Perform a test read.
    match reader.read_temps() {
        Ok(reading) => {
            info!("Thermal reader (library) initialized: {}", reading.summary());
            Some(reader)
        }
        Err(e) => {
            warn!("Thermal reader (library) test read failed: {e}");
            warn!("Automatic fan control will be unavailable");
            None
        }
    }
}
