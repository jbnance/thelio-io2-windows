// src/thermal.rs — CPU / GPU Temperature Reading (multi-source, multi-vendor)
//
// CPU temperature sources (tried in order until one succeeds):
//
//   1. WMI MSAcpi_ThermalZoneTemperature (root\WMI)
//      Returns temps in tenths of Kelvin; we convert to Celsius.
//      Works on many Intel systems but often unavailable on AMD.
//
//   2. WMI Win32_PerfFormattedData_Counters_ThermalZoneInformation (root\CIMV2)
//      Returns temps in Kelvin (whole degrees).
//      Available on Windows 10 1903+ for both Intel and AMD.
//
//   3. LibreHardwareMonitor WMI (root\LibreHardwareMonitor)
//      If LibreHardwareMonitor is running, exposes a Sensor WMI class
//      with detailed per-core temps for any CPU/GPU vendor.
//
//   4. OpenHardwareMonitor WMI (root\OpenHardwareMonitor)
//      Same schema as above but for the older OHM software.
//
// GPU temperature sources (all checked, results combined):
//
//   1. nvidia-smi CLI for NVIDIA GPUs (returns per-GPU temp).
//   2. LibreHardwareMonitor / OpenHardwareMonitor WMI for any GPU vendor
//      (AMD, Intel Arc, NVIDIA).  If nvidia-smi already reported a temp,
//      duplicates are harmless because we take the max.
//
// The overall temperature used for fan control is the maximum of CPU and all
// GPU readings, since the Thelio chassis fans cool the entire system.  Both
// individual readings are preserved for logging and status display.
//
// WMI requires COM initialization on the calling thread; the `wmi` crate
// handles CoInitialize internally via `COMLibrary::new()`.

use std::process::Command;

use log::{debug, info, warn};
use serde::{Deserialize, Serialize};
use wmi::{COMLibrary, WMIConnection, WMIError};

// ── WMI query structs ─────────────────────────────────────────────────────

/// ACPI thermal zones (root\WMI).
/// `CurrentTemperature` is in tenths of a degree Kelvin.
#[derive(Deserialize, Debug)]
#[serde(rename = "MSAcpi_ThermalZoneTemperature")]
#[serde(rename_all = "PascalCase")]
struct AcpiThermalZone {
    current_temperature: u32,
}

/// Performance-counter thermal zones (root\CIMV2).
/// `Temperature` is in degrees Kelvin (whole number).
/// Available on Windows 10 1903+ for Intel and AMD.
#[derive(Deserialize, Debug)]
#[serde(rename = "Win32_PerfFormattedData_Counters_ThermalZoneInformation")]
#[serde(rename_all = "PascalCase")]
struct PerfCounterThermalZone {
    temperature: u32,
}

/// Hardware-monitor sensor (LibreHardwareMonitor / OpenHardwareMonitor).
/// Both tools expose a `Sensor` WMI class with the same schema.
///
/// Identifiers follow a path format:
///   CPU:  /intelcpu/0/temperature/0, /amdcpu/0/temperature/0
///   GPU:  /gpu-nvidia/0/temperature/0, /gpu-amd/0/temperature/0
#[derive(Deserialize, Debug)]
#[serde(rename = "Sensor")]
#[serde(rename_all = "PascalCase")]
struct HwMonSensor {
    identifier: String,
    sensor_type: String,
    value: f32,
    #[allow(dead_code)]
    name: String,
}

// ── Temperature reading result ───────────────────────────────────────────

/// A snapshot of all temperature readings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ThermalReading {
    /// CPU temperature in °C, or `None` if unavailable.
    pub cpu_c: Option<f64>,
    /// GPU temperature in °C (max across all GPUs), or `None` if unavailable.
    pub gpu_c: Option<f64>,
    /// Maximum of CPU and GPU — used for fan curve evaluation.
    pub max_c: f64,
}

impl ThermalReading {
    /// Format a compact summary string for logging.
    pub fn summary(&self) -> String {
        let cpu = self
            .cpu_c
            .map(|t| format!("{:.1}°C", t))
            .unwrap_or_else(|| "n/a".into());
        let gpu = self
            .gpu_c
            .map(|t| format!("{:.1}°C", t))
            .unwrap_or_else(|| "n/a".into());
        format!("CPU={cpu} GPU={gpu} max={:.1}°C", self.max_c)
    }
}

// ── Public API ───────────────────────────────────────────────────────────

/// Reads CPU and GPU temperatures from multiple sources.
///
/// Holds optional WMI connections to several namespaces; at read time the
/// sources are tried in priority order and the first successful reading
/// wins for CPU.  GPU temperatures are collected from *all* available
/// sources and the maximum is used.
///
/// Create one per thread (COM is per-thread).
pub struct ThermalReader {
    #[allow(dead_code)]
    com: COMLibrary,
    /// WMI connection to root\WMI for ACPI thermal zones.
    acpi_conn: Option<WMIConnection>,
    /// WMI connection to root\CIMV2 for performance-counter thermal data.
    cimv2_conn: Option<WMIConnection>,
    /// WMI connection to root\LibreHardwareMonitor (if running).
    lhm_conn: Option<WMIConnection>,
    /// WMI connection to root\OpenHardwareMonitor (if running).
    ohm_conn: Option<WMIConnection>,
}

/// Error type for thermal reading failures.
#[derive(Debug, thiserror::Error)]
pub enum ThermalError {
    #[error("WMI error: {0}")]
    Wmi(#[from] WMIError),
    #[error("no temperature sources available")]
    NoSources,
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Try to open a WMI connection to `namespace`; return `None` on failure.
fn try_wmi_connect(namespace: &str, com: COMLibrary) -> Option<WMIConnection> {
    match WMIConnection::with_namespace_path(namespace, com) {
        Ok(conn) => {
            debug!("WMI: connected to {namespace}");
            Some(conn)
        }
        Err(e) => {
            debug!("WMI: {namespace} not available ({e})");
            None
        }
    }
}

/// Sanity-check a Celsius reading: reject values that are clearly wrong.
fn is_sane_celsius(c: f64) -> bool {
    c > 0.0 && c < 150.0
}

/// Take the maximum of an iterator of f64, returning None if empty.
fn fold_max(iter: impl Iterator<Item = f64>) -> Option<f64> {
    iter.fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))))
}

impl ThermalReader {
    /// Create a new thermal reader.  Initializes COM and attempts to connect
    /// to all known WMI temperature namespaces.  Connection failures are
    /// non-fatal — the unavailable source is simply skipped at read time.
    pub fn new() -> Result<Self, ThermalError> {
        let com = COMLibrary::new()?;

        let acpi_conn = try_wmi_connect("root\\WMI", com);
        let cimv2_conn = try_wmi_connect("root\\CIMV2", com);
        let lhm_conn = try_wmi_connect("root\\LibreHardwareMonitor", com);
        let ohm_conn = try_wmi_connect("root\\OpenHardwareMonitor", com);

        Ok(Self {
            com,
            acpi_conn,
            cimv2_conn,
            lhm_conn,
            ohm_conn,
        })
    }

    // ── CPU temperature sources ──────────────────────────────────────────

    /// ACPI thermal zones (root\WMI) — tenths of Kelvin → °C.
    fn read_acpi_thermal(&self, conn: &WMIConnection) -> Option<f64> {
        let zones: Vec<AcpiThermalZone> = match conn.query() {
            Ok(z) => z,
            Err(e) => {
                debug!("ACPI thermal query failed: {e}");
                return None;
            }
        };

        fold_max(
            zones
                .iter()
                .map(|z| {
                    let celsius = (z.current_temperature as f64 / 10.0) - 273.15;
                    debug!(
                        "ACPI zone: {} tenths-K → {:.1}°C",
                        z.current_temperature, celsius
                    );
                    celsius
                })
                .filter(|&c| is_sane_celsius(c)),
        )
    }

    /// Performance-counter thermal zones (root\CIMV2) — Kelvin → °C.
    ///
    /// Available on Windows 10 1903+ and works on both Intel and AMD.
    /// The documented unit is whole-degree Kelvin, but some systems report
    /// tenths of Kelvin.  We use a heuristic: values >500 are treated as
    /// tenths (real CPU temps never reach 227 °C / 500 K).
    fn read_perf_counter_thermal(&self, conn: &WMIConnection) -> Option<f64> {
        let zones: Vec<PerfCounterThermalZone> = match conn.query() {
            Ok(z) => z,
            Err(e) => {
                debug!("Perf-counter thermal query failed: {e}");
                return None;
            }
        };

        fold_max(
            zones
                .iter()
                .map(|z| {
                    let raw = z.temperature as f64;
                    // Heuristic: values > 500 are likely tenths-of-Kelvin.
                    let celsius = if raw > 500.0 {
                        (raw / 10.0) - 273.15
                    } else {
                        raw - 273.15
                    };
                    debug!("Perf-counter zone: {} → {:.1}°C", z.temperature, celsius);
                    celsius
                })
                .filter(|&c| is_sane_celsius(c)),
        )
    }

    /// Hardware-monitor CPU sensors (LHM / OHM) — already in °C.
    ///
    /// Filters to sensors whose identifier contains "/cpu", "/intelcpu",
    /// or "/amdcpu" and whose SensorType is "Temperature".  Returns the
    /// maximum across all CPU temperature sensors (package + cores).
    fn read_hwmon_cpu_temp(&self, conn: &WMIConnection) -> Option<f64> {
        let sensors: Vec<HwMonSensor> = match conn.query() {
            Ok(s) => s,
            Err(e) => {
                debug!("HW-monitor sensor query failed: {e}");
                return None;
            }
        };

        fold_max(
            sensors
                .iter()
                .filter(|s| s.sensor_type == "Temperature")
                .filter(|s| {
                    let id = s.identifier.to_lowercase();
                    id.contains("/cpu") || id.contains("/intelcpu") || id.contains("/amdcpu")
                })
                .map(|s| {
                    debug!("HW-monitor CPU sensor: {} ({}) = {:.1}°C", s.name, s.identifier, s.value);
                    s.value as f64
                })
                .filter(|&c| is_sane_celsius(c)),
        )
    }

    /// Read the current CPU temperature in °C by trying each source in
    /// priority order.  Returns the first successful reading.
    fn read_cpu_temp(&self) -> Option<f64> {
        // 1. ACPI Thermal Zone (root\WMI)
        if let Some(ref conn) = self.acpi_conn {
            if let Some(temp) = self.read_acpi_thermal(conn) {
                debug!("CPU temp via ACPI thermal zone: {temp:.1}°C");
                return Some(temp);
            }
        }

        // 2. Performance Counters (root\CIMV2)
        if let Some(ref conn) = self.cimv2_conn {
            if let Some(temp) = self.read_perf_counter_thermal(conn) {
                debug!("CPU temp via performance counters: {temp:.1}°C");
                return Some(temp);
            }
        }

        // 3. LibreHardwareMonitor
        if let Some(ref conn) = self.lhm_conn {
            if let Some(temp) = self.read_hwmon_cpu_temp(conn) {
                debug!("CPU temp via LibreHardwareMonitor: {temp:.1}°C");
                return Some(temp);
            }
        }

        // 4. OpenHardwareMonitor
        if let Some(ref conn) = self.ohm_conn {
            if let Some(temp) = self.read_hwmon_cpu_temp(conn) {
                debug!("CPU temp via OpenHardwareMonitor: {temp:.1}°C");
                return Some(temp);
            }
        }

        None
    }

    // ── GPU temperature sources ──────────────────────────────────────────

    /// Collect NVIDIA GPU temperatures via nvidia-smi.
    ///
    /// nvidia-smi returns one line per GPU; all valid readings are appended
    /// to `out`.  Silently returns nothing if nvidia-smi is not installed.
    fn collect_nvidia_smi_temps(&self, out: &mut Vec<f64>) {
        let output = match Command::new("nvidia-smi")
            .args([
                "--query-gpu=temperature.gpu",
                "--format=csv,noheader,nounits",
            ])
            .output()
        {
            Ok(o) if o.status.success() => o,
            _ => return,
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if let Ok(t) = line.trim().parse::<f64>() {
                if is_sane_celsius(t) {
                    debug!("nvidia-smi GPU temp: {t:.0}°C");
                    out.push(t);
                }
            }
        }
    }

    /// Collect GPU temperatures from a hardware-monitor WMI connection.
    ///
    /// Filters sensors whose identifier contains "/gpu" (matches
    /// /gpu-nvidia/, /gpu-amd/, /gpu-intel/, etc.) and whose SensorType
    /// is "Temperature".  All valid readings are appended to `out`.
    fn collect_hwmon_gpu_temps(&self, conn: &WMIConnection, out: &mut Vec<f64>) {
        let sensors: Vec<HwMonSensor> = match conn.query() {
            Ok(s) => s,
            Err(_) => return,
        };

        for s in sensors
            .iter()
            .filter(|s| s.sensor_type == "Temperature")
            .filter(|s| s.identifier.to_lowercase().contains("/gpu"))
        {
            let t = s.value as f64;
            if is_sane_celsius(t) {
                debug!(
                    "HW-monitor GPU sensor: {} ({}) = {:.1}°C",
                    s.name, s.identifier, t
                );
                out.push(t);
            }
        }
    }

    /// Read GPU temperatures from all available sources and return the max.
    ///
    /// Temperatures are collected from nvidia-smi (NVIDIA GPUs) and from
    /// LibreHardwareMonitor / OpenHardwareMonitor (any vendor).  If both
    /// report the same GPU the duplicate is harmless — we only need the max.
    fn read_gpu_temp(&self) -> Option<f64> {
        let mut all_temps: Vec<f64> = Vec::new();

        // 1. NVIDIA GPUs via nvidia-smi
        self.collect_nvidia_smi_temps(&mut all_temps);

        // 2. Any GPU via LibreHardwareMonitor
        if let Some(ref conn) = self.lhm_conn {
            self.collect_hwmon_gpu_temps(conn, &mut all_temps);
        }

        // 3. Any GPU via OpenHardwareMonitor
        if let Some(ref conn) = self.ohm_conn {
            self.collect_hwmon_gpu_temps(conn, &mut all_temps);
        }

        if all_temps.is_empty() {
            return None;
        }

        let max = all_temps
            .iter()
            .copied()
            .fold(f64::NEG_INFINITY, f64::max);

        if all_temps.len() > 1 {
            debug!(
                "GPU temps ({} source(s)): {:?} → max {:.1}°C",
                all_temps.len(),
                all_temps,
                max
            );
        }

        Some(max)
    }

    // ── Combined reading ─────────────────────────────────────────────────

    /// Read temperatures from all sources and return a `ThermalReading`.
    ///
    /// Returns individual CPU and GPU temperatures plus the overall max.
    /// The max is used for fan curve evaluation.  Returns an error only if
    /// *no* source provides a reading.
    pub fn read_temps(&self) -> Result<ThermalReading, ThermalError> {
        let cpu = self.read_cpu_temp();
        let gpu = self.read_gpu_temp();

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

        debug!("Thermal: {}", reading.summary());
        Ok(reading)
    }
}

/// Try to create a ThermalReader; if it fails, log a warning and return None.
///
/// This is used at daemon startup so that initialisation failures don't
/// prevent the service from running (it will just lack automatic fan
/// control).  Logs which temperature sources are available and performs a
/// test read.
pub fn try_init() -> Option<ThermalReader> {
    match ThermalReader::new() {
        Ok(reader) => {
            // Log which WMI namespaces connected successfully.
            let mut wmi_sources = Vec::new();
            if reader.acpi_conn.is_some() {
                wmi_sources.push("ACPI thermal zones (root\\WMI)");
            }
            if reader.cimv2_conn.is_some() {
                wmi_sources.push("performance counters (root\\CIMV2)");
            }
            if reader.lhm_conn.is_some() {
                wmi_sources.push("LibreHardwareMonitor");
            }
            if reader.ohm_conn.is_some() {
                wmi_sources.push("OpenHardwareMonitor");
            }

            if wmi_sources.is_empty() {
                warn!("No WMI temperature namespaces available");
            } else {
                info!("WMI temperature sources: {}", wmi_sources.join(", "));
            }

            // Check nvidia-smi availability.
            match Command::new("nvidia-smi").arg("-L").output() {
                Ok(output) if output.status.success() => {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    let gpu_count = stdout.lines().count();
                    info!("nvidia-smi: {gpu_count} NVIDIA GPU(s) detected");
                }
                Ok(_) => {
                    debug!("nvidia-smi found but returned error (no NVIDIA GPU?)");
                }
                Err(_) => {
                    info!("nvidia-smi not found; NVIDIA GPU temps via WMI only");
                }
            }

            // Do a test read to make sure at least one source works.
            match reader.read_temps() {
                Ok(reading) => {
                    info!("Thermal reader initialized: {}", reading.summary());
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
            warn!("Failed to initialize COM for thermal reading: {e}");
            warn!("Automatic fan control will be unavailable");
            None
        }
    }
}
