// src/power.rs — Power Event Types
//
// Defines the PowerEvent enum forwarded to the device loop when the system
// suspends or resumes.
//
// Actual event delivery is handled differently depending on run mode:
//   - Service mode:  the SCM delivers SERVICE_CONTROL_POWEREVENT to our
//                    service control handler in main.rs, which sends on this
//                    channel.  No extra Win32 registration is needed.
//   - Console mode:  power events are not monitored (development mode only).

/// Events forwarded from the Windows power subsystem to the device loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerEvent {
    Suspending,
    Resumed,
}
