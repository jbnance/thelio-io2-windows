// src/thermal.rs — CPU / GPU Temperature Reading
//
// Two backends are supported for reading hardware temperatures:
//
// 1. **Library** (`--lhm-mode library`, default): Spawns the `lhm-helper`
//    sidecar process, which uses the LibreHardwareMonitorLib NuGet package
//    directly.  No running LHM instance required.  GPU temps (NVIDIA, AMD,
//    Intel) are read natively by the library.
//
// 2. **HTTP** (`--lhm-mode http`): Connects to the LibreHardwareMonitor web
//    server at a configurable URL.  Requires LHM to be running with its
//    built-in HTTP server enabled.  nvidia-smi is used as a supplementary
//    GPU temperature source.
//
// Both backends produce the same `ThermalReading` output.  The daemon polls
// every thermal cycle (2 s) and uses the max of CPU and GPU readings for
// fan curve evaluation.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

use base64::Engine;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};

use crate::thermal_lib::{self, LibThermalReader};

// ── LHM configuration ───────────────────────────────────────────────────

/// Configuration for connecting to the LibreHardwareMonitor web server.
pub struct LhmConfig {
    /// Base URL including scheme and port, e.g. `http://localhost:8085`.
    pub url: String,
    /// Optional HTTP Basic Auth username.
    pub username: Option<String>,
    /// Optional HTTP Basic Auth password.
    pub password: Option<String>,
}

impl Default for LhmConfig {
    fn default() -> Self {
        Self {
            url: "http://localhost:8085".into(),
            username: None,
            password: None,
        }
    }
}

// ── LHM JSON tree ───────────────────────────────────────────────────────

/// A node in the LHM `/data.json` sensor tree.
///
/// The tree is hierarchical: Computer → Hardware → Category → Sensor.
/// Only leaf sensor nodes have `sensor_id`, `sensor_type`, and `raw_value`.
#[derive(Deserialize, Debug)]
struct LhmNode {
    #[serde(rename = "Children", default)]
    children: Vec<LhmNode>,

    /// Sensor identifier path, e.g. `/amdcpu/0/temperature/0`.
    /// Only present on sensor nodes.
    #[serde(rename = "SensorId")]
    sensor_id: Option<String>,

    /// Sensor type string, e.g. `"Temperature"`, `"Load"`, `"Fan"`.
    /// Only present on sensor nodes.
    #[serde(rename = "Type")]
    sensor_type: Option<String>,

    /// Raw value as a string.  May be a plain number (`"65.5"`) or include
    /// a unit suffix (`"46.9 °C"`, `"1200 RPM"`).  Only present on sensor nodes.
    #[serde(rename = "RawValue")]
    raw_value: Option<String>,

    /// Human-readable name, e.g. `"CPU Package"`, `"GPU Core"`.
    #[serde(rename = "Text", default)]
    text: String,
}

/// A flattened temperature sensor extracted from the LHM tree.
#[derive(Debug)]
struct TempSensor {
    id: String,
    name: String,
    temp_c: f64,
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

/// Reads CPU and GPU temperatures from the LHM HTTP API (web server backend).
pub struct HttpThermalReader {
    /// Full URL to the /data.json endpoint.
    url: String,
    /// Pre-encoded Basic Auth header value, if credentials were provided.
    auth_header: Option<String>,
    /// Reusable HTTP agent with timeout settings.
    agent: ureq::Agent,
}

/// Error type for thermal reading failures.
#[derive(Debug, thiserror::Error)]
pub enum ThermalError {
    #[error("HTTP request failed: {0}")]
    Http(String),
    #[error("JSON parse error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("no temperature sources available")]
    NoSources,
}

// ── Helpers ──────────────────────────────────────────────────────────────

/// Sanity-check a Celsius reading: reject values that are clearly wrong.
pub(crate) fn is_sane_celsius(c: f64) -> bool {
    c > 0.0 && c < 150.0
}

/// Take the maximum of an iterator of f64, returning None if empty.
pub(crate) fn fold_max(iter: impl Iterator<Item = f64>) -> Option<f64> {
    iter.fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))))
}

/// Extract the leading numeric portion from a value string.
///
/// LHM's `RawValue` field may be a plain number (`"65.5"`) or may include
/// a unit suffix (`"46.9 °C"`, `"1200 RPM"`).  This function strips any
/// trailing non-numeric characters and parses the number.
fn parse_raw_value(raw: &str) -> Option<f64> {
    let trimmed = raw.trim();
    // Find the end of the numeric portion: optional leading minus, digits, optional decimal.
    let end = trimmed
        .find(|c: char| !c.is_ascii_digit() && c != '.' && c != '-')
        .unwrap_or(trimmed.len());
    trimmed[..end].parse::<f64>().ok()
}

/// Recursively walk the LHM node tree and collect all temperature sensors.
fn collect_temp_sensors(node: &LhmNode, out: &mut Vec<TempSensor>) {
    // Check if this node is a temperature sensor.
    if let (Some(id), Some(typ), Some(raw)) =
        (&node.sensor_id, &node.sensor_type, &node.raw_value)
    {
        if typ == "Temperature" {
            if let Some(temp) = parse_raw_value(raw) {
                if is_sane_celsius(temp) {
                    out.push(TempSensor {
                        id: id.clone(),
                        name: node.text.clone(),
                        temp_c: temp,
                    });
                }
            }
        }
    }

    // Recurse into children.
    for child in &node.children {
        collect_temp_sensors(child, out);
    }
}

impl HttpThermalReader {
    /// Create a new thermal reader that connects to the LHM web server.
    pub fn new(config: &LhmConfig) -> Self {
        // Build the full /data.json URL.
        let url = format!("{}/data.json", config.url.trim_end_matches('/'));

        // Pre-encode the Basic Auth header if credentials are provided.
        let auth_header = match (&config.username, &config.password) {
            (Some(user), Some(pass)) => {
                let encoded = base64::engine::general_purpose::STANDARD
                    .encode(format!("{user}:{pass}"));
                Some(format!("Basic {encoded}"))
            }
            (Some(user), None) => {
                let encoded =
                    base64::engine::general_purpose::STANDARD.encode(format!("{user}:"));
                Some(format!("Basic {encoded}"))
            }
            _ => None,
        };

        // Build a reusable HTTP agent with a short timeout.
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_connect(Some(Duration::from_secs(3)))
            .timeout_recv_response(Some(Duration::from_secs(5)))
            .timeout_recv_body(Some(Duration::from_secs(5)))
            .build()
            .into();

        Self {
            url,
            auth_header,
            agent,
        }
    }

    /// Fetch and parse the LHM sensor tree, returning all temperature sensors.
    fn fetch_temp_sensors(&self) -> Result<Vec<TempSensor>, ThermalError> {
        let mut req = self.agent.get(&self.url);
        if let Some(ref auth) = self.auth_header {
            req = req.header("Authorization", auth);
        }

        let body = req
            .call()
            .map_err(|e| ThermalError::Http(e.to_string()))?
            .body_mut()
            .read_to_string()
            .map_err(|e| ThermalError::Http(e.to_string()))?;

        let root: LhmNode = serde_json::from_str(&body)?;

        let mut sensors = Vec::new();
        collect_temp_sensors(&root, &mut sensors);

        Ok(sensors)
    }

    /// Read the current CPU temperature in °C from LHM sensors.
    ///
    /// Filters sensors whose identifier contains `/cpu`, `/intelcpu`, or
    /// `/amdcpu`.  Returns the max across all CPU temperature sensors.
    fn read_cpu_temp(&self, sensors: &[TempSensor]) -> Option<f64> {
        fold_max(
            sensors
                .iter()
                .filter(|s| {
                    let id = s.id.to_lowercase();
                    id.contains("/cpu") || id.contains("/intelcpu") || id.contains("/amdcpu")
                })
                .map(|s| {
                    debug!("LHM CPU sensor: {} ({}) = {:.1}°C", s.name, s.id, s.temp_c);
                    s.temp_c
                }),
        )
    }

    /// Read the current GPU temperature in °C from LHM sensors and nvidia-smi.
    ///
    /// LHM sensors matching `/gpu` are collected along with any nvidia-smi
    /// readings.  Returns the max across all GPU temperature readings.
    fn read_gpu_temp(&self, sensors: &[TempSensor]) -> Option<f64> {
        let mut all_temps: Vec<f64> = Vec::new();

        // LHM GPU sensors (any vendor).
        for s in sensors
            .iter()
            .filter(|s| s.id.to_lowercase().contains("/gpu"))
        {
            debug!("LHM GPU sensor: {} ({}) = {:.1}°C", s.name, s.id, s.temp_c);
            all_temps.push(s.temp_c);
        }

        // nvidia-smi supplement.
        self.collect_nvidia_smi_temps(&mut all_temps);

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

    /// Collect NVIDIA GPU temperatures via nvidia-smi.
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

    /// Read temperatures from all sources and return a `ThermalReading`.
    ///
    /// Fetches the LHM sensor tree once, then extracts CPU and GPU temps.
    /// Returns an error only if *no* source provides a reading.
    pub fn read_temps(&self) -> Result<ThermalReading, ThermalError> {
        let sensors = self.fetch_temp_sensors()?;

        let cpu = self.read_cpu_temp(&sensors);
        let gpu = self.read_gpu_temp(&sensors);

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

/// Try to create a HttpThermalReader and perform a test read.
///
/// Returns `None` (with warnings logged) if LHM is not reachable or
/// returns no temperature data.  The daemon will fall back to manual mode.
pub fn try_init(config: &LhmConfig) -> Option<HttpThermalReader> {
    let reader = HttpThermalReader::new(config);

    info!("Connecting to LibreHardwareMonitor at {}", reader.url);
    if reader.auth_header.is_some() {
        info!("Using HTTP Basic Auth for LHM connection");
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
            info!("nvidia-smi not found; GPU temps via LHM only");
        }
    }

    // Do a test read to verify LHM is reachable and returning data.
    match reader.read_temps() {
        Ok(reading) => {
            info!("Thermal reader initialized: {}", reading.summary());
            Some(reader)
        }
        Err(ThermalError::Http(e)) => {
            warn!("Cannot reach LibreHardwareMonitor web server: {e}");
            warn!(
                "Ensure LHM is running with the web server enabled \
                 (Options → HTTP Server)"
            );
            warn!("Automatic fan control will be unavailable");
            None
        }
        Err(e) => {
            warn!("Thermal reader test read failed: {e}");
            warn!("Automatic fan control will be unavailable");
            None
        }
    }
}

// ── ThermalSource — unified backend dispatch ─────────────────────────────

/// Which backend to use for reading temperatures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LhmMode {
    /// Connect to a running LHM web server via HTTP.
    Http,
    /// Use the lhm-helper sidecar (LibreHardwareMonitorLib).
    Library,
}

/// Unified temperature source wrapping either backend.
pub enum ThermalSource {
    Http(HttpThermalReader),
    Library(LibThermalReader),
}

impl ThermalSource {
    /// Read current temperatures, delegating to the active backend.
    pub fn read_temps(&mut self) -> Result<ThermalReading, ThermalError> {
        match self {
            Self::Http(r) => r.read_temps(),
            Self::Library(r) => r.read_temps(),
        }
    }
}

/// Initialize a `ThermalSource` using the specified backend mode.
///
/// Returns `None` (with warnings logged) if the backend cannot be started
/// or returns no temperature data.
pub fn try_init_source(
    mode: LhmMode,
    config: &LhmConfig,
    helper_path: &Path,
) -> Option<ThermalSource> {
    match mode {
        LhmMode::Http => try_init(config).map(ThermalSource::Http),
        LhmMode::Library => thermal_lib::try_init_lib(helper_path).map(ThermalSource::Library),
    }
}

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_raw_value ──────────────────────────────────────────────────

    #[test]
    fn parse_raw_value_plain_integer() {
        assert_eq!(parse_raw_value("65"), Some(65.0));
    }

    #[test]
    fn parse_raw_value_plain_float() {
        assert_eq!(parse_raw_value("46.9"), Some(46.9));
    }

    #[test]
    fn parse_raw_value_with_celsius_suffix() {
        // LHM sends values like "46.9 °C"
        assert_eq!(parse_raw_value("46.9 °C"), Some(46.9));
    }

    #[test]
    fn parse_raw_value_with_rpm_suffix() {
        assert_eq!(parse_raw_value("1200 RPM"), Some(1200.0));
    }

    #[test]
    fn parse_raw_value_with_leading_whitespace() {
        assert_eq!(parse_raw_value("  72.3 °C  "), Some(72.3));
    }

    #[test]
    fn parse_raw_value_negative() {
        assert_eq!(parse_raw_value("-5.0"), Some(-5.0));
    }

    #[test]
    fn parse_raw_value_empty_string() {
        assert_eq!(parse_raw_value(""), None);
    }

    #[test]
    fn parse_raw_value_non_numeric() {
        assert_eq!(parse_raw_value("abc"), None);
    }

    #[test]
    fn parse_raw_value_zero() {
        assert_eq!(parse_raw_value("0"), Some(0.0));
    }

    // ── is_sane_celsius ──────────────────────────────────────────────────

    #[test]
    fn sane_celsius_normal_range() {
        assert!(is_sane_celsius(25.0));
        assert!(is_sane_celsius(65.5));
        assert!(is_sane_celsius(100.0));
    }

    #[test]
    fn sane_celsius_boundary_low() {
        // 0.0 is rejected (> 0.0 required)
        assert!(!is_sane_celsius(0.0));
        assert!(is_sane_celsius(0.1));
    }

    #[test]
    fn sane_celsius_boundary_high() {
        // 150.0 is rejected (< 150.0 required)
        assert!(!is_sane_celsius(150.0));
        assert!(is_sane_celsius(149.9));
    }

    #[test]
    fn sane_celsius_negative() {
        assert!(!is_sane_celsius(-10.0));
    }

    #[test]
    fn sane_celsius_extreme() {
        assert!(!is_sane_celsius(999.0));
        assert!(!is_sane_celsius(f64::NAN));
    }

    // ── fold_max ────────────────────────────────────────────────────────

    #[test]
    fn fold_max_empty() {
        let result = fold_max(std::iter::empty());
        assert_eq!(result, None);
    }

    #[test]
    fn fold_max_single() {
        let result = fold_max(std::iter::once(42.0));
        assert_eq!(result, Some(42.0));
    }

    #[test]
    fn fold_max_multiple() {
        let result = fold_max(vec![10.0, 50.0, 30.0].into_iter());
        assert_eq!(result, Some(50.0));
    }

    #[test]
    fn fold_max_all_equal() {
        let result = fold_max(vec![25.0, 25.0, 25.0].into_iter());
        assert_eq!(result, Some(25.0));
    }

    #[test]
    fn fold_max_with_negatives() {
        let result = fold_max(vec![-5.0, -1.0, -10.0].into_iter());
        assert_eq!(result, Some(-1.0));
    }

    // ── collect_temp_sensors ────────────────────────────────────────────

    #[test]
    fn collect_temp_sensors_leaf_node() {
        let node = LhmNode {
            children: vec![],
            sensor_id: Some("/amdcpu/0/temperature/0".into()),
            sensor_type: Some("Temperature".into()),
            raw_value: Some("65.5 °C".into()),
            text: "CPU Package".into(),
        };
        let mut sensors = Vec::new();
        collect_temp_sensors(&node, &mut sensors);

        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].id, "/amdcpu/0/temperature/0");
        assert_eq!(sensors[0].name, "CPU Package");
        assert!((sensors[0].temp_c - 65.5).abs() < 0.001);
    }

    #[test]
    fn collect_temp_sensors_nested_tree() {
        let node = LhmNode {
            children: vec![
                LhmNode {
                    children: vec![
                        LhmNode {
                            children: vec![],
                            sensor_id: Some("/amdcpu/0/temperature/0".into()),
                            sensor_type: Some("Temperature".into()),
                            raw_value: Some("70.0".into()),
                            text: "CPU Core".into(),
                        },
                        LhmNode {
                            children: vec![],
                            sensor_id: Some("/amdcpu/0/load/0".into()),
                            sensor_type: Some("Load".into()),
                            raw_value: Some("50.0".into()),
                            text: "CPU Total".into(),
                        },
                    ],
                    sensor_id: None,
                    sensor_type: None,
                    raw_value: None,
                    text: "AMD Ryzen".into(),
                },
                LhmNode {
                    children: vec![LhmNode {
                        children: vec![],
                        sensor_id: Some("/gpu-nvidia/0/temperature/0".into()),
                        sensor_type: Some("Temperature".into()),
                        raw_value: Some("55.0 °C".into()),
                        text: "GPU Core".into(),
                    }],
                    sensor_id: None,
                    sensor_type: None,
                    raw_value: None,
                    text: "NVIDIA RTX".into(),
                },
            ],
            sensor_id: None,
            sensor_type: None,
            raw_value: None,
            text: "Computer".into(),
        };

        let mut sensors = Vec::new();
        collect_temp_sensors(&node, &mut sensors);

        // Should find 2 Temperature sensors (not the Load sensor)
        assert_eq!(sensors.len(), 2);
        assert_eq!(sensors[0].name, "CPU Core");
        assert_eq!(sensors[1].name, "GPU Core");
    }

    #[test]
    fn collect_temp_sensors_rejects_insane_temp() {
        let node = LhmNode {
            children: vec![],
            sensor_id: Some("/cpu/0/temperature/0".into()),
            sensor_type: Some("Temperature".into()),
            raw_value: Some("0.0 °C".into()), // rejected: not > 0
            text: "Dead Sensor".into(),
        };
        let mut sensors = Vec::new();
        collect_temp_sensors(&node, &mut sensors);

        assert!(sensors.is_empty());
    }

    #[test]
    fn collect_temp_sensors_skips_non_temperature_type() {
        let node = LhmNode {
            children: vec![],
            sensor_id: Some("/cpu/0/fan/0".into()),
            sensor_type: Some("Fan".into()),
            raw_value: Some("1200 RPM".into()),
            text: "CPU Fan".into(),
        };
        let mut sensors = Vec::new();
        collect_temp_sensors(&node, &mut sensors);

        assert!(sensors.is_empty());
    }

    #[test]
    fn collect_temp_sensors_empty_tree() {
        let node = LhmNode {
            children: vec![],
            sensor_id: None,
            sensor_type: None,
            raw_value: None,
            text: "Root".into(),
        };
        let mut sensors = Vec::new();
        collect_temp_sensors(&node, &mut sensors);

        assert!(sensors.is_empty());
    }

    // ── ThermalReading::summary ─────────────────────────────────────────

    #[test]
    fn thermal_reading_summary_both() {
        let r = ThermalReading {
            cpu_c: Some(65.0),
            gpu_c: Some(55.0),
            max_c: 65.0,
        };
        assert_eq!(r.summary(), "CPU=65.0°C GPU=55.0°C max=65.0°C");
    }

    #[test]
    fn thermal_reading_summary_cpu_only() {
        let r = ThermalReading {
            cpu_c: Some(70.0),
            gpu_c: None,
            max_c: 70.0,
        };
        assert_eq!(r.summary(), "CPU=70.0°C GPU=n/a max=70.0°C");
    }
}
