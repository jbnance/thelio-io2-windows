// src/thelio_io.rs — Thelio Io 2 Driver (HID)
//
// USB Vendor: 0x3384  Product: 0x000B
//
// Communicates via 32-byte HID output/input reports (no report IDs).
// Report layout (0-based byte offsets):
//   [0]  HID_CMD  — command byte sent to device
//   [1]  HID_RES  — result/status byte received from device (0x00 = success)
//   [2…] HID_DATA — payload bytes
//
// Commands:
//   CMD_FAN_GET      (7)  — read fan PWM; data[0]=channel → data[1]=pwm
//   CMD_FAN_SET      (8)  — set fan PWM;  data[0]=channel, data[1]=pwm
//   CMD_LED_SET_MODE (16) — LED mode;     data[0]=0, data[1]=1 (suspend) / 0 (resume)
//   CMD_FAN_TACH     (22) — read RPM;     data[0]=channel → data[1]|data[2]<<8 = rpm
//
// Fan channels (0-based):
//   0 = CPU Fan
//   1 = Intake Fan
//   2 = GPU Fan
//   3 = Aux Fan

use hidapi::{HidApi, HidDevice};
use log::{debug, info};

use crate::device::{Device as DeviceTrait, DeviceError};

// ── USB / HID identifiers ──────────────────────────────────────────────────
const VENDOR_ID: u16 = 0x3384;
const PRODUCT_ID: u16 = 0x000B;

// ── HID report layout ──────────────────────────────────────────────────────
const BUFFER_SIZE: usize = 32;
// hidapi on Windows requires a leading report-ID byte (0x00 for devices with
// no report IDs) when writing. The write buffer is therefore one byte larger
// than the HID report, with the payload starting at index 1.
const WRITE_BUFFER_SIZE: usize = BUFFER_SIZE + 1;
const HID_CMD: usize = 0;
const HID_RES: usize = 1;
const HID_DATA: usize = 2;

// ── Command bytes ──────────────────────────────────────────────────────────
const CMD_FAN_GET: u8 = 7;
const CMD_FAN_SET: u8 = 8;
const CMD_LED_SET_MODE: u8 = 16;
const CMD_FAN_TACH: u8 = 22;

// ── HID read timeout ──────────────────────────────────────────────────────
const REQ_TIMEOUT_MS: i32 = 300;

// ── Fan channel metadata ───────────────────────────────────────────────────
const FAN_LABELS: &[&str] = &["CPU Fan", "Intake Fan", "GPU Fan", "Aux Fan"];

// ── Driver struct ──────────────────────────────────────────────────────────
pub struct ThelioIoDevice {
    device: HidDevice,
    /// Receive buffer — 32 bytes, no report ID prefix.
    buffer: [u8; BUFFER_SIZE],
    /// Transmit buffer — 33 bytes, byte 0 is the 0x00 report ID required by
    /// hidapi on Windows; the actual HID payload starts at byte 1.
    write_buf: [u8; WRITE_BUFFER_SIZE],
}

// ── Discovery ──────────────────────────────────────────────────────────────

/// Try to open the first connected Thelio Io 2 device.
pub fn open() -> anyhow::Result<Option<ThelioIoDevice>> {
    let api = HidApi::new()?;

    for dev_info in api.device_list() {
        if dev_info.vendor_id() == VENDOR_ID && dev_info.product_id() == PRODUCT_ID {
            info!(
                "Found Thelio Io 2 at {:?} (usage page {:04X} usage {:04X})",
                dev_info.path(),
                dev_info.usage_page(),
                dev_info.usage(),
            );

            // On Windows there may be multiple HID interfaces; the kernel
            // driver checks collection[0].usage == 0xFF600061.
            // The hidapi DeviceInfo doesn't expose collection data directly,
            // but usage_page 0xFF60 with usage 0x61 is the right interface.
            if dev_info.usage_page() != 0xFF60 || dev_info.usage() != 0x61 {
                debug!("  Skipping (wrong usage page/usage)");
                continue;
            }

            let device = dev_info.open_device(&api)?;
            let mut d = ThelioIoDevice {
                device,
                buffer: [0u8; BUFFER_SIZE],
                write_buf: [0u8; WRITE_BUFFER_SIZE],
            };

            // Verify comms with a benign LED-mode read (set mode 0 = normal).
            // We don't fail on error here; some firmware versions may not
            // respond to this command.
            let _ = d.send_cmd(CMD_LED_SET_MODE, 0, 0, 0);

            info!("Thelio Io 2 opened successfully");
            return Ok(Some(d));
        }
    }

    Ok(None)
}

// ── Low-level protocol ─────────────────────────────────────────────────────

impl ThelioIoDevice {
    /// Send a 32-byte output report and block until we receive an input report.
    /// Returns the response buffer on success.
    fn send_cmd(&mut self, cmd: u8, b1: u8, b2: u8, b3: u8) -> Result<[u8; BUFFER_SIZE], DeviceError> {
        // Build the write buffer. Byte 0 is the report ID (0x00 = no report
        // ID); hidapi on Windows requires this prefix and strips it before
        // sending, so the device sees a clean 32-byte report starting at
        // write_buf[1].
        self.write_buf = [0u8; WRITE_BUFFER_SIZE];
        self.write_buf[1 + HID_CMD] = cmd;
        self.write_buf[1 + HID_DATA] = b1;
        self.write_buf[1 + HID_DATA + 1] = b2;
        self.write_buf[1 + HID_DATA + 2] = b3;

        self.device
            .write(&self.write_buf)
            .map_err(|e| DeviceError::Comm(e.to_string()))?;

        // HID input report with timeout
        let n = self.device
            .read_timeout(&mut self.buffer, REQ_TIMEOUT_MS)
            .map_err(|e| DeviceError::Comm(e.to_string()))?;

        if n == 0 {
            return Err(DeviceError::Timeout);
        }

        debug!(
            "cmd={:02X} b1={:02X} b2={:02X} → res={:02X} d[0]={:02X} d[1]={:02X}",
            cmd, b1, b2,
            self.buffer[HID_RES],
            self.buffer[HID_DATA],
            self.buffer[HID_DATA + 1],
        );

        // Check the device-side result byte
        if self.buffer[HID_RES] != 0x00 {
            return Err(DeviceError::BadResponse);
        }

        Ok(self.buffer)
    }

    /// Read a single u8 value from the device.
    fn get_u8(&mut self, cmd: u8, channel: u8) -> Result<u8, DeviceError> {
        let buf = self.send_cmd(cmd, channel, 0, 0)?;
        Ok(buf[HID_DATA + 1])
    }

    /// Read a little-endian u16 value from the device.
    fn get_u16(&mut self, cmd: u8, channel: u8) -> Result<u16, DeviceError> {
        let buf = self.send_cmd(cmd, channel, 0, 0)?;
        let lo = buf[HID_DATA + 1] as u16;
        let hi = buf[HID_DATA + 2] as u16;
        Ok(lo | (hi << 8))
    }
}

// ── DeviceTrait implementation ─────────────────────────────────────────────

impl DeviceTrait for ThelioIoDevice {
    fn name(&self) -> &str {
        "System76 Thelio Io 2"
    }

    fn fan_count(&self) -> usize {
        FAN_LABELS.len()
    }

    fn fan_label(&self, channel: usize) -> Result<String, DeviceError> {
        FAN_LABELS
            .get(channel)
            .map(|&s| s.to_string())
            .ok_or(DeviceError::InvalidChannel(channel))
    }

    fn read_rpm(&mut self, channel: usize) -> Result<u32, DeviceError> {
        if channel >= self.fan_count() {
            return Err(DeviceError::InvalidChannel(channel));
        }
        let rpm = self.get_u16(CMD_FAN_TACH, channel as u8)?;
        Ok(rpm as u32)
    }

    fn read_pwm(&mut self, channel: usize) -> Result<u8, DeviceError> {
        if channel >= self.fan_count() {
            return Err(DeviceError::InvalidChannel(channel));
        }
        self.get_u8(CMD_FAN_GET, channel as u8)
    }

    fn write_pwm(&mut self, channel: usize, pwm: u8) -> Result<(), DeviceError> {
        if channel >= self.fan_count() {
            return Err(DeviceError::InvalidChannel(channel));
        }
        self.send_cmd(CMD_FAN_SET, channel as u8, pwm, 0)?;
        Ok(())
    }

    fn notify_suspend(&mut self) -> Result<(), DeviceError> {
        // data[0]=0 (mode index), data[1]=1 (suspend mode)
        self.send_cmd(CMD_LED_SET_MODE, 0, 1, 0)
            .map(|_| ())
    }

    fn notify_resume(&mut self) -> Result<(), DeviceError> {
        // data[1]=0 (normal mode)
        self.send_cmd(CMD_LED_SET_MODE, 0, 0, 0)
            .map(|_| ())
    }
}
