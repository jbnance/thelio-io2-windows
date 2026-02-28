// src/thermal.rs — CPU / GPU Temperature Reading
//
// CPU temperature: queried via WMI (MSAcpi_ThermalZoneTemperature in root\WMI).
//   Returns temperatures in tenths of Kelvin; we convert to Celsius and return
//   the highest reading across all thermal zones.
//
// GPU temperature: queried via nvidia-smi (optional).  If nvidia-smi is not
//   present or no NVIDIA GPU is found, GPU temperature is silently skipped.
//
// The overall temperature reported is the maximum of CPU and GPU readings,
// since the Thelio chassis fans cool the entire system.
//
// WMI requires COM initialization on the calling thread; the `wmi` crate
// handles CoInitialize internally.

use std::process::Command;

use log::{debug, info, warn};
use serde::Deserialize;
use wmi::{COMLibrary, WMIConnection, WMIError};

// ── WMI query struct ─────────────────────────────────────────────────────

/// Maps to the WMI class MSAcpi_ThermalZoneTemperature.
/// The `CurrentTemperature` field is in tenths of a degree Kelvin.
#[derive(Deserialize, Debug)]
#[serde(rename = "MSAcpi_ThermalZoneTemperature")]
#[serde(rename_all = "PascalCase")]
struct ThermalZone {
    current_temperature: u32,
}

// ── Public API ───────────────────────────────────────────────────────────

/// Reads CPU and GPU temperatures.
///
/// Holds a COM library handle and a WMI connection to the `root\WMI`
/// namespace for CPU thermal queries.  GPU queries are done via nvidia-smi
/// subprocess calls.  Create one per thread (COM is per-thread).
pub struct ThermalReader {
    #[allow(dead_code)]
    com: COMLibrary,
    conn: WMIConnection,
}

/// Error type for thermal reading failures.
#[derive(Debug, thiserror::Error)]
pub enum ThermalError {
    #[error("WMI error: {0}")]
    Wmi(#[from] WMIError),
    #[error("no temperature sources available")]
    NoSources,
}

impl ThermalReader {
    /// Create a new thermal reader.  Initializes COM and connects to WMI.
    pub fn new() -> Result<Self, ThermalError> {
        let com = COMLibrary::new()?;
        // Connect to root\WMI (not root\CIMV2) for ACPI thermal data.
        let conn = WMIConnection::with_namespace_path("root\\WMI", com)?;
        Ok(Self { com, conn })
    }

    /// Read the current CPU temperature in degrees Celsius via WMI.
    ///
    /// Queries all MSAcpi_ThermalZoneTemperature instances and returns
    /// the highest temperature found.  On desktops this is typically the
    /// CPU package temperature reported by ACPI.
    fn read_cpu_temp(&self) -> Option<f64> {
        let zones: Vec<ThermalZone> = match self.conn.query() {
            Ok(z) => z,
            Err(e) => {
                warn!("WMI thermal query failed: {e}");
                return None;
            }
        };

        if zones.is_empty() {
            return None;
        }

        let max_temp = zones
            .iter()
            .map(|z| {
                // Convert tenths-of-Kelvin to Celsius
                let celsius = (z.current_temperature as f64 / 10.0) - 273.15;
                debug!(
                    "Thermal zone: {} tenths-K = {:.1}°C",
                    z.current_temperature, celsius
                );
                celsius
            })
            .filter(|&c| c > 0.0 && c < 150.0) // sanity check
            .fold(None, |acc: Option<f64>, c| {
                Some(acc.map_or(c, |a| a.max(c)))
            });

        max_temp
    }

    /// Read GPU temperature via nvidia-smi.
    ///
    /// Returns the highest GPU temperature in °C, or `None` if nvidia-smi
    /// is not installed or no NVIDIA GPU is present.  This is a lightweight
    /// subprocess call; nvidia-smi typically completes in <100ms.
    fn read_gpu_temp(&self) -> Option<f64> {
        let output = Command::new("nvidia-smi")
            .args(["--query-gpu=temperature.gpu", "--format=csv,noheader,nounits"])
            .output()
            .ok()?; // nvidia-smi not found → silently return None

        if !output.status.success() {
            return None;
        }

        let stdout = String::from_utf8_lossy(&output.stdout);
        let max = stdout
            .lines()
            .filter_map(|l| l.trim().parse::<f64>().ok())
            .filter(|&v| v > 0.0 && v < 150.0)
            .fold(None, |acc: Option<f64>, v| {
                Some(acc.map_or(v, |a| a.max(v)))
            });

        if let Some(t) = max {
            debug!("GPU temperature: {:.1}°C", t);
        }

        max
    }

    /// Read the maximum temperature across all sources (CPU + GPU) in °C.
    ///
    /// Returns the highest of CPU and GPU temperatures.  If one source is
    /// unavailable, the other is used.  Returns an error only if *no*
    /// source provides a reading.
    pub fn read_max_temp(&self) -> Result<f64, ThermalError> {
        let cpu = self.read_cpu_temp();
        let gpu = self.read_gpu_temp();

        match (cpu, gpu) {
            (Some(c), Some(g)) => {
                debug!("CPU: {c:.1}°C, GPU: {g:.1}°C → max {:.1}°C", c.max(g));
                Ok(c.max(g))
            }
            (Some(c), None) => Ok(c),
            (None, Some(g)) => Ok(g),
            (None, None) => Err(ThermalError::NoSources),
        }
    }
}

/// Try to create a ThermalReader; if it fails, log a warning and return None.
/// This is used at daemon startup so that WMI failures don't prevent the
/// service from running (it will just not have automatic fan control).
pub fn try_init() -> Option<ThermalReader> {
    match ThermalReader::new() {
        Ok(reader) => {
            // Do a test read to make sure it actually works.
            match reader.read_max_temp() {
                Ok(temp) => {
                    info!("Thermal reader initialized; current temp: {:.1}°C", temp);
                    Some(reader)
                }
                Err(e) => {
                    warn!("Thermal reader created but test read failed: {e}");
                    warn!("Automatic fan control will be unavailable");
                    None
                }
            }
        }
        Err(e) => {
            warn!("Failed to initialize WMI thermal reader: {e}");
            warn!("Automatic fan control will be unavailable");
            None
        }
    }
}
