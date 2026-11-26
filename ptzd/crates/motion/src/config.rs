use std::path::PathBuf;
use std::time::Duration;

use ptzd_core::RawSpeed;

/// Field of view of one lens in degrees.
#[derive(Clone, Copy, Debug)]
pub struct Fov {
    pub h: f64,
    pub v: f64,
}

#[derive(Clone, Debug)]
pub struct MotionConfig {
    /// Holds `pos`, `presets.json`, `clean` (plan §4.3, §4.4, §7a).
    pub state_dir: PathBuf,
    // Measured 2026-11-25 (T4): image shift per step, both lenses.
    pub wide: Fov,
    pub tele: Fov,
    // Measured 2026-11-25 (T4): tilt 11.27/11.29 (tele/wide fit), pan 8.1 to
    // 8.3, so the pan range is about 330 degrees, not 360.
    pub pan_steps_per_deg: f64,
    pub tilt_steps_per_deg: f64,
    /// Raw tilt step at which the lens is level with the pan plane. Measured
    /// 2026-11-25: 2.0 to 2.8 degrees below step 0.
    pub tilt_level_step: f64,
    // Measured 2026-11-25 (T6): raw tilt + is image down, raw pan + is right.
    pub tilt_reversed: bool,
    pub pan_reversed: bool,
    // UNPROVEN (T5): raw speed for Frigate's speed 1. >= 48 is class 0 (ramps),
    // plan §2.2. Real steps/s per class unknown.
    pub fast_speed: RawSpeed,
    // UNPROVEN (T5): "mid class" for ContinuousMove (plan §4.3). 32..47 = class 4.
    pub mid_speed: RawSpeed,
    // UNPROVEN (T12): is Reolink LimitLeft the low step end? Set only the left
    // limit in the app, compare its value with GET_POS at the left end.
    pub pan_left_is_low: bool,
    /// Soft margin from hw min/max, steps. Plan §4.3 says start at 20; UNPROVEN (T3).
    pub soft_margin: u32,
    /// Plan §4.3: Frigate's 0.1 s intercept bound.
    pub dwell: Duration,
    /// Plan §4.3: GET_STATE cadence while moving.
    pub poll: Duration,
    /// Worker wake-up when idle (foreign-motion and re-home checks).
    pub idle_tick: Duration,
    /// Unused since W3 (position is saved only at shutdown); kept so
    /// callers still compile.
    pub persist_debounce: Duration,
    /// MOVING stays on this long after both axes go idle.
    // UNPROVEN (T5): mechanical settle; also keeps the Frigate fit intercept >= 0.1 s
    pub settle: Duration,
    /// Per speed class (<=15, 16..31, 32..47, >=48): nominal steps/s, one
    /// step per 1.4 ms timer tick divided by the plan §2.2 dividers 8/4/2/1
    /// (hw doc §3, start_timer 0x2264). UNPROVEN (T5); class 0 ramps.
    pub step_rates: [f64; 4],
    /// Move deadline = distance / (rate / slack) + margin (R8). Policy. The
    /// rates are UNPROVEN (T5), so the deadline errs long: 3x the computed
    /// travel time. Too short would STOP a healthy move at full speed.
    pub move_slack: f64,
    pub move_margin: Duration,
    /// How long home() waits for `device`'s own homing/jog to finish (W1):
    /// the longest stretch the head may stay busy before homing fails.
    pub home_idle_timeout: Duration,
    /// Boot order (review 2 B2): wait this long for `device` to calibrate
    /// the motors before ptzd INIT_POSes or AUTOTESTs itself.
    // UNPROVEN (T14): how soon after runsvdir `device` starts its boot homing.
    pub device_home_wait: Duration,
    /// Adopting a calibrated position needs this many consecutive idle,
    /// unchanged GET_POS polls, `adopt_stable_interval` apart (about 2 s), so
    /// a late `device` INIT_POS or goto is not adopted mid-way.
    // UNPROVEN (T14): whether `device` moves or INIT_POSes after calibrating.
    pub adopt_stable_polls: u32,
    pub adopt_stable_interval: Duration,
    /// ContinuousMove without Timeout: stop after this (policy, not hardware).
    pub continuous_timeout: Duration,
    /// Homing wait limit per axis (policy; full-span seek time UNPROVEN).
    pub home_timeout: Duration,
    /// Daily re-home hour, UTC. None = off (plan §4.3).
    pub rehome_hour_utc: Option<u32>,
    /// Re-home only after this long without any command.
    pub rehome_idle: Duration,
    /// SET_BACKLASH values, same as `device` (plan §2.2 table).
    pub backlash_pan: u32,
    pub backlash_tilt: u32,
}

/// Plan §2.2: `device` sets backlash pan 0, tilt 16. Anything above this cap
/// is refused (review 2 R4): backlash steps are not counted in `cur`.
pub const MAX_BACKLASH: u32 = 64;

impl MotionConfig {
    pub fn new(state_dir: impl Into<PathBuf>) -> Self {
        MotionConfig {
            state_dir: state_dir.into(),
            wide: Fov { h: 104.0, v: 57.3 },
            tele: Fov { h: 39.7, v: 22.7 },
            pan_steps_per_deg: 8.2,
            tilt_steps_per_deg: 11.28,
            tilt_level_step: 28.0,
            tilt_reversed: true,
            pan_reversed: false,
            fast_speed: 48,
            mid_speed: 32,
            pan_left_is_low: true,
            soft_margin: 20,
            dwell: Duration::from_millis(150),
            poll: Duration::from_millis(10),
            idle_tick: Duration::from_millis(500),
            persist_debounce: Duration::from_secs(5),
            settle: Duration::from_millis(150),
            step_rates: [1e9 / 1.4e6 / 8.0, 1e9 / 1.4e6 / 4.0, 1e9 / 1.4e6 / 2.0, 1e9 / 1.4e6],
            move_slack: 3.0,
            move_margin: Duration::from_secs(2),
            home_idle_timeout: Duration::from_secs(120),
            device_home_wait: Duration::from_secs(180),
            adopt_stable_polls: 5,
            adopt_stable_interval: Duration::from_millis(400),
            continuous_timeout: Duration::from_secs(30),
            home_timeout: Duration::from_secs(120),
            rehome_hour_utc: None,
            rehome_idle: Duration::from_secs(10 * 60),
            backlash_pan: 0,
            backlash_tilt: 16,
        }
    }
}

fn pos_finite(name: &str, v: f64) -> Result<(), String> {
    if v.is_finite() && v > 0.0 {
        Ok(())
    } else {
        Err(format!("{name} must be finite and > 0, got {v}"))
    }
}

impl MotionConfig {
    /// Rejects values that make tracking diverge or drive the head wrongly
    /// (review 2 W3, R4, M4).
    pub fn validate(&self) -> Result<(), String> {
        for (n, v) in [
            ("wide_fov_h", self.wide.h),
            ("wide_fov_v", self.wide.v),
            ("tele_fov_h", self.tele.h),
            ("tele_fov_v", self.tele.v),
        ] {
            pos_finite(n, v)?;
            if v > 180.0 {
                return Err(format!("{n} must be <= 180 degrees, got {v}"));
            }
        }
        pos_finite("pan_steps_per_deg", self.pan_steps_per_deg)?;
        pos_finite("tilt_steps_per_deg", self.tilt_steps_per_deg)?;
        if !self.tilt_level_step.is_finite() {
            return Err(format!("tilt_level_step must be finite, got {}", self.tilt_level_step));
        }
        for r in self.step_rates {
            pos_finite("step_rates", r)?;
        }
        pos_finite("move_slack", self.move_slack)?;
        if self.move_margin.is_zero() {
            return Err("move_margin_secs must be > 0".into());
        }
        // Largest usable half: min(total) 1025 steps; margin must leave a range.
        if self.soft_margin == 0 || self.soft_margin >= 512 {
            return Err(format!("soft_margin must be 1..511 steps, got {}", self.soft_margin));
        }
        for (n, v) in [("backlash_pan", self.backlash_pan), ("backlash_tilt", self.backlash_tilt)] {
            if v > MAX_BACKLASH {
                return Err(format!("{n} must be <= {MAX_BACKLASH} (device uses 0/16), got {v}"));
            }
        }
        if self.poll.is_zero() || self.idle_tick.is_zero() {
            return Err("poll_secs and idle_tick_secs must be > 0".into());
        }
        if self.adopt_stable_polls == 0 {
            return Err("adopt_stable_polls must be >= 1".into());
        }
        Ok(())
    }
}
