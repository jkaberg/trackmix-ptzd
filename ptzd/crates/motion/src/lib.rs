//! Motion: `PtzControl` on top of a `MotorDev` (plan §4.2-4.4, §7a).
//! One worker thread owns the device. See ../../README.md.
#![forbid(unsafe_code)]

mod clock;
mod config;
mod core;
mod store;
#[cfg(test)]
mod tests;

pub use clock::{Clock, SystemClock};
pub use config::{Fov, MotionConfig};

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use ptzd_core::{
    Axis, Lens, MotorDev, PanLimits, PtzControl, PtzError, PtzStatus, Preset, ReolinkState,
};

use crate::core::{Cmd, Core, Kind, MoveReq};
use crate::store::{Store, StoredPreset};

/// Expected GET_INFO, plan §4.4 step 1: pan {2701,0,16}, tilt {1025,0,16}.
pub(crate) const EXPECT_INFO: [(u32, u32, u32); 2] = [(2701, 0, 16), (1025, 0, 16)];

pub(crate) fn ix(a: Axis) -> usize {
    match a {
        Axis::Pan => 0,
        Axis::Tilt => 1,
    }
}

pub(crate) type Range = (u32, u32);

pub(crate) struct Inner {
    pub homed: bool,
    pub fail: Option<String>,
    pub rehoming: bool,
    /// Something other than ptzd is moving a motor (W5).
    pub foreign: bool,
    /// GET_STATE stayed busy past our R8 deadline: do not report it as MOVING\n    /// (a hung read would stall Frigate forever) until it reads idle once.
    pub hung: bool,
    pub closing: bool,
    pub moving: bool,
    pub accepted_at: Duration,
    pub last_cmd: Duration,
    pub pos: [u32; 2],
    /// Hardware soft-margin window per axis (inclusive).
    pub range_hw: [Range; 2],
    pub presets: Vec<StoredPreset>,
}

pub(crate) struct Shared {
    pub inner: Mutex<Inner>,
    pub abort: AtomicBool,
    pub warned_one_sided: AtomicBool,
}

impl Shared {
    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// hw window intersected with Reolink pan limits. Tilt has no Reolink limit.
/// Each set limit applies on its own. `left_is_low` says whether LimitLeft is
/// the low step end (UNPROVEN, T12).
pub(crate) fn usable(hw: [Range; 2], lim: PanLimits, left_is_low: bool) -> Result<[Range; 2], String> {
    let mut r = hw;
    let (low, high) = if left_is_low { (lim.left, lim.right) } else { (lim.right, lim.left) };
    if let (Some(l), Some(h)) = (low, high) {
        if l > h {
            log::warn!("pan limits order contradicts pan_left_is_low={left_is_low}, using min/max");
        }
        r[0].0 = r[0].0.max(l.min(h));
        r[0].1 = r[0].1.min(l.max(h));
    } else {
        if let Some(l) = low {
            r[0].0 = r[0].0.max(l);
        }
        if let Some(h) = high {
            r[0].1 = r[0].1.min(h);
        }
    }
    for (i, n) in ["pan", "tilt"].iter().enumerate() {
        if r[i].0 >= r[i].1 {
            return Err(format!("{n} usable range empty ({}..{})", r[i].0, r[i].1));
        }
    }
    Ok(r)
}

pub(crate) fn clamp_to(v: f64, r: Range) -> u32 {
    if !v.is_finite() {
        return r.0;
    }
    v.max(r.0 as f64).min(r.1 as f64) as u32
}

/// steps -> generic -1..1 (sign flips for reversed axes).
pub(crate) fn to_generic(steps: u32, r: Range, reversed: bool) -> f32 {
    let g = ((steps as f64 - r.0 as f64) / (r.1 - r.0) as f64) * 2.0 - 1.0;
    let g = g.clamp(-1.0, 1.0);
    (if reversed { -g } else { g }) as f32
}

pub(crate) fn from_generic(g: f32, r: Range, reversed: bool) -> u32 {
    let g = (g as f64).clamp(-1.0, 1.0);
    let g = if reversed { -g } else { g };
    clamp_to(r.0 as f64 + (g + 1.0) / 2.0 * (r.1 - r.0) as f64 + 0.5, r)
}

pub struct Motion {
    shared: Arc<Shared>,
    tx: Sender<Cmd>,
    reolink: Arc<dyn ReolinkState>,
    clock: Arc<dyn Clock>,
    cfg: Arc<MotionConfig>,
    store: Store,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl Motion {
    /// Spawns the worker, which homes first. Moves return NotReady until done.
    pub fn new(dev: Box<dyn MotorDev>, reolink: Arc<dyn ReolinkState>, cfg: MotionConfig) -> Motion {
        Self::with_clock(dev, reolink, cfg, Arc::new(SystemClock::new()))
    }

    pub fn with_clock(
        dev: Box<dyn MotorDev>,
        reolink: Arc<dyn ReolinkState>,
        cfg: MotionConfig,
        clock: Arc<dyn Clock>,
    ) -> Motion {
        let (m, mut core) = Self::build(dev, reolink, cfg, clock);
        let h = std::thread::spawn(move || core.run());
        *m.handle.lock().unwrap_or_else(|e| e.into_inner()) = Some(h);
        m
    }

    /// Motion plus a worker core the caller drives (tests).
    pub(crate) fn build(
        dev: Box<dyn MotorDev>,
        reolink: Arc<dyn ReolinkState>,
        cfg: MotionConfig,
        clock: Arc<dyn Clock>,
    ) -> (Motion, Core) {
        let cfg = Arc::new(cfg);
        log::warn!("pan_left_is_low={} is unverified (T12)", cfg.pan_left_is_low);
        let store = Store::new(&cfg.state_dir);
        let m = cfg.soft_margin;
        let hw = [
            (EXPECT_INFO[0].1.saturating_add(m), (EXPECT_INFO[0].0 - 1).saturating_sub(m)),
            (EXPECT_INFO[1].1.saturating_add(m), (EXPECT_INFO[1].0 - 1).saturating_sub(m)),
        ];
        let presets = store.read_presets();
        let shared = Arc::new(Shared {
            inner: Mutex::new(Inner {
                homed: false,
                fail: None,
                rehoming: false,
                foreign: false,
                hung: false,
                closing: false,
                moving: false,
                accepted_at: Duration::ZERO,
                last_cmd: clock.now(),
                pos: [0, 0],
                range_hw: hw,
                presets: presets.clone().unwrap_or_default(),
            }),
            abort: AtomicBool::new(false),
            warned_one_sided: AtomicBool::new(false),
        });
        let (tx, rx) = channel();
        let core = Core::new(dev, reolink.clone(), cfg.clone(), clock.clone(), shared.clone(), rx, presets.is_none());
        let motion = Motion {
            shared,
            tx,
            reolink,
            clock,
            cfg,
            store,
            handle: Mutex::new(None),
        };
        (motion, core)
    }

    /// Homing (or a re-home) failed for good. ptzd exits non-zero so runit
    /// retries (review 2 W1); the message is the reason.
    pub fn fatal(&self) -> Option<String> {
        self.shared.lock().fail.clone()
    }

    /// Live GET_POS through the worker (review 2 W2), cached value on timeout.
    fn fresh_pos(&self) -> [u32; 2] {
        let (tx, rx) = channel();
        if self.tx.send(Cmd::Pos(tx)).is_ok() {
            if let Ok(Some(p)) = rx.recv_timeout(Duration::from_millis(300)) {
                return p;
            }
        }
        log::warn!("live GET_POS unavailable, using the cached position");
        self.shared.lock().pos
    }

    pub fn is_ready(&self) -> bool {
        self.gate().is_ok()
    }

    /// STOP, wait idle, save position, write the clean marker, join worker (§7a).
    pub fn shutdown(&self) {
        {
            let mut i = self.shared.lock();
            i.closing = true;
        }
        self.shared.abort.store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.tx.send(Cmd::Shutdown);
        let h = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = h {
            let _ = h.join();
        }
    }

    /// On-demand re-home (CLI). Runs when idle; moves are refused meanwhile.
    pub fn rehome(&self) -> Result<(), PtzError> {
        self.gate()?;
        self.tx.send(Cmd::Rehome).map_err(|_| PtzError::NotReady("worker stopped".into()))
    }

    /// Self-movers off, not closing/failed/re-homing, homed, usable range ok.
    fn gate(&self) -> Result<[Range; 2], PtzError> {
        let sm = self.reolink.self_movers();
        if !sm.enabled.is_empty() {
            return Err(PtzError::NotReady(format!("self-movers active: {}", sm.enabled.join(", "))));
        }
        let i = self.shared.lock();
        if i.closing {
            return Err(PtzError::NotReady("shutting down".into()));
        }
        if let Some(f) = &i.fail {
            return Err(PtzError::NotReady(format!("homing failed: {f}")));
        }
        if i.rehoming {
            return Err(PtzError::NotReady("re-homing".into()));
        }
        if !i.homed {
            return Err(PtzError::NotReady("homing not complete".into()));
        }
        if i.foreign {
            return Err(PtzError::NotReady("camera is being moved by the Reolink app".into()));
        }
        let lim = self.reolink.pan_limits();
        if lim.left.is_some() != lim.right.is_some()
            && !self.shared.warned_one_sided.swap(true, std::sync::atomic::Ordering::SeqCst)
        {
            // R7: with one limit set, which step end it clamps depends on T12.
            log::warn!(
                "one-sided Reolink pan limit applied with pan_left_is_low={} (UNPROVEN, T12): the wrong end may be clamped",
                self.cfg.pan_left_is_low
            );
        }
        usable(i.range_hw, lim, self.cfg.pan_left_is_low).map_err(PtzError::NotReady)
    }

    fn accept(&self, kind: Kind, speed: u32) -> Result<(), PtzError> {
        self.gate()?;
        let mut i = self.shared.lock();
        let now = self.clock.now();
        i.moving = true;
        i.accepted_at = now;
        i.last_cmd = now;
        self.tx
            .send(Cmd::Move(MoveReq { kind, speed }))
            .map_err(|_| PtzError::NotReady("worker stopped".into()))
    }

    /// Speed 1 (Frigate) -> fast class; lower -> mid class (policy).
    fn speed_for(&self, s: Option<f32>) -> u32 {
        match s {
            Some(v) if v < 0.99 => self.cfg.mid_speed,
            _ => self.cfg.fast_speed,
        }
    }
}

fn check_unit(name: &str, v: f32) -> Result<(), PtzError> {
    if v.is_finite() && v.abs() <= 1.0 {
        Ok(())
    } else {
        Err(PtzError::InvalidArg(format!("{name}={v} outside -1..1")))
    }
}

pub(crate) fn sgn(reversed: bool) -> f64 {
    if reversed {
        -1.0
    } else {
        1.0
    }
}

/// Image angles (degrees right/up of centre) of a point, seen with the lens
/// `depression` degrees below level -> (pan right, tilt up) in degrees that
/// centre it. Equal angles per pixel, as measured on the wide lens. Looking
/// down, the same image offset needs more pan (about 1/cos(depression)).
pub(crate) fn aim(right: f64, up: f64, depression: f64) -> (f64, f64) {
    let (ax, ay) = (right.to_radians(), up.to_radians());
    let th = ax.hypot(ay);
    if th < 1e-9 {
        return (0.0, 0.0);
    }
    let (sd, cd) = depression.to_radians().sin_cos();
    let (r, u, f) = (th.sin() * ax / th, th.sin() * ay / th, th.cos());
    // Floor: a point at or past straight down must not spin the head around.
    let horiz = (f * cd + u * sd).max(0.1);
    let vert = u * cd - f * sd;
    let pan = r.atan2(horiz);
    let tilt = vert.atan2(r.hypot(horiz));
    (pan.to_degrees(), tilt.to_degrees() + depression)
}

impl PtzControl for Motion {
    fn relative_move(&self, lens: Lens, x: f32, y: f32, speed: Option<f32>) -> Result<(), PtzError> {
        check_unit("x", x)?;
        check_unit("y", y)?;
        let fov = match lens {
            Lens::Wide => self.cfg.wide,
            Lens::Tele => self.cfg.tele,
        };
        // Image angles only; core turns them into steps at issue time, because
        // the pan needed depends on the tilt the head then has.
        let right = x as f64 * fov.h / 2.0;
        let up = y as f64 * fov.v / 2.0;
        self.accept(Kind::Rel { right, up }, self.speed_for(speed))
    }

    fn continuous_move(&self, vx: f32, vy: f32, timeout: Option<Duration>) -> Result<(), PtzError> {
        check_unit("vx", vx)?;
        check_unit("vy", vy)?;
        let d = |v: f32, rev: bool| -> i8 {
            let s = if v > 1e-3 {
                1
            } else if v < -1e-3 {
                -1
            } else {
                0
            };
            if rev {
                -s
            } else {
                s
            }
        };
        let dir = [d(vx, self.cfg.pan_reversed), d(vy, self.cfg.tilt_reversed)];
        if dir == [0, 0] {
            return self.stop();
        }
        self.accept(Kind::Cont { dir, timeout }, self.cfg.mid_speed)
    }

    fn absolute_move(&self, pan: f32, tilt: f32, speed: Option<f32>) -> Result<(), PtzError> {
        check_unit("pan", pan)?;
        check_unit("tilt", tilt)?;
        self.accept(Kind::Abs { pan, tilt }, self.speed_for(speed))
    }

    fn stop(&self) -> Result<(), PtzError> {
        self.shared.lock().last_cmd = self.clock.now();
        self.tx.send(Cmd::Stop).map_err(|_| PtzError::NotReady("worker stopped".into()))
    }

    fn status(&self) -> PtzStatus {
        let i = self.shared.lock();
        let r = usable(i.range_hw, self.reolink.pan_limits(), self.cfg.pan_left_is_low).unwrap_or(i.range_hw);
        PtzStatus {
            pan: to_generic(i.pos[0], r[0], self.cfg.pan_reversed),
            tilt: to_generic(i.pos[1], r[1], self.cfg.tilt_reversed),
            moving: i.moving || i.rehoming || (i.foreign && !i.hung),
        }
    }

    fn presets(&self) -> Vec<Preset> {
        let i = self.shared.lock();
        let r = usable(i.range_hw, self.reolink.pan_limits(), self.cfg.pan_left_is_low).unwrap_or(i.range_hw);
        i.presets
            .iter()
            .map(|p| Preset {
                token: p.token.clone(),
                name: p.name.clone(),
                pan: to_generic(p.pan, r[0], self.cfg.pan_reversed),
                tilt: to_generic(p.tilt, r[1], self.cfg.tilt_reversed),
            })
            .collect()
    }

    fn goto_preset(&self, token: &str) -> Result<(), PtzError> {
        let p = self
            .shared
            .lock()
            .presets
            .iter()
            .find(|p| p.token == token)
            .cloned()
            .ok_or_else(|| PtzError::NoSuchPreset(token.to_string()))?;
        self.accept(Kind::Steps { pan: p.pan, tilt: p.tilt }, self.cfg.fast_speed)
    }

    fn set_preset(&self, name: Option<&str>, token: Option<&str>) -> Result<String, PtzError> {
        if !self.shared.lock().homed {
            return Err(PtzError::NotReady("homing not complete".into()));
        }
        let pos = self.fresh_pos();
        let mut i = self.shared.lock();
        let tok = match token {
            Some(t) if !t.is_empty() => t.to_string(),
            _ => (1u32..)
                .map(|n| n.to_string())
                .find(|t| !i.presets.iter().any(|p| &p.token == t))
                .unwrap_or_default(),
        };
        match i.presets.iter_mut().find(|p| p.token == tok) {
            Some(p) => {
                p.pan = pos[0];
                p.tilt = pos[1];
                if let Some(n) = name {
                    p.name = n.to_string();
                }
            }
            None => i.presets.push(StoredPreset {
                token: tok.clone(),
                name: name.map(str::to_string).unwrap_or_else(|| format!("Preset {tok}")),
                pan: pos[0],
                tilt: pos[1],
            }),
        }
        self.store.write_presets(&i.presets)?;
        Ok(tok)
    }

    fn remove_preset(&self, token: &str) -> Result<(), PtzError> {
        let mut i = self.shared.lock();
        let n = i.presets.len();
        i.presets.retain(|p| p.token != token);
        if i.presets.len() == n {
            return Err(PtzError::NoSuchPreset(token.to_string()));
        }
        self.store.write_presets(&i.presets)?;
        Ok(())
    }
}

impl Drop for Motion {
    fn drop(&mut self) {
        // Worker sees the channel close and runs the shutdown sequence.
        let h = self.handle.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(h) = h {
            self.shared.abort.store(true, std::sync::atomic::Ordering::SeqCst);
            let _ = self.tx.send(Cmd::Shutdown);
            let _ = h.join();
        }
    }
}
