# thelio-io2-windows

A Windows userspace daemon (service) written in Rust that replaces the Linux
`system76-io-dkms` kernel driver for the **System76 Thelio Io 2**, enabling
fan monitoring, control, and automatic temperature-based fan speed management
on System76 Thelio desktop computers running Windows.

**Supported device:** System76 Thelio Io 2 — USB HID `3384:000B`

---

## Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│                  Windows Service (thelio-io2)                    │
│                                                                 │
│  ┌───────────────────────────────────────────────────────────┐  │
│  │                 Device Loop (main thread)                  │  │
│  │  - Auto-detects & opens the Thelio Io 2                    │  │
│  │  - Handles IPC requests from named pipe clients            │  │
│  │  - Handles suspend/resume power events                     │  │
│  │  - Polls temperature & applies fan curves every 2 seconds  │  │
│  │  - Auto-reconnects if the device is unplugged              │  │
│  └──────────────┬────────────────────────────────────────────┘  │
│                  │                                               │
│       ┌──────────┼──────────────┐                                │
│       │          │              │                                │
│  ┌────▼─────┐ ┌──▼──────────┐ ┌▼───────────────┐               │
│  │   IPC    │ │   Power     │ │ Thermal Reader  │               │
│  │  Server  │ │   Events    │ │ (LHM WMI +      │               │
│  │ (thread) │ │ (SCM ctrl)  │ │  nvidia-smi)    │               │
│  └──────────┘ └─────────────┘ └─────────────────┘               │
└─────────────────────────────────────────────────────────────────┘
         ▲ named pipe: \\.\pipe\thelio-io2
         │
┌────────┴────────┐
│  CLI Client     │   thelio-io2-client status
│  or any app     │   thelio-io2-client set-profile balanced
└─────────────────┘
```

---

## Prerequisites

### LibreHardwareMonitor (required for temperature monitoring)

[LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor)
must be installed and **running** for the daemon to read CPU and GPU
temperatures.  It exposes sensor data via WMI, which is the only reliable
way to read CPU die temperature on Windows across both Intel and AMD
processors.

1. Download the latest release from the
   [LibreHardwareMonitor releases page](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/releases).
2. Run `LibreHardwareMonitor.exe` (no installation required).
3. Optionally configure LHM to start automatically with Windows
   (Options → Run On Windows Startup).

> **Note:** Without LibreHardwareMonitor the daemon falls back to native
> WMI thermal sources (ACPI thermal zones), which do not work on many
> systems — particularly AMD Ryzen.  If no temperature source is available
> the daemon switches to **manual** mode and fans must be controlled
> explicitly.

### Thelio Io 2

The Thelio Io 2 uses standard USB HID and requires no additional drivers on
Windows — it works out of the box.

### Rust toolchain (to build from source)

```
rustup target add x86_64-pc-windows-msvc
```

---

## Building

```powershell
cargo build --release
```

This produces:
- `target\release\thelio-io2-daemon.exe` — the Windows service
- `target\release\thelio-io2-client.exe` — the CLI client

---

## Installation

### Register the Windows Service

Run the following from an **elevated** PowerShell:

```powershell
$bin = "$PWD\target\release\thelio-io2-daemon.exe"

sc.exe create thelio-io2 `
    binPath= "$bin" `
    DisplayName= "System76 Thelio Io2 Fan Controller" `
    start= auto

sc.exe description thelio-io2 "Controls fan speeds on modern System76 Thelio desktops."

sc.exe start thelio-io2
```

The service starts with the **balanced** profile by default.  To start with a
different profile or log level, include the flags in the binary path:

```powershell
$bin = "$PWD\target\release\thelio-io2-daemon.exe --profile performance --log-level debug"

sc.exe create thelio-io2 `
    binPath= "$bin" `
    DisplayName= "System76 Thelio Io2 Fan Controller" `
    start= auto
```

### Remove the Service

```powershell
sc.exe stop thelio-io2
sc.exe delete thelio-io2
```

---

## Power Profiles

The daemon supports four fan control profiles, selectable at startup or at
runtime via the CLI client.

| Profile | Behavior |
|---|---|
| **quiet** | Fans stay off below 50 °C, then ramp 25 % → 100 % by 90 °C. Prioritises silence. |
| **balanced** | Fans stay off below 45 °C, then ramp 30 % → 100 % by 88 °C. Good default. |
| **performance** | Fans stay off below 40 °C, then ramp 30 % → 100 % by 85 °C. Aggressive cooling. |
| **manual** | No automatic fan control. PWM must be set explicitly via `set-pwm`. |

### Fan curve details

Each profile defines a sorted list of *(temperature, duty %)* points.  Between
points the duty cycle is linearly interpolated.  A **2 °C hysteresis band**
prevents rapid oscillation when the temperature hovers near a curve point.

All four fan channels (CPU, Intake, GPU, Aux) are set to the same duty cycle,
matching the Linux driver behaviour.

**Balanced** (default)

| Temp (°C) | Duty (%) |
|-----------|----------|
| < 45      | 0        |
| 45        | 30       |
| 55        | 35       |
| 65        | 40       |
| 75        | 50       |
| 78        | 60       |
| 81        | 70       |
| 84        | 80       |
| 86        | 90       |
| ≥ 88      | 100      |

**Quiet**

| Temp (°C) | Duty (%) |
|-----------|----------|
| < 50      | 0        |
| 50        | 25       |
| 60        | 30       |
| 70        | 40       |
| 78        | 55       |
| 82        | 70       |
| 86        | 85       |
| ≥ 90      | 100      |

**Performance**

| Temp (°C) | Duty (%) |
|-----------|----------|
| < 40      | 0        |
| 40        | 30       |
| 50        | 40       |
| 60        | 55       |
| 70        | 68       |
| 75        | 78       |
| 80        | 90       |
| ≥ 85      | 100      |

---

## Temperature Sources

The daemon reads CPU and GPU temperatures and uses the **maximum** across
all readings for fan curve evaluation (since the Thelio chassis fans cool
the entire system).  Both Intel and AMD processors are supported.

### CPU temperature

Sources are tried in priority order; the first one that returns a valid
reading wins:

| Priority | Source | Notes |
|:--------:|--------|-------|
| 1 | **LibreHardwareMonitor** WMI (`root\LibreHardwareMonitor`) | Primary source.  Reads CPU die temperature via LHM's kernel driver.  Works with any CPU vendor (Intel, AMD). |
| 2 | `MSAcpi_ThermalZoneTemperature` (`root\WMI`) | Fallback.  ACPI thermal zones.  Works on some Intel systems but often empty on AMD. |
| 3 | `Win32_PerfFormattedData_Counters_ThermalZoneInformation` (`root\CIMV2`) | Fallback.  Performance-counter thermal zones.  Reads from the same ACPI data as source 2. |

### GPU temperature

All available sources are checked and the **maximum** across every detected
GPU is used.  This handles systems with multiple GPUs (discrete + integrated,
or multi-GPU configurations):

| Source | Supported GPUs | Notes |
|--------|---------------|-------|
| **LibreHardwareMonitor** WMI | NVIDIA, AMD, Intel Arc | Identifies GPUs via `/gpu-nvidia/`, `/gpu-amd/`, `/gpu-intel/` sensor paths. |
| `nvidia-smi` CLI | NVIDIA | Returns one reading per GPU.  Supplements LHM; silently skipped if not installed. |

If no temperature source is available the daemon logs a warning and falls
back to **manual** mode.

---

## Usage — Daemon

### Command-line options

| Flag | Values | Default | Description |
|---|---|---|---|
| `--console` | *(none)* | — | Run as a foreground console process instead of a Windows service. |
| `--profile` | `quiet`, `balanced`, `performance`, `manual` | `balanced` | Initial fan control profile. |
| `--log-level` | `error`, `warn`, `info`, `debug`, `trace` | `info` | Log verbosity. Use `debug` to see per-poll temperature/PWM details. |

### Console mode (development / debugging)

```powershell
# Run with the default balanced profile
thelio-io2-daemon.exe --console

# Run with a specific profile
thelio-io2-daemon.exe --console --profile performance

# Run with debug logging to see every temperature poll
thelio-io2-daemon.exe --console --log-level debug
```

### Service mode

When installed as a Windows service the daemon starts automatically.  The
`--profile` and `--log-level` flags can be passed via the service `binPath`
(see Installation).

---

## Usage — CLI Client

```powershell
# Show current fan status (RPM, PWM, duty %)
thelio-io2-client status

# Show the active profile and current temperature
thelio-io2-client profile

# Switch to a different profile at runtime
thelio-io2-client set-profile quiet
thelio-io2-client set-profile balanced
thelio-io2-client set-profile performance
thelio-io2-client set-profile manual

# Manually set a fan channel (switches to manual mode)
thelio-io2-client set-pwm 0 128      # channel 0, 50% duty
thelio-io2-client set-pwm 0 255      # channel 0, full speed
```

> **Note:** Using `set-pwm` automatically switches the daemon to the
> **manual** profile.  Use `set-profile` to re-enable automatic fan control.

### Example output

```
> thelio-io2-client status
Device: System76 Thelio Io 2
Ch    Label           RPM    PWM
----------------------------------------
0     CPU Fan        1200     128  (50.2%)
1     Intake Fan      960      96  (37.6%)
2     GPU Fan        1440     160  (62.7%)
3     Aux Fan           0       0  (0.0%)

> thelio-io2-client profile
Active profile: balanced
CPU temperature: 62.5°C
```

---

## IPC Protocol

The named pipe `\\.\pipe\thelio-io2` accepts newline-delimited JSON.

### Commands (client → daemon)

```jsonc
// Read all fan channels
"ReadState"

// Set PWM duty cycle (channel: 0-based, pwm: 0–255)
// NOTE: switches daemon to manual profile
{"SetPwm": {"channel": 0, "pwm": 200}}

// Set the active power profile
{"SetProfile": {"profile": "balanced"}}

// Query the current profile and temperature
"GetProfile"

// Signal suspend / resume (sent automatically via Windows power events)
"NotifySuspend"
"NotifyResume"
```

### Responses (daemon → client)

```jsonc
// Fan state
{"State": {"device_name": "System76 Thelio Io 2", "fans": [...]}}

// Profile info (returned by GetProfile and SetProfile)
{"ProfileInfo": {"profile": "balanced", "temp_c": 62.5}}

// Success
"Ok"

// Error
{"Error": "NotConnected"}
{"Error": {"InvalidChannel": 5}}
{"Error": {"Comm": "HID write failed"}}
```

---

## Mapping from Linux Driver to Windows Daemon

| Linux concept | Windows daemon equivalent |
|---|---|
| `hwmon` sysfs (`fan1_input`, `pwm1`, …) | Named pipe JSON IPC |
| `hid_hw_output_report` | `hidapi::HidDevice::write` |
| `raw_event` / `wait_for_completion` | `hidapi::HidDevice::read_timeout` |
| `PM_SUSPEND_PREPARE` notifier | SCM `ServiceControl::PowerEvent` in service control handler |
| `CMD_LED_SET_MODE` on suspend | `Device::notify_suspend` / `notify_resume` |
| DKMS module autoload | Windows service `start= auto` |
| `system76-power` profiles | `--profile` flag + `SetProfile` / `GetProfile` IPC |
| `system76-power` fan curves | `fan_curve.rs` with system76-power-compatible curves |
| `/sys/class/thermal/` | LibreHardwareMonitor WMI (+ ACPI fallback) |

---

## Acknowledgments

This project is adapted from and inspired by the following System76 open-source
projects:

- **[system76-io-dkms](https://github.com/pop-os/system76-io-dkms)** — Linux
  kernel driver for the Thelio Io board.  The USB HID protocol implementation
  (command bytes, report layout, fan channel mapping) in `thelio_io.rs` was
  ported from this driver.

- **[system76-power](https://github.com/pop-os/system76-power)** — Linux power
  management daemon.  The fan curve data (temperature-to-duty mappings for
  quiet, balanced, and performance profiles) in `fan_curve.rs` was ported from
  this utility's `src/fan.rs`.

---

## License

GPL-2.0, matching the original Linux driver.
