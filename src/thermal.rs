// src/thermal.rs — CPU / GPU Temperature Reading via LibreHardwareMonitor HTTP
//
// Temperature data comes from LibreHardwareMonitor (LHM), which must be
// running with its built-in web server enabled.  LHM serves a JSON sensor
// tree at /data.json that contains per-component temperature readings for
// any CPU and GPU vendor (Intel, AMD, NVIDIA, Intel Arc).
//
// The daemon polls LHM's HTTP endpoint every thermal cycle (2 s).  The
// JSON response is a recursive tree of nodes; we walk it to find all
// temperature sensors, then select the max CPU and max GPU readings.
//
// nvidia-smi is used as a supplementary GPU temperature source.
//
// The overall temperature used for fan control is the maximum of CPU and
// all GPU readings, since the Thelio chassis fans cool the entire system.

use std::process::Command;
use std::time::Duration;

use base64::Engine;
use log::{debug, info, warn};
use serde::{Deserialize, Serialize};

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

    /// Raw numeric value as a string, e.g. `"65.5"`.
    /// Only present on sensor nodes.
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

/// Reads CPU and GPU temperatures from the LHM HTTP API.
pub struct ThermalReader {
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
fn is_sane_celsius(c: f64) -> bool {
    c > 0.0 && c < 150.0
}

/// Take the maximum of an iterator of f64, returning None if empty.
fn fold_max(iter: impl Iterator<Item = f64>) -> Option<f64> {
    iter.fold(None, |acc: Option<f64>, v| Some(acc.map_or(v, |a| a.max(v))))
}

/// Recursively walk the LHM node tree and collect all temperature sensors.
fn collect_temp_sensors(node: &LhmNode, out: &mut Vec<TempSensor>) {
    // Check if this node is a temperature sensor.
    if let (Some(id), Some(typ), Some(raw)) =
        (&node.sensor_id, &node.sensor_type, &node.raw_value)
    {
        if typ == "Temperature" {
            if let Ok(temp) = raw.parse::<f64>() {
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

impl ThermalReader {
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
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(3))
            .timeout_read(Duration::from_secs(5))
            .build();

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
            req = req.set("Authorization", auth);
        }

        let resp = req.call().map_err(|e| ThermalError::Http(e.to_string()))?;
        let body = resp
            .into_string()
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

/// Try to create a ThermalReader and perform a test read.
///
/// Returns `None` (with warnings logged) if LHM is not reachable or
/// returns no temperature data.  The daemon will fall back to manual mode.
pub fn try_init(config: &LhmConfig) -> Option<ThermalReader> {
    let reader = ThermalReader::new(config);

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
