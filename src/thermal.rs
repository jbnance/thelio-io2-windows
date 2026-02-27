// src/thermal.rs — CPU Temperature Reading via WMI
//
// Uses Windows Management Instrumentation to query thermal zone temperatures.
// The MSAcpi_ThermalZoneTemperature class (in the root\WMI namespace)
// returns temperatures in tenths of Kelvin.  We convert to Celsius and
// return the highest reading across all thermal zones.
//
// This requires COM initialization on the calling thread.  The `wmi` crate
// handles CoInitialize internally.

use log::{debug, warn};
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

/// Reads CPU/system temperature from WMI.
///
/// Each instance holds a COM library handle and a WMI connection to the
/// `root\WMI` namespace.  Create one per thread (COM is per-thread).
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
    #[error("no thermal zones found")]
    NoZones,
}

impl ThermalReader {
    /// Create a new thermal reader.  Initializes COM and connects to WMI.
    pub fn new() -> Result<Self, ThermalError> {
        let com = COMLibrary::new()?;
        // Connect to root\WMI (not root\CIMV2) for ACPI thermal data.
        let conn = WMIConnection::with_namespace_path("root\\WMI", com)?;
        Ok(Self { com, conn })
    }

    /// Read the current CPU temperature in degrees Celsius.
    ///
    /// Queries all MSAcpi_ThermalZoneTemperature instances and returns
    /// the highest temperature found.  On desktops this is typically the
    /// CPU package temperature reported by ACPI.
    pub fn read_cpu_temp(&self) -> Result<f64, ThermalError> {
        let zones: Vec<ThermalZone> = self.conn.query()?;

        if zones.is_empty() {
            return Err(ThermalError::NoZones);
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
            .fold(f64::NEG_INFINITY, f64::max);

        Ok(max_temp)
    }
}

/// Try to create a ThermalReader; if it fails, log a warning and return None.
/// This is used at daemon startup so that WMI failures don't prevent the
/// service from running (it will just not have automatic fan control).
pub fn try_init() -> Option<ThermalReader> {
    match ThermalReader::new() {
        Ok(reader) => {
            // Do a test read to make sure it actually works.
            match reader.read_cpu_temp() {
                Ok(temp) => {
                    log::info!("Thermal reader initialized; current CPU temp: {:.1}°C", temp);
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
