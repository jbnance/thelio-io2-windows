// src/fan_curve.rs — Fan Curve Profiles
//
// Defines power profiles (Quiet, Balanced, Performance) that map CPU
// temperature to fan PWM duty cycle.  Curves are defined as sorted lists
// of (temperature_celsius, duty_percent) points with linear interpolation
// between them.
//
// The curves are modeled after the system76-power Linux utility's thermal
// management behavior for Thelio desktops.

use serde::{Deserialize, Serialize};
use std::fmt;

// ── Profile enum ─────────────────────────────────────────────────────────

/// Power profile that determines the fan speed curve.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Profile {
    Quiet,
    Balanced,
    Performance,
    Manual,
}

impl Profile {
    /// Parse a profile name (case-insensitive).
    pub fn from_str_loose(s: &str) -> Option<Profile> {
        match s.to_lowercase().as_str() {
            "quiet" => Some(Profile::Quiet),
            "balanced" => Some(Profile::Balanced),
            "performance" => Some(Profile::Performance),
            "manual" => Some(Profile::Manual),
            _ => None,
        }
    }

    /// Return the fan curve for this profile, or `None` for Manual.
    pub fn curve(&self) -> Option<&'static FanCurve> {
        match self {
            Profile::Quiet => Some(&QUIET_CURVE),
            Profile::Balanced => Some(&BALANCED_CURVE),
            Profile::Performance => Some(&PERFORMANCE_CURVE),
            Profile::Manual => None,
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Profile::Quiet => write!(f, "quiet"),
            Profile::Balanced => write!(f, "balanced"),
            Profile::Performance => write!(f, "performance"),
            Profile::Manual => write!(f, "manual"),
        }
    }
}

// ── Fan curve definition ─────────────────────────────────────────────────

/// A single point on a fan curve: (temperature in °C, duty cycle 0–100%).
#[derive(Debug, Clone, Copy)]
pub struct CurvePoint {
    pub temp_c: f64,
    pub duty_pct: f64,
}

/// A fan curve: a sorted array of (temp, duty%) points.
/// Temperatures below the first point clamp to its duty; temperatures above
/// the last point clamp to its duty.  Between points, linear interpolation.
pub struct FanCurve {
    pub points: &'static [CurvePoint],
}

impl FanCurve {
    /// Evaluate the curve at a given temperature, returning duty as 0–100%.
    pub fn duty_percent(&self, temp_c: f64) -> f64 {
        let pts = self.points;
        if pts.is_empty() {
            return 100.0; // safety: full speed if no curve
        }

        // Clamp below first point
        if temp_c <= pts[0].temp_c {
            return pts[0].duty_pct;
        }

        // Clamp above last point
        if temp_c >= pts[pts.len() - 1].temp_c {
            return pts[pts.len() - 1].duty_pct;
        }

        // Linear interpolation between surrounding points
        for i in 0..pts.len() - 1 {
            let lo = &pts[i];
            let hi = &pts[i + 1];
            if temp_c >= lo.temp_c && temp_c <= hi.temp_c {
                let t = (temp_c - lo.temp_c) / (hi.temp_c - lo.temp_c);
                return lo.duty_pct + t * (hi.duty_pct - lo.duty_pct);
            }
        }

        // Should not reach here, but safety fallback
        100.0
    }

    /// Evaluate the curve and return a PWM value 0–255.
    pub fn duty_pwm(&self, temp_c: f64) -> u8 {
        let pct = self.duty_percent(temp_c);
        let pwm = (pct / 100.0 * 255.0).round() as u16;
        pwm.min(255) as u8
    }
}

// ── Curve data ───────────────────────────────────────────────────────────
//
// Modeled after system76-power's thermal management for Thelio desktops.
//
// Quiet:       keeps fans low until temps get quite high
// Balanced:    moderate ramp, good default
// Performance: aggressive cooling, fans spin up early

static QUIET_POINTS: &[CurvePoint] = &[
    CurvePoint { temp_c: 0.0,  duty_pct: 25.0 },
    CurvePoint { temp_c: 45.0, duty_pct: 25.0 },
    CurvePoint { temp_c: 55.0, duty_pct: 35.0 },
    CurvePoint { temp_c: 65.0, duty_pct: 50.0 },
    CurvePoint { temp_c: 75.0, duty_pct: 75.0 },
    CurvePoint { temp_c: 85.0, duty_pct: 100.0 },
];

static BALANCED_POINTS: &[CurvePoint] = &[
    CurvePoint { temp_c: 0.0,  duty_pct: 30.0 },
    CurvePoint { temp_c: 40.0, duty_pct: 30.0 },
    CurvePoint { temp_c: 50.0, duty_pct: 35.0 },
    CurvePoint { temp_c: 60.0, duty_pct: 50.0 },
    CurvePoint { temp_c: 70.0, duty_pct: 75.0 },
    CurvePoint { temp_c: 80.0, duty_pct: 100.0 },
];

static PERFORMANCE_POINTS: &[CurvePoint] = &[
    CurvePoint { temp_c: 0.0,  duty_pct: 40.0 },
    CurvePoint { temp_c: 35.0, duty_pct: 40.0 },
    CurvePoint { temp_c: 45.0, duty_pct: 50.0 },
    CurvePoint { temp_c: 55.0, duty_pct: 65.0 },
    CurvePoint { temp_c: 65.0, duty_pct: 85.0 },
    CurvePoint { temp_c: 75.0, duty_pct: 100.0 },
];

static QUIET_CURVE: FanCurve = FanCurve { points: QUIET_POINTS };
static BALANCED_CURVE: FanCurve = FanCurve { points: BALANCED_POINTS };
static PERFORMANCE_CURVE: FanCurve = FanCurve { points: PERFORMANCE_POINTS };

// ── Hysteresis helper ────────────────────────────────────────────────────

/// Tracks temperature with hysteresis to avoid fan speed oscillation.
/// The effective temperature only changes when the real temperature moves
/// more than `hysteresis` degrees away from the last committed value.
pub struct TempHysteresis {
    last_temp: Option<f64>,
    hysteresis: f64,
}

impl TempHysteresis {
    pub fn new(hysteresis: f64) -> Self {
        Self {
            last_temp: None,
            hysteresis,
        }
    }

    /// Feed a new temperature reading.  Returns the effective temperature
    /// to use for curve evaluation (may be the previous value if the change
    /// is within the hysteresis band).
    pub fn update(&mut self, temp_c: f64) -> f64 {
        match self.last_temp {
            None => {
                self.last_temp = Some(temp_c);
                temp_c
            }
            Some(prev) => {
                if (temp_c - prev).abs() >= self.hysteresis {
                    self.last_temp = Some(temp_c);
                    temp_c
                } else {
                    prev
                }
            }
        }
    }
}
