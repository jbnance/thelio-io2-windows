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
│  ┌────▼─────┐ ┌──▼──────────┐ ┌▼──────────────────────┐        │
│  │   IPC    │ │   Power     │ │  Thermal Source        │        │
│  │  Server  │ │   Events    │ │  HTTP mode (LHM web    │        │
│  │ (thread) │ │ (SCM ctrl)  │ │    server + nvidia-smi)│        │
│  └──────────┘ └─────────────┘ │  or Library mode       │        │
│                                │    (lhm-helper.exe)    │        │
│                                └────────────────────────┘        │
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

### Temperature monitoring

The daemon supports two modes for reading CPU and GPU temperatures.
Both use [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor)
under the hood, which provides reliable sensor access across Intel and AMD
processors and all major GPU vendors.

#### Library mode (`--lhm-mode library`) — default

Uses the bundled `lhm-helper.exe` sidecar, which wraps the
LibreHardwareMonitorLib NuGet package directly.  **No separate LHM
installation or GUI is required** — just place `lhm-helper.exe` in the same
directory as `thelio-io2-daemon.exe` (this is the default layout in release
archives).

The daemon must run with **administrator privileges** (the same requirement
as LHM itself) so the library can access hardware sensors.

#### HTTP mode (`--lhm-mode http`)

Connects to a running LibreHardwareMonitor instance via its built-in HTTP
web server.  This is the legacy mode and is useful if you already have LHM
running or want to monitor sensors from a remote machine.

1. Download the latest release from the
   [LibreHardwareMonitor releases page](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/releases).
2. Run `LibreHardwareMonitor.exe` (no installation required).
3. Enable the web server: **Options → HTTP Server**.  The default port is
   **8085**.  You can verify it works by visiting `http://localhost:8085`
   in a browser.
4. Optionally configure LHM to start automatically with Windows
   (Options → Run On Windows Startup).

> **Note:** For AMD Ryzen processors you may also need to install the
> [PawnIO driver](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/wiki/PawnIO)
> for LHM/LibreHardwareMonitorLib to read CPU temperatures.

> **Note:** Without a working temperature source the daemon switches to
> **manual** mode — fans must be controlled explicitly via the CLI client.

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
# Build the Rust daemon and client
cargo build --release

# Build the lhm-helper sidecar (.NET 10 SDK required)
dotnet publish lhm-helper/lhm-helper.csproj `
    --configuration Release --runtime win-x64 `
    --self-contained true -p:PublishSingleFile=true `
    --output lhm-helper/publish
```

This produces:
- `target\release\thelio-io2-daemon.exe` — the Windows service
- `target\release\thelio-io2-client.exe` — the CLI client
- `lhm-helper\publish\lhm-helper.exe` — the temperature reader sidecar

---

## Installation

### Installing from a GitHub Release (recommended)

Pre-built binaries are published on the
[Releases](../../releases) page for every tagged version.

1. Download the latest `thelio-io2-windows-v*.zip` from the
   [Releases page](../../releases/latest).
2. Extract the zip to a permanent location, for example:
   ```powershell
   Expand-Archive thelio-io2-windows-v*.zip -DestinationPath "C:\Program Files\thelio-io2"
   ```
3. Open an **elevated** (Run as Administrator) PowerShell and register the
   service:
   ```powershell
   $bin = "C:\Program Files\thelio-io2\thelio-io2-daemon.exe"

   sc.exe create thelio-io2 `
       binPath= "`"$bin`"" `
       DisplayName= "System76 Thelio Io2 Fan Controller" `
       start= auto

   sc.exe description thelio-io2 "Controls fan speeds on modern System76 Thelio desktops."

   sc.exe start thelio-io2
   ```
4. Optionally, add the install directory to your `PATH` so you can run
   `thelio-io2-client` from any terminal:
   ```powershell
   [Environment]::SetEnvironmentVariable(
       "Path",
       [Environment]::GetEnvironmentVariable("Path", "Machine") + ";C:\Program Files\thelio-io2",
       "Machine"
   )
   ```

#### Upgrading

To upgrade to a new release:

```powershell
sc.exe stop thelio-io2
# Extract the new zip over the existing directory
Expand-Archive thelio-io2-windows-v*.zip -DestinationPath "C:\Program Files\thelio-io2" -Force
sc.exe start thelio-io2
```

### Installing from source

#### Prerequisites

- [Rust toolchain](https://rustup.rs/) with the `x86_64-pc-windows-msvc` target:
  ```powershell
  rustup target add x86_64-pc-windows-msvc
  ```
- [.NET 10 SDK](https://dotnet.microsoft.com/download/dotnet/10.0)

#### Build the binaries

```powershell
# Build the Rust daemon and client
cargo build --release

# Build the lhm-helper sidecar
dotnet publish lhm-helper/lhm-helper.csproj `
    --configuration Release --runtime win-x64 `
    --self-contained true -p:PublishSingleFile=true `
    --output lhm-helper/publish
```

This produces:
- `target\release\thelio-io2-daemon.exe` — the Windows service
- `target\release\thelio-io2-client.exe` — the CLI client
- `lhm-helper\publish\lhm-helper.exe` — the temperature reader sidecar

Copy `lhm-helper\publish\lhm-helper.exe` next to `thelio-io2-daemon.exe` so that
library mode can find it automatically:

```powershell
Copy-Item lhm-helper\publish\lhm-helper.exe target\release\
```

#### Register the Windows Service

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

### Library mode

In library mode (`--lhm-mode library`), the bundled `lhm-helper.exe`
sidecar accesses hardware sensors directly via LibreHardwareMonitorLib.
CPU and GPU temperatures (NVIDIA, AMD, Intel) are all read natively by
the library — no additional tools like `nvidia-smi` are needed.

### HTTP mode

In HTTP mode (`--lhm-mode http`), the daemon connects to LHM's built-in
HTTP web server and fetches the `/data.json` sensor tree every 2 seconds.

- **CPU:** Sensors with IDs containing `/cpu`, `/intelcpu`, or `/amdcpu`.
- **GPU:** Sensors with IDs containing `/gpu`, plus `nvidia-smi` as a
  supplementary source (silently skipped if not installed).

### Fallback behavior

If no temperature source is available the daemon logs a warning and falls
back to **manual** mode.  It retries every 30 seconds and automatically
restores the user's desired profile when a source becomes available.

---

## Usage — Daemon

### Command-line options

| Flag | Values | Default | Description |
|---|---|---|---|
| `--console` | *(none)* | — | Run as a foreground console process instead of a Windows service. |
| `--profile` | `quiet`, `balanced`, `performance`, `manual` | `balanced` | Initial fan control profile. |
| `--log-level` | `error`, `warn`, `info`, `debug`, `trace` | `info` | Log verbosity. Use `debug` to see per-poll temperature/PWM details. |
| `--lhm-mode` | `http`, `library` | `library` | Temperature backend. `library` uses the bundled `lhm-helper.exe`; `http` connects to LHM's web server. |
| `--lhm-helper-path` | file path | `lhm-helper.exe` in daemon dir | Path to `lhm-helper.exe` (only used in library mode). |
| `--lhm-url` | URL (scheme://host:port) | `http://localhost:8085` | LibreHardwareMonitor web server URL (only used in http mode). |
| `--lhm-user` | username | *(none)* | HTTP Basic Auth username for LHM (only used in http mode). |
| `--lhm-password` | password | *(none)* | HTTP Basic Auth password for LHM (only used in http mode). |

### Console mode (development / debugging)

```powershell
# Run with the default balanced profile (HTTP mode)
thelio-io2-daemon.exe --console

# Run with the library backend (no LHM GUI needed)
thelio-io2-daemon.exe --console --lhm-mode library

# Run with a specific profile
thelio-io2-daemon.exe --console --profile performance

# Run with debug logging to see every temperature poll
thelio-io2-daemon.exe --console --log-level debug
```

### Service mode

When installed as a Windows service the daemon starts automatically.  The
`--profile` and `--log-level` flags can be passed via the service `binPath`
(see Installation).

**Logging:** In service mode, logs are written to the **Windows Event Log**
instead of stdout.  To view them, open **Event Viewer → Windows Logs →
Application** and filter by source **thelio-io2**.

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

## Troubleshooting

### Daemon says "Cannot reach LibreHardwareMonitor web server" (HTTP mode)

1. Verify LHM is running — look for `LibreHardwareMonitor.exe` in Task Manager.
2. Ensure the HTTP server is enabled: **Options → HTTP Server** (port 8085).
3. Test manually: open `http://localhost:8085/data.json` in a browser.
4. If using a non-default port or remote host, pass `--lhm-url` to the daemon.
5. Consider switching to `--lhm-mode library` to avoid needing the LHM GUI entirely.

### Daemon says "Failed to start lhm-helper" (Library mode)

1. Verify `lhm-helper.exe` exists in the same directory as `thelio-io2-daemon.exe`.
2. Ensure the daemon is running as **administrator** (required for hardware access).
3. If `lhm-helper.exe` is in a different location, pass `--lhm-helper-path`.

### CPU temperature shows "n/a"

- **AMD Ryzen:** LHM may need the
  [PawnIO driver](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor/wiki/PawnIO)
  to access CPU temperature registers.
- **Intel:** ensure LHM lists CPU temperature sensors in its main window.
- Run the daemon with `--log-level debug` and look for `LHM CPU sensor:` lines
  in the output (console mode) or Event Viewer (service mode).

### GPU temperature shows "n/a"

- Confirm LHM shows GPU temperature sensors.
- For NVIDIA GPUs, ensure `nvidia-smi` is on the PATH and working:
  `nvidia-smi --query-gpu=temperature.gpu --format=csv,noheader,nounits`
- The daemon uses both LHM and nvidia-smi; if either reports a value it will
  be used.

### No logs visible in Event Viewer (service mode)

- Open **Event Viewer → Windows Logs → Application**.
- Filter by source: **thelio-io2**.
- If no entries appear, try restarting the service:
  `sc.exe stop thelio-io2 && sc.exe start thelio-io2`

### Device shows "not connected"

- Verify the Thelio Io 2 is listed in Device Manager under
  **Human Interface Devices** (vendor `3384`, product `000B`).
- Try unplugging and re-plugging the internal USB header cable.
- The daemon retries every 5 seconds automatically.

### Client says "Cannot open pipe — is the daemon running?"

- Verify the service is running: `sc.exe query thelio-io2`
- In console mode, ensure only one instance is running (only one process
  can own the named pipe).

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
| `/sys/class/thermal/` | LibreHardwareMonitorLib (library mode) or LHM HTTP API + nvidia-smi (http mode) |

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
