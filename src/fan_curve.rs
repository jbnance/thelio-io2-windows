// src/fan_curve.rs — Fan Curve Profiles
//
// Defines power profiles (Quiet, Balanced, Performance) that map CPU
// temperature to fan PWM duty cycle.  Curves are defined as sorted lists
// of (temperature_celsius, duty_percent) points with linear interpolation
// between them.
//
// The curves are ported from the system76-power Linux utility's fan control
// (src/fan.rs) for Thelio desktops.  Temperatures and duty values have been
// converted from the original hundredths-of-a-degree / hundredths-of-a-percent
// integer representation to floating-point for clarity.

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
// Ported from system76-power (src/fan.rs).  The original code uses integer
// hundredths (e.g. 4500 = 45.00 °C, 3000 = 30.00%); we store the same
// values as f64 for readability.
//
// Balanced ("standard" in system76-power):
//   Fans stay off below 45 °C, then ramp 30% → 100% over 45–88 °C.
//
// Quiet:
//   Fans stay off below 50 °C, then ramp 25% → 100% over 50–90 °C.
//
// Performance:
//   Fans stay off below 40 °C, then ramp 30% → 100% over 40–85 °C.

static BALANCED_POINTS: &[CurvePoint] = &[
    CurvePoint { temp_c: 44.99, duty_pct: 0.0 },
    CurvePoint { temp_c: 45.0,  duty_pct: 30.0 },
    CurvePoint { temp_c: 55.0,  duty_pct: 35.0 },
    CurvePoint { temp_c: 65.0,  duty_pct: 40.0 },
    CurvePoint { temp_c: 75.0,  duty_pct: 50.0 },
    CurvePoint { temp_c: 78.0,  duty_pct: 60.0 },
    CurvePoint { temp_c: 81.0,  duty_pct: 70.0 },
    CurvePoint { temp_c: 84.0,  duty_pct: 80.0 },
    CurvePoint { temp_c: 86.0,  duty_pct: 90.0 },
    CurvePoint { temp_c: 88.0,  duty_pct: 100.0 },
];

static QUIET_POINTS: &[CurvePoint] = &[
    CurvePoint { temp_c: 49.99, duty_pct: 0.0 },
    CurvePoint { temp_c: 50.0,  duty_pct: 25.0 },
    CurvePoint { temp_c: 60.0,  duty_pct: 30.0 },
    CurvePoint { temp_c: 70.0,  duty_pct: 40.0 },
    CurvePoint { temp_c: 78.0,  duty_pct: 55.0 },
    CurvePoint { temp_c: 82.0,  duty_pct: 70.0 },
    CurvePoint { temp_c: 86.0,  duty_pct: 85.0 },
    CurvePoint { temp_c: 90.0,  duty_pct: 100.0 },
];

static PERFORMANCE_POINTS: &[CurvePoint] = &[
    CurvePoint { temp_c: 39.99, duty_pct: 0.0 },
    CurvePoint { temp_c: 40.0,  duty_pct: 30.0 },
    CurvePoint { temp_c: 50.0,  duty_pct: 40.0 },
    CurvePoint { temp_c: 60.0,  duty_pct: 55.0 },
    CurvePoint { temp_c: 70.0,  duty_pct: 68.0 },
    CurvePoint { temp_c: 75.0,  duty_pct: 78.0 },
    CurvePoint { temp_c: 80.0,  duty_pct: 90.0 },
    CurvePoint { temp_c: 85.0,  duty_pct: 100.0 },
];

static BALANCED_CURVE: FanCurve = FanCurve { points: BALANCED_POINTS };
static QUIET_CURVE: FanCurve = FanCurve { points: QUIET_POINTS };
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

// ── Tests ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn below_curve_returns_fans_off() {
        let curve = Profile::Balanced.curve().unwrap();
        // Below 44.99 °C → fans off (0%)
        assert_eq!(curve.duty_pwm(0.0), 0);
        assert_eq!(curve.duty_pwm(30.0), 0);
        assert_eq!(curve.duty_pwm(44.0), 0);
    }

    #[test]
    fn above_curve_returns_full_speed() {
        let curve = Profile::Balanced.curve().unwrap();
        assert_eq!(curve.duty_pwm(88.0), 255);
        assert_eq!(curve.duty_pwm(100.0), 255);
    }

    #[test]
    fn exact_point_matches() {
        let curve = Profile::Balanced.curve().unwrap();
        // 45.0 °C → 30%
        let pct = curve.duty_percent(45.0);
        assert!((pct - 30.0).abs() < 0.01, "expected 30%, got {pct}");
        // 88.0 °C → 100%
        let pct = curve.duty_percent(88.0);
        assert!((pct - 100.0).abs() < 0.01, "expected 100%, got {pct}");
    }

    #[test]
    fn interpolates_between_points() {
        let curve = Profile::Balanced.curve().unwrap();
        // Between 45.0 °C (30%) and 55.0 °C (35%) → midpoint 50.0 ≈ 32.5%
        let pct = curve.duty_percent(50.0);
        assert!(pct > 30.0 && pct < 35.0, "expected ~32.5%, got {pct}");
    }

    #[test]
    fn duty_to_pwm_bounds() {
        // 0% → PWM 0
        let curve = Profile::Balanced.curve().unwrap();
        assert_eq!(curve.duty_pwm(0.0), 0);
        // 100% → PWM 255
        assert_eq!(curve.duty_pwm(88.0), 255);
    }

    #[test]
    fn duty_to_pwm_midpoint() {
        // 50% duty → PWM ~128
        let curve = Profile::Balanced.curve().unwrap();
        let pwm = curve.duty_pwm(75.0); // 75 °C = 50%
        assert!(pwm >= 126 && pwm <= 129, "expected ~128, got {pwm}");
    }

    #[test]
    fn quiet_fans_off_below_threshold() {
        let curve = Profile::Quiet.curve().unwrap();
        assert_eq!(curve.duty_pwm(40.0), 0);
        assert_eq!(curve.duty_pwm(49.0), 0);
    }

    #[test]
    fn performance_spins_up_earliest() {
        let curve = Profile::Performance.curve().unwrap();
        // At 40 °C, performance should already be at 30%
        let pct = curve.duty_percent(40.0);
        assert!((pct - 30.0).abs() < 0.01, "expected 30%, got {pct}");
        // But quiet is still at 0% at 40 °C
        let quiet = Profile::Quiet.curve().unwrap();
        assert_eq!(quiet.duty_pwm(40.0), 0);
    }

    #[test]
    fn manual_has_no_curve() {
        assert!(Profile::Manual.curve().is_none());
    }

    #[test]
    fn hysteresis_suppresses_small_changes() {
        let mut hyst = TempHysteresis::new(2.0);
        assert_eq!(hyst.update(50.0), 50.0); // first reading
        assert_eq!(hyst.update(51.0), 50.0); // within band, returns old
        assert_eq!(hyst.update(50.5), 50.0); // still within band
        assert_eq!(hyst.update(52.0), 52.0); // crosses threshold, updates
        assert_eq!(hyst.update(51.0), 52.0); // within band of new value
    }

    #[test]
    fn profile_display_roundtrip() {
        for p in &[Profile::Quiet, Profile::Balanced, Profile::Performance, Profile::Manual] {
            let s = p.to_string();
            let parsed = Profile::from_str_loose(&s).unwrap();
            assert_eq!(*p, parsed);
        }
    }
}
