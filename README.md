# thelio-io2-windows

A Windows userspace daemon (service) written in Rust that replaces the Linux
`system76-io-dkms` kernel driver for the **System76 Thelio Io 2**, enabling
fan monitoring and control on System76 Thelio desktop computers running Windows.

**Supported device:** System76 Thelio Io 2 — USB HID `3384:000B`

---

## Architecture

```
┌─────────────────────────────────────────────────────────┐
│              Windows Service (thelio-io2)               │
│                                                         │
│  ┌─────────────────────────────────────────────────┐   │
│  │               Device Loop (main thread)          │   │
│  │  - Auto-detects & opens the Thelio Io 2          │   │
│  │  - Handles IPC requests from named pipe clients  │   │
│  │  - Handles suspend/resume power events           │   │
│  │  - Auto-reconnects if the device is unplugged    │   │
│  └──────────────────┬──────────────────────────────┘   │
│                     │                                   │
│          ┌──────────┴──────────┐                        │
│          │                     │                        │
│  ┌───────▼──────┐    ┌────────▼────────┐               │
│  │  IPC Server  │    │  Power Events   │               │
│  │ (own thread) │    │  (SCM control   │               │
│  │ named pipe   │    │   handler)      │               │
│  └──────────────┘    └─────────────────┘               │
└─────────────────────────────────────────────────────────┘
         ▲ named pipe: \\.\pipe\thelio-io2
         │
┌────────┴────────┐
│  CLI Client     │   thelio-io2-client status
│  or any app     │   thelio-io2-client set-pwm 0 128
└─────────────────┘
```

---

## Prerequisites

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

### Remove the Service

```powershell
sc.exe stop thelio-io2
sc.exe delete thelio-io2
```

---

## Usage — CLI Client

```powershell
# Show current fan status
thelio-io2-client status

# Set channel 0 (CPU fan) to 50% duty cycle (128/255)
thelio-io2-client set-pwm 0 128

# Full speed
thelio-io2-client set-pwm 0 255
```

Example output:
```
Device: System76 Thelio Io 2
Ch    Label           RPM    PWM
----------------------------------------
0     CPU Fan        1200     128  (50.2%)
1     Intake Fan      960      96  (37.6%)
2     GPU Fan        1440     160  (62.7%)
3     Aux Fan           0       0  (0.0%)
```

---

## IPC Protocol

The named pipe `\\.\pipe\thelio-io2` accepts newline-delimited JSON.

### Commands (client → daemon)

```jsonc
// Read all fan channels
{"ReadState": null}

// Set PWM duty cycle (channel: 0-based, pwm: 0–255)
{"SetPwm": {"channel": 0, "pwm": 200}}

// Signal suspend / resume (sent automatically via Windows power events)
{"NotifySuspend": null}
{"NotifyResume": null}
```

### Responses (daemon → client)

```jsonc
// Fan state
{"State": {"device_name": "System76 Thelio Io 2", "fans": [...]}}

// Success
"Ok"

// Error
{"Error": "NotConnected"}
{"Error": {"InvalidChannel": 5}}
{"Error": {"Comm": "HID write failed"}}
```

---

## Development / Debugging

Run as a console process without registering a service:

```powershell
thelio-io2-daemon.exe --console
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

---

## License

GPL-2.0, matching the original Linux driver.
