// src/device.rs — Shared Device trait and data types
//
// Abstracts over both the original System76 Io board (USB bulk/serial)
// and the Thelio Io 2 (HID), so the IPC layer and service logic can be
// device-agnostic.

use anyhow::Result;
use serde::{Deserialize, Serialize};

/// A single fan channel with its label, current RPM, and PWM duty cycle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanChannel {
    /// Index used in IPC commands (0-based)
    pub index: usize,
    /// Human-readable name, e.g. "CPU Fan" or "CPUF"
    pub label: String,
    /// Current fan speed in RPM
    pub rpm: u32,
    /// Current PWM duty cycle 0–255
    pub pwm: u8,
}

/// Full snapshot of device state, returned by `Device::read_state`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceState {
    pub device_name: String,
    pub fans: Vec<FanChannel>,
}

/// Commands that can be sent to a device.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeviceCommand {
    /// Read all fan channels and return a DeviceState.
    ReadState,
    /// Set PWM for a channel (0-based index, value 0–255).
    /// When a profile is active, this switches to Manual mode.
    SetPwm { channel: usize, pwm: u8 },
    /// Notify device about an upcoming system suspend.
    NotifySuspend,
    /// Notify device that the system has resumed.
    NotifyResume,
    /// Set the active power profile ("quiet", "balanced", "performance", "manual").
    SetProfile { profile: String },
    /// Query the current power profile.
    GetProfile,
}

/// Errors that a device operation can return, serializable for IPC.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
pub enum DeviceError {
    #[error("device not connected")]
    NotConnected,
    #[error("channel {0} does not exist")]
    InvalidChannel(usize),
    #[error("PWM value {0} out of range (0–255)")]
    InvalidPwm(u8),
    #[error("communication error: {0}")]
    Comm(String),
    #[error("device returned an error response")]
    DeviceError,
    #[error("operation timed out")]
    Timeout,
}

/// IPC response envelope sent back to named-pipe clients.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum IpcResponse {
    State(DeviceState),
    Ok,
    Error(DeviceError),
    /// Current profile name and temperature info.
    ProfileInfo {
        profile: String,
        temp_c: Option<f64>,
    },
}

/// The core trait every device backend must implement.
/// All methods take `&mut self` because USB and HID handles are not `Send`-safe
/// across threads without wrapping; the service loop drives a single backend
/// from one thread and forwards results via channels.
pub trait Device: Send {
    /// Return a human-readable name for this device.
    fn name(&self) -> &str;

    /// Return the number of fan channels this device exposes.
    fn fan_count(&self) -> usize;

    /// Return the label for the given channel.
    fn fan_label(&self, channel: usize) -> Result<String, DeviceError>;

    /// Read the RPM tachometer for a fan channel.
    fn read_rpm(&mut self, channel: usize) -> Result<u32, DeviceError>;

    /// Read the PWM duty cycle (0–255) for a fan channel.
    fn read_pwm(&mut self, channel: usize) -> Result<u8, DeviceError>;

    /// Write a PWM duty cycle (0–255) for a fan channel.
    fn write_pwm(&mut self, channel: usize, pwm: u8) -> Result<(), DeviceError>;

    /// Notify the device that the system is about to suspend.
    fn notify_suspend(&mut self) -> Result<(), DeviceError>;

    /// Notify the device that the system has resumed from suspend.
    fn notify_resume(&mut self) -> Result<(), DeviceError>;

    /// Convenience: read the full device state in one shot.
    fn read_state(&mut self) -> Result<DeviceState, DeviceError> {
        let name = self.name().to_string();
        let count = self.fan_count();
        let mut fans = Vec::with_capacity(count);

        for i in 0..count {
            let label = self.fan_label(i)?;
            let rpm = self.read_rpm(i)?;
            let pwm = self.read_pwm(i)?;
            fans.push(FanChannel { index: i, label, rpm, pwm });
        }

        Ok(DeviceState { device_name: name, fans })
    }
}
