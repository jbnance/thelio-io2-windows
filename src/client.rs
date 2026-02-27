// src/client.rs — Command-line client for thelio-io2-daemon
//
// Usage:
//   thelio-io2-client status
//   thelio-io2-client set-pwm <channel> <0-255>
//   thelio-io2-client profile
//   thelio-io2-client set-profile <quiet|balanced|performance|manual>

use std::{
    io::{BufRead, BufReader, Write},
};

// On Windows, open the named pipe as a regular file.
use std::fs::OpenOptions;

use anyhow::{bail, Context, Result};

use serde::{Deserialize, Serialize};

const PIPE_PATH: &str = r"\\.\pipe\thelio-io2";

// ── Shared types (duplicated from daemon for single-crate build) ────────

#[derive(Debug, Serialize, Deserialize)]
enum DeviceCommand {
    ReadState,
    SetPwm { channel: usize, pwm: u8 },
    NotifySuspend,
    NotifyResume,
    SetProfile { profile: String },
    GetProfile,
}

#[derive(Debug, Serialize, Deserialize)]
struct FanChannel {
    index: usize,
    label: String,
    rpm: u32,
    pwm: u8,
}

#[derive(Debug, Serialize, Deserialize)]
struct DeviceState {
    device_name: String,
    fans: Vec<FanChannel>,
}

#[derive(Debug, Serialize, Deserialize)]
enum DeviceError {
    NotConnected,
    InvalidChannel(usize),
    InvalidPwm(u8),
    Comm(String),
    DeviceError,
    Timeout,
}

#[derive(Debug, Serialize, Deserialize)]
enum IpcResponse {
    State(DeviceState),
    Ok,
    Error(DeviceError),
    ProfileInfo {
        profile: String,
        temp_c: Option<f64>,
    },
}

// ── IPC communication ───────────────────────────────────────────────────

fn send_command(cmd: &DeviceCommand) -> Result<IpcResponse> {
    let mut pipe = OpenOptions::new()
        .read(true)
        .write(true)
        .open(PIPE_PATH)
        .with_context(|| format!("Cannot open {PIPE_PATH} — is the daemon running?"))?;

    let mut json = serde_json::to_string(cmd)?;
    json.push('\n');
    pipe.write_all(json.as_bytes())
        .context("Write to pipe failed")?;

    let mut response_line = String::new();
    BufReader::new(&mut pipe)
        .read_line(&mut response_line)
        .context("Read from pipe failed")?;

    let response: IpcResponse = serde_json::from_str(response_line.trim())
        .context("Failed to parse daemon response")?;

    Ok(response)
}

// ── Command handlers ────────────────────────────────────────────────────

fn cmd_status() -> Result<()> {
    match send_command(&DeviceCommand::ReadState)? {
        IpcResponse::State(state) => {
            println!("Device: {}", state.device_name);
            println!("{:<4}  {:<14}  {:>8}  {:>5}", "Ch", "Label", "RPM", "PWM");
            println!("{}", "-".repeat(40));
            for fan in &state.fans {
                println!(
                    "{:<4}  {:<14}  {:>8}  {:>5}  ({:.1}%)",
                    fan.index,
                    fan.label,
                    fan.rpm,
                    fan.pwm,
                    fan.pwm as f64 / 255.0 * 100.0,
                );
            }
        }
        IpcResponse::Error(e) => bail!("Daemon error: {e:?}"),
        other => bail!("Unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_set_pwm(channel: usize, pwm: u8) -> Result<()> {
    match send_command(&DeviceCommand::SetPwm { channel, pwm })? {
        IpcResponse::Ok => {
            println!(
                "PWM for channel {channel} set to {pwm} ({:.1}%)",
                pwm as f64 / 255.0 * 100.0
            );
            println!("Note: profile switched to manual mode.");
        }
        IpcResponse::Error(e) => bail!("Daemon error: {e:?}"),
        other => bail!("Unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_profile() -> Result<()> {
    match send_command(&DeviceCommand::GetProfile)? {
        IpcResponse::ProfileInfo { profile, temp_c } => {
            println!("Active profile: {profile}");
            if let Some(t) = temp_c {
                println!("CPU temperature: {:.1}°C", t);
            } else {
                println!("CPU temperature: unavailable");
            }
        }
        IpcResponse::Error(e) => bail!("Daemon error: {e:?}"),
        other => bail!("Unexpected response: {other:?}"),
    }
    Ok(())
}

fn cmd_set_profile(name: &str) -> Result<()> {
    match send_command(&DeviceCommand::SetProfile {
        profile: name.to_string(),
    })? {
        IpcResponse::ProfileInfo { profile, temp_c } => {
            println!("Profile set to: {profile}");
            if let Some(t) = temp_c {
                println!("CPU temperature: {:.1}°C", t);
            }
        }
        IpcResponse::Error(e) => bail!("Daemon error: {e:?}"),
        other => bail!("Unexpected response: {other:?}"),
    }
    Ok(())
}

// ── Entry point ─────────────────────────────────────────────────────────

fn print_usage() {
    eprintln!("System76 Io Client");
    eprintln!();
    eprintln!("Usage:");
    eprintln!("  thelio-io2-client status                              Show fan status");
    eprintln!("  thelio-io2-client set-pwm <channel> <0-255>           Set fan PWM (switches to manual)");
    eprintln!("  thelio-io2-client profile                             Show current profile & temp");
    eprintln!("  thelio-io2-client set-profile <name>                  Set profile");
    eprintln!();
    eprintln!("Profiles: quiet, balanced, performance, manual");
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    match args.get(1).map(String::as_str) {
        Some("status") => cmd_status(),
        Some("set-pwm") => {
            let channel: usize = args
                .get(2)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("Expected channel number"))?;
            let pwm: u8 = args
                .get(3)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| anyhow::anyhow!("Expected PWM value 0–255"))?;
            cmd_set_pwm(channel, pwm)
        }
        Some("profile") => cmd_profile(),
        Some("set-profile") => {
            let name = args
                .get(2)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "Expected profile name: quiet, balanced, performance, or manual"
                    )
                })?;
            cmd_set_profile(name)
        }
        _ => {
            print_usage();
            std::process::exit(1);
        }
    }
}
