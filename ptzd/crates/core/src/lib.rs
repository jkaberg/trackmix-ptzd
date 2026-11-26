//! Shared types and the trait seams between crates. Every crate
//! depends on these signatures, so change them last.
//! "plan" = ptzd/docs/design.md, "hw doc" = research/ptzd-hardware.md (§ = section).

use std::fmt;
use std::io;
use std::time::Duration;

// ---------------------------------------------------------------- hw seam

/// Only the two motorised axes exist on this model (plan §2.2/§2.3).
/// Kernel motor index: pan = 3, tilt = 4. Indices 5/6 oops the kernel,
/// so no other value is representable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Axis {
    Pan,
    Tilt,
}

impl Axis {
    pub const ALL: [Axis; 2] = [Axis::Pan, Axis::Tilt];
    pub fn motor_index(self) -> u32 {
        match self {
            Axis::Pan => 3,
            Axis::Tilt => 4,
        }
    }
}

/// GET_INFO result: {motor, total, min, step}. Expected pan {2701,0,16},
/// tilt {1025,0,16} (plan §4.4 step 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MotorInfo {
    pub total: u32,
    pub min: u32,
    pub step: u32,
}

/// Driver speed class, from the `speed` field of SET_POS (plan §2):
/// speed <=15 -> class 6, 16..31 -> 5, 32..47 -> 4, >=48 -> 0 (class 0 ramps).
/// The hw layer passes the raw `speed` value through; motion picks it.
pub type RawSpeed = u32;

/// The only code that touches `/dev/lens_zfi`. Implemented by
/// `ptzd_hw::LensZfi` (real ioctls) and `ptzd_hw::SimMotor` (mock that
/// reproduces the verified driver quirks). Positions are raw driver steps.
pub trait MotorDev: Send {
    fn info(&mut self, a: Axis) -> io::Result<MotorInfo>;
    /// GET_STATE: ioctl return value 1 = moving, 0 = idle.
    fn is_moving(&mut self, a: Axis) -> io::Result<bool>;
    fn position(&mut self, a: Axis) -> io::Result<u32>;
    fn is_calibrated(&mut self, a: Axis) -> io::Result<bool>;
    /// SET_POS. Implementations MUST refuse (Err) when the axis is moving
    /// (no live retarget, review B1) or uncalibrated, and MUST clamp
    /// `pos` into the soft-margin window before the ioctl. Non-blocking.
    fn set_pos(&mut self, a: Axis, pos: u32, speed: RawSpeed) -> io::Result<()>;
    /// STOP. Driver waits <= 1 s and returns EBUSY otherwise.
    fn stop(&mut self, a: Axis) -> io::Result<()>;
    /// INIT_POS: sets position and calibrated flag, no motion.
    fn init_pos(&mut self, a: Axis, pos: u32) -> io::Result<()>;
    /// AUTOTEST (homing) for ONE motor: buffer [idx,0,0,0,0] (review B2).
    /// Non-blocking; caller polls `is_moving` and then `is_calibrated`.
    fn autotest(&mut self, a: Axis) -> io::Result<()>;
    fn set_backlash(&mut self, a: Axis, steps: u32) -> io::Result<()>;
}

// ------------------------------------------------------------ optics/lens

/// One ONVIF profile per lens (plan §6). Both share the one PT head.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lens {
    Wide,
    Tele,
}

impl Lens {
    pub fn token(self) -> &'static str {
        match self {
            Lens::Wide => "wide",
            Lens::Tele => "tele",
        }
    }
    pub fn from_token(t: &str) -> Option<Lens> {
        match t {
            "wide" => Some(Lens::Wide),
            "tele" => Some(Lens::Tele),
            _ => None,
        }
    }
}

// ------------------------------------------------------- onvif <-> motion

#[derive(Debug)]
pub enum PtzError {
    /// Bad argument from the client (maps to an ONVIF sender Fault).
    InvalidArg(String),
    /// Moves refused right now: not homed, self-movers still on, limits
    /// unknown, etc. (maps to a receiver Fault with this text).
    NotReady(String),
    NoSuchPreset(String),
    Hw(io::Error),
}

impl fmt::Display for PtzError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PtzError::InvalidArg(s) => write!(f, "invalid argument: {s}"),
            PtzError::NotReady(s) => write!(f, "not ready: {s}"),
            PtzError::NoSuchPreset(s) => write!(f, "no such preset: {s}"),
            PtzError::Hw(e) => write!(f, "hardware: {e}"),
        }
    }
}
impl std::error::Error for PtzError {}
impl From<io::Error> for PtzError {
    fn from(e: io::Error) -> Self {
        PtzError::Hw(e)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PtzStatus {
    /// Generic space, -1..1 across the usable (limited) range.
    pub pan: f32,
    pub tilt: f32,
    /// MOVING until both axes idle AND min-dwell (150 ms) elapsed (§4.3).
    pub moving: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Preset {
    pub token: String,
    pub name: String,
    pub pan: f32,
    pub tilt: f32,
}

/// What the ONVIF layer calls. Implemented by `ptzd_motion::Motion`.
/// All methods return quickly; motion happens on the worker thread.
pub trait PtzControl: Send + Sync {
    /// FOV-space RelativeMove (§4.2). x, y in -1..1, 1.0 = half the FOV
    /// of `lens`. `speed` in 0..1 (Frigate always sends 1).
    fn relative_move(&self, lens: Lens, x: f32, y: f32, speed: Option<f32>) -> Result<(), PtzError>;
    /// Generic velocity -1..1. Moves towards the limit until Stop.
    /// `timeout` from the request if present.
    fn continuous_move(&self, vx: f32, vy: f32, timeout: Option<Duration>) -> Result<(), PtzError>;
    /// Generic position -1..1 over the usable range.
    fn absolute_move(&self, pan: f32, tilt: f32, speed: Option<f32>) -> Result<(), PtzError>;
    fn stop(&self) -> Result<(), PtzError>;
    fn status(&self) -> PtzStatus;
    fn presets(&self) -> Vec<Preset>;
    fn goto_preset(&self, token: &str) -> Result<(), PtzError>;
    /// Returns the token. `token` given = overwrite that preset.
    fn set_preset(&self, name: Option<&str>, token: Option<&str>) -> Result<String, PtzError>;
    fn remove_preset(&self, token: &str) -> Result<(), PtzError>;
}

// ------------------------------------------------------ auth / reolink

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthResult {
    Ok,
    BadCredentials,
    /// Reolink API unreachable or rate-limited: fail closed, Fault text says so.
    Unavailable,
}

/// Verifies a WS-UsernameToken PasswordText pair against the camera's own
/// accounts by a Reolink `Login` on localhost (plan §9 Credentials).
/// Implemented by `ptzd_reolink::ReolinkAuth`. Caches (user, sha256(pw))
/// -> token in RAM only. A successful check also lets the reolink side
/// run the self-mover disable and limit refresh (D1, D4).
pub trait Authenticator: Send + Sync {
    fn check(&self, user: &str, password: &str) -> AuthResult;
}

/// App-configured limits and self-mover state, from the Reolink side.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PanLimits {
    /// Raw pan steps; None = unset (-1 in Reolink's storage).
    pub left: Option<u32>,
    pub right: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SelfMovers {
    /// Names of self-movers found ON (e.g. "smart_track", "guard",
    /// "patrol 2", "pattern 1"). Empty = safe to move.
    pub enabled: Vec<String>,
}

/// Feeds motion with limits and the "safe to move" gate.
/// Implemented by `ptzd_reolink::Reolink`.
pub trait ReolinkState: Send + Sync {
    fn pan_limits(&self) -> PanLimits;
    fn self_movers(&self) -> SelfMovers;
}
