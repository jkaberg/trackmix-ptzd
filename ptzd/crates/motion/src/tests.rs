//! Core tests on a fake clock and a fake MotorDev. Independent of ptzd-hw.
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ptzd_core::*;

use crate::core::Core;
use crate::{Clock, Motion, MotionConfig};

// ------------------------------------------------------------ fake clock

struct FakeClock {
    ms: AtomicU64,
    day_hour: Mutex<(u64, u32)>,
    /// Called on every sleep (probe for "what does status() say mid-homing").
    hook: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
}
impl FakeClock {
    fn new() -> Arc<Self> {
        Arc::new(FakeClock { ms: AtomicU64::new(1000), day_hour: Mutex::new((100, 0)), hook: Mutex::new(None) })
    }
    fn advance(&self, ms: u64) {
        self.ms.fetch_add(ms, Ordering::SeqCst);
    }
}
impl Clock for FakeClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.ms.load(Ordering::SeqCst))
    }
    fn sleep(&self, d: Duration) {
        self.advance(d.as_millis() as u64);
        if let Some(h) = self.hook.lock().unwrap().as_ref() {
            h();
        }
    }
    fn utc_day_hour(&self) -> (u64, u32) {
        *self.day_hour.lock().unwrap()
    }
}

// ------------------------------------------------------------ fake motor

#[derive(Debug, Clone, PartialEq)]
enum Ev {
    SetPos(Axis, u32, u32),
    Stop(Axis),
    Init(Axis, u32),
    Autotest(Axis),
    Backlash(Axis, u32),
}

#[derive(Default, Clone, Copy)]
struct Ax {
    pos: f64,
    target: f64,
    moving: bool,
    calibrated: bool,
}

struct St {
    ax: [Ax; 2],
    last: Duration,
    log: Vec<Ev>,
    /// SET_POS issued while moving or uncalibrated.
    violations: u32,
    info: [(u32, u32, u32); 2],
    /// GET_STATE reads 1 forever and STOP has no effect.
    stuck: bool,
    /// INIT_POS/AUTOTEST fail with EBUSY this many times first.
    busy_left: u32,
}

/// Moves 1 step/ms.
#[derive(Clone)]
struct Fake {
    st: Arc<Mutex<St>>,
    clock: Arc<FakeClock>,
}

const HOME: [f64; 2] = [1353.0, 100.0]; // plan §2.2 cfg home

impl Fake {
    fn new(clock: Arc<FakeClock>, calibrated: bool, pos: [u32; 2]) -> Fake {
        let a = |p: u32| Ax { pos: p as f64, target: p as f64, moving: false, calibrated };
        Fake {
            st: Arc::new(Mutex::new(St {
                ax: [a(pos[0]), a(pos[1])],
                last: clock.now(),
                log: vec![],
                violations: 0,
                info: [(2701, 0, 16), (1025, 0, 16)],
                stuck: false,
                busy_left: 0,
            })),
            clock,
        }
    }
    fn upd(&self, s: &mut St) {
        let now = self.clock.now();
        let dt = (now - s.last).as_millis() as f64;
        s.last = now;
        for a in s.ax.iter_mut() {
            if a.moving {
                let d = a.target - a.pos;
                if d.abs() <= dt {
                    a.pos = a.target;
                    a.moving = false;
                } else {
                    a.pos += dt * d.signum();
                }
            }
        }
    }
    fn log(&self) -> Vec<Ev> {
        self.st.lock().unwrap().log.clone()
    }
    fn clear_log(&self) {
        self.st.lock().unwrap().log.clear();
    }
    /// Another process (the Reolink app) starts a move.
    fn ext_move(&self, a: Axis, pos: u32) {
        let mut s = self.st.lock().unwrap();
        self.upd(&mut s);
        let ax = &mut s.ax[i(a)];
        ax.target = pos as f64;
        ax.moving = true;
    }
    fn stops(&self) -> usize {
        self.log().iter().filter(|e| matches!(e, Ev::Stop(_))).count()
    }
    fn violations(&self) -> u32 {
        self.st.lock().unwrap().violations
    }
    fn pos(&self) -> [u32; 2] {
        let mut s = self.st.lock().unwrap();
        self.upd(&mut s);
        [s.ax[0].pos as u32, s.ax[1].pos as u32]
    }
    fn setposes(&self) -> Vec<(Axis, u32, u32)> {
        self.log()
            .into_iter()
            .filter_map(|e| if let Ev::SetPos(a, p, s) = e { Some((a, p, s)) } else { None })
            .collect()
    }
}

fn i(a: Axis) -> usize {
    match a {
        Axis::Pan => 0,
        Axis::Tilt => 1,
    }
}

impl MotorDev for Fake {
    fn info(&mut self, a: Axis) -> io::Result<MotorInfo> {
        let s = self.st.lock().unwrap();
        let (total, min, step) = s.info[i(a)];
        Ok(MotorInfo { total, min, step })
    }
    fn is_moving(&mut self, a: Axis) -> io::Result<bool> {
        let mut s = self.st.lock().unwrap();
        self.upd(&mut s);
        Ok(s.ax[i(a)].moving || s.stuck)
    }
    fn position(&mut self, a: Axis) -> io::Result<u32> {
        let mut s = self.st.lock().unwrap();
        self.upd(&mut s);
        Ok(s.ax[i(a)].pos as u32)
    }
    fn is_calibrated(&mut self, a: Axis) -> io::Result<bool> {
        Ok(self.st.lock().unwrap().ax[i(a)].calibrated)
    }
    fn set_pos(&mut self, a: Axis, pos: u32, speed: RawSpeed) -> io::Result<()> {
        let mut s = self.st.lock().unwrap();
        self.upd(&mut s);
        s.log.push(Ev::SetPos(a, pos, speed));
        let ax = &mut s.ax[i(a)];
        if ax.moving || !ax.calibrated {
            s.violations += 1;
            return Err(io::Error::from_raw_os_error(16));
        }
        ax.target = pos as f64;
        ax.moving = ax.target != ax.pos;
        Ok(())
    }
    fn stop(&mut self, a: Axis) -> io::Result<()> {
        let mut s = self.st.lock().unwrap();
        self.upd(&mut s);
        s.log.push(Ev::Stop(a));
        let ax = &mut s.ax[i(a)];
        ax.moving = false;
        ax.target = ax.pos;
        Ok(())
    }
    fn init_pos(&mut self, a: Axis, pos: u32) -> io::Result<()> {
        let mut s = self.st.lock().unwrap();
        if s.busy_left > 0 {
            s.busy_left -= 1;
            return Err(io::Error::from_raw_os_error(16));
        }
        s.log.push(Ev::Init(a, pos));
        s.ax[i(a)] = Ax { pos: pos as f64, target: pos as f64, moving: false, calibrated: true };
        Ok(())
    }
    fn autotest(&mut self, a: Axis) -> io::Result<()> {
        let mut s = self.st.lock().unwrap();
        self.upd(&mut s);
        if s.busy_left > 0 {
            s.busy_left -= 1;
            return Err(io::Error::from_raw_os_error(16));
        }
        s.log.push(Ev::Autotest(a));
        let ax = &mut s.ax[i(a)];
        // calibrated before the final goto ends (plan §2.2)
        ax.calibrated = true;
        ax.target = HOME[i(a)];
        ax.moving = ax.target != ax.pos;
        Ok(())
    }
    fn set_backlash(&mut self, a: Axis, v: u32) -> io::Result<()> {
        self.st.lock().unwrap().log.push(Ev::Backlash(a, v));
        Ok(())
    }
}

// ----------------------------------------------------------- fake reolink

#[derive(Default)]
struct FakeRl {
    lim: Mutex<PanLimits>,
    sm: Mutex<Vec<String>>,
}
impl ReolinkState for FakeRl {
    fn pan_limits(&self) -> PanLimits {
        *self.lim.lock().unwrap()
    }
    fn self_movers(&self) -> SelfMovers {
        SelfMovers { enabled: self.sm.lock().unwrap().clone() }
    }
}

// ---------------------------------------------------------------- harness

struct H {
    m: Motion,
    core: Core,
    dev: Fake,
    clock: Arc<FakeClock>,
    rl: Arc<FakeRl>,
    dir: PathBuf,
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ptzd-motion-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn mk(tag: &str, calibrated: bool, pos: [u32; 2]) -> H {
    let dir = tmpdir(tag);
    mk_in(dir, calibrated, pos, None)
}

/// Round optics, level at tilt 512, so hand values stay simple.
fn test_cfg(dir: &std::path::Path) -> MotionConfig {
    let mut cfg = MotionConfig::new(dir);
    cfg.pan_steps_per_deg = 7.5;
    cfg.tilt_steps_per_deg = 1024.0 / 90.0;
    cfg.tilt_level_step = 512.0;
    cfg.tilt_reversed = false;
    cfg.wide = crate::Fov { h: 104.0, v: 60.0 };
    cfg.tele = crate::Fov { h: 38.0, v: 21.0 };
    cfg
}

fn mk_in(dir: PathBuf, calibrated: bool, pos: [u32; 2], tweak: Option<&dyn Fn(&mut MotionConfig)>) -> H {
    let clock = FakeClock::new();
    let dev = Fake::new(clock.clone(), calibrated, pos);
    let rl = Arc::new(FakeRl::default());
    let mut cfg = test_cfg(&dir);
    if let Some(t) = tweak {
        t(&mut cfg);
    }
    let (m, core) = Motion::build(Box::new(dev.clone()), rl.clone(), cfg, clock.clone());
    H { m, core, dev, clock, rl, dir }
}

impl H {
    fn homed(mut self) -> H {
        self.core.home().unwrap();
        self
    }
    /// Advance 10 ms ticks.
    fn run(&mut self, ms: u64) {
        for _ in 0..ms / 10 {
            self.clock.advance(10);
            self.core.pump();
        }
    }
    fn settle(&mut self) {
        self.core.pump();
        for _ in 0..2000 {
            if !self.m.status().moving {
                return;
            }
            self.run(10);
        }
        panic!("never settled");
    }
}

fn near(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-4
}

// ------------------------------------------------------------------ tests

#[test]
fn fov_math_matches_hand_values() {
    // pan 7.5 steps/deg, tilt 1024/90 = 11.3778 steps/deg, level at tilt 512.
    // At level a pure x offset is pure pan: 0.5*104/2*7.5 = 195 -> 1548
    let mut h = mk("fov1", true, [1353, 512]).homed();
    h.m.relative_move(Lens::Wide, 0.5, 0.0, Some(1.0)).unwrap();
    h.settle();
    assert_eq!(h.dev.pos(), [1548, 512]);
    // A pure y offset is pure tilt: 21/2*11.3778 = 119.47 -> 119
    h.m.relative_move(Lens::Tele, 0.0, 1.0, Some(1.0)).unwrap();
    h.settle();
    assert_eq!(h.dev.pos(), [1548, 631]);
    // and stays so away from level: -0.25*60/2*11.3778 = -85.33 -> -85
    h.m.relative_move(Lens::Wide, 0.0, -0.25, Some(1.0)).unwrap();
    h.settle();
    assert_eq!(h.dev.pos(), [1548, 546]);
}

#[test]
fn relative_move_uses_the_tilt_it_starts_from() {
    // Independent ray-model values (Python, not aim()): 26 right, 7.5 down of
    // centre at level -> pan +26.14 deg, tilt -7.25 deg.
    let mut h = mk("fov2", true, [1353, 512]).homed();
    h.m.relative_move(Lens::Wide, 0.5, -0.25, Some(1.0)).unwrap();
    h.settle();
    assert_eq!(h.dev.pos(), [1549, 430]);
}

#[test]
fn aim_looking_down() {
    use crate::aim;
    let close = |a: (f64, f64), b: (f64, f64)| (a.0 - b.0).abs() < 0.05 && (a.1 - b.1).abs() < 0.05;
    // small offset: pan grows by 1/cos(depression) = 1.414 at 45
    let (p, t) = aim(1.0, 0.0, 45.0);
    assert!((p - 1.414).abs() < 0.005 && t.abs() < 0.02, "{p} {t}");
    // y only: pure tilt
    assert!(close(aim(0.0, -12.0, 45.0), (0.0, -12.0)));
    // mirrored in x
    let (l, r) = (aim(-20.0, 5.0, 45.0), aim(20.0, 5.0, 45.0));
    assert!(close((-l.0, l.1), r));
    // a point past straight down must not swing the head around
    let (p, t) = aim(0.0, -28.0, 70.0);
    assert!(p == 0.0 && t.is_finite() && t.abs() <= 28.0, "{p} {t}");
    assert!(aim(5.0, -28.0, 85.0).0.abs() <= 90.0);
    assert_eq!(aim(0.0, 0.0, 45.0), (0.0, 0.0));
}

#[test]
fn tilt_reversed_flips_sign() {
    let mut h = mk_in(tmpdir("rev"), true, [1353, 512], Some(&|c| c.tilt_reversed = true)).homed();
    h.m.relative_move(Lens::Wide, 0.0, -0.25, None).unwrap();
    h.settle();
    assert_eq!(h.dev.pos()[1], 512 + 85);
}

#[test]
fn frigate_speed_one_uses_fast_class() {
    let mut h = mk("spd", true, [1353, 512]).homed();
    h.m.relative_move(Lens::Wide, 0.1, 0.0, Some(1.0)).unwrap();
    h.settle();
    let cfg = MotionConfig::new("x");
    assert_eq!(h.dev.setposes()[0].2, cfg.fast_speed);
}

#[test]
fn never_set_pos_while_moving() {
    let mut h = mk("b1", true, [1353, 512]).homed();
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap(); // 390 steps
    h.run(100);
    assert!(h.m.status().moving);
    h.dev.clear_log();
    h.m.relative_move(Lens::Wide, -1.0, 0.0, None).unwrap(); // retarget while moving
    h.run(50);
    h.m.absolute_move(0.0, 0.0, None).unwrap();
    h.settle();
    assert_eq!(h.dev.violations(), 0);
    let log = h.dev.log();
    let first_set = log.iter().position(|e| matches!(e, Ev::SetPos(..))).unwrap();
    assert!(log[..first_set].contains(&Ev::Stop(Axis::Pan)), "STOP before SET_POS: {log:?}");
    // final target: last absolute move = centre of usable range (20..2680 -> 1350)
    assert_eq!(h.dev.pos()[0], 1350);
}

#[test]
fn hw_margin_clamps() {
    let mut h = mk("clamp", true, [2600, 512]).homed();
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap(); // +390
    h.settle();
    assert_eq!(h.dev.pos()[0], 2680); // 2700 - 20
    h.m.relative_move(Lens::Wide, -1.0, 0.0, None).unwrap();
    h.m.relative_move(Lens::Wide, -1.0, 0.0, None).unwrap();
    h.settle();
    h.m.continuous_move(-1.0, 0.0, None).unwrap();
    h.run(5000);
    assert_eq!(h.dev.pos()[0], 20);
    // tilt: 1024 - 20 = 1004 top
    h.m.relative_move(Lens::Wide, 0.0, 1.0, None).unwrap();
    h.settle();
    h.m.continuous_move(0.0, 1.0, None).unwrap();
    h.run(2000);
    assert_eq!(h.dev.pos()[1], 1004);
}

#[test]
fn reolink_limits_clamp_and_reread_each_move() {
    let mut h = mk("rl", true, [1353, 512]).homed();
    *h.rl.lim.lock().unwrap() = PanLimits { left: Some(500), right: Some(2000) };
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap(); // +390 -> 1743, inside
    h.settle();
    assert_eq!(h.dev.pos()[0], 1743);
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap(); // 2133 -> 2000
    h.settle();
    assert_eq!(h.dev.pos()[0], 2000);
    // status maps the limited range to -1..1
    assert!(near(h.m.status().pan, 1.0));
    // limits change: next move uses the new ones (order swapped = still min/max)
    *h.rl.lim.lock().unwrap() = PanLimits { left: Some(1800), right: Some(900) };
    h.m.relative_move(Lens::Wide, -0.1, 0.0, None).unwrap(); // -39 -> 1961 -> clamp 1800
    h.settle();
    assert_eq!(h.dev.pos()[0], 1800);
    // limits outside the hw window leave nothing usable
    *h.rl.lim.lock().unwrap() = PanLimits { left: Some(2690), right: Some(2700) };
    assert!(matches!(h.m.relative_move(Lens::Wide, 0.1, 0.0, None), Err(PtzError::NotReady(_))));
}

#[test]
fn single_limits_apply_with_both_flag_values() {
    for left_is_low in [true, false] {
        // left-only
        let mut h = mk_in(tmpdir("lo"), true, [1353, 512], Some(&|c| c.pan_left_is_low = left_is_low)).homed();
        *h.rl.lim.lock().unwrap() = PanLimits { left: Some(1000), right: None };
        let (lo, hi) = if left_is_low { (1000, 2680) } else { (20, 1000) };
        h.m.absolute_move(-1.0, 0.0, None).unwrap();
        h.settle();
        assert_eq!(h.dev.pos()[0], lo);
        h.m.absolute_move(1.0, 0.0, None).unwrap();
        h.settle();
        assert_eq!(h.dev.pos()[0], hi);
        // right-only
        *h.rl.lim.lock().unwrap() = PanLimits { left: None, right: Some(2000) };
        let (lo, hi) = if left_is_low { (20, 2000) } else { (2000, 2680) };
        h.m.absolute_move(-1.0, 0.0, None).unwrap();
        h.settle();
        assert_eq!(h.dev.pos()[0], lo);
        h.m.absolute_move(1.0, 0.0, None).unwrap();
        h.settle();
        assert_eq!(h.dev.pos()[0], hi);
    }
}

#[test]
fn generic_space_maps_usable_range() {
    let mut h = mk("gen", true, [1353, 512]).homed();
    *h.rl.lim.lock().unwrap() = PanLimits { left: Some(1000), right: Some(2000) };
    h.m.absolute_move(-1.0, 0.0, None).unwrap();
    h.settle();
    assert_eq!(h.dev.pos()[0], 1000);
    assert!(near(h.m.status().pan, -1.0));
    h.m.absolute_move(0.5, 0.0, None).unwrap(); // 1000 + 0.75*1000
    h.settle();
    assert_eq!(h.dev.pos()[0], 1750);
    assert!(near(h.m.status().pan, 0.5));
}

#[test]
fn dwell_150ms_after_acceptance() {
    let mut h = mk("dwell", true, [1353, 512]).homed();
    // tiny move: 0.01 * 52 * 7.5 = 3.9 -> 4 steps = 4 ms of motion
    h.m.relative_move(Lens::Wide, 0.01, 0.0, None).unwrap();
    assert!(h.m.status().moving, "moving from acceptance");
    h.run(140);
    assert!(h.m.status().moving, "still inside dwell");
    // idle after ~4 ms, plus the 150 ms settle
    h.run(60);
    assert!(!h.m.status().moving);
    // long move: moving until idle even though dwell is over
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap(); // 390 ms
    h.run(300);
    assert!(h.m.status().moving);
    h.settle();
}

#[test]
fn zero_move_still_dwells() {
    let mut h = mk("zero", true, [1353, 512]).homed();
    h.m.relative_move(Lens::Wide, 0.0, 0.0, None).unwrap();
    assert!(h.dev.setposes().is_empty());
    h.run(100);
    assert!(h.m.status().moving);
    h.run(60);
    assert!(!h.m.status().moving);
}

#[test]
fn gate_self_movers_and_homing() {
    let mut h = mk("gate", true, [1353, 512]);
    // not homed yet
    match h.m.relative_move(Lens::Wide, 0.1, 0.0, None) {
        Err(PtzError::NotReady(s)) => assert!(s.contains("homing")),
        o => panic!("{o:?}"),
    }
    h.core.home().unwrap();
    *h.rl.sm.lock().unwrap() = vec!["smart_track".into(), "patrol 2".into()];
    for r in [
        h.m.relative_move(Lens::Wide, 0.1, 0.0, None),
        h.m.continuous_move(0.5, 0.0, None),
        h.m.absolute_move(0.0, 0.0, None),
        h.m.goto_preset("home"),
    ] {
        match r {
            Err(PtzError::NotReady(s)) => assert!(s.contains("smart_track") && s.contains("patrol 2"), "{s}"),
            o => panic!("{o:?}"),
        }
    }
    h.rl.sm.lock().unwrap().clear();
    h.m.relative_move(Lens::Wide, 0.1, 0.0, None).unwrap();
    h.settle();
    assert!(matches!(h.m.relative_move(Lens::Wide, 1.5, 0.0, None), Err(PtzError::InvalidArg(_))));
}

#[test]
fn presets_roundtrip_and_home() {
    let mut h = mk("pre", true, [1353, 512]).homed();
    let p = h.m.presets();
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].token, "home");
    h.m.relative_move(Lens::Wide, 0.5, 0.0, None).unwrap();
    h.settle();
    let t = h.m.set_preset(Some("door"), None).unwrap();
    assert_eq!(t, "1");
    assert_eq!(h.m.set_preset(None, None).unwrap(), "2");
    h.m.goto_preset("home").unwrap();
    h.settle();
    assert_eq!(h.dev.pos(), [1353, 512]);
    h.m.goto_preset("1").unwrap();
    h.settle();
    assert_eq!(h.dev.pos()[0], 1548);
    // overwrite by token keeps name
    h.m.goto_preset("home").unwrap();
    h.settle();
    assert_eq!(h.m.set_preset(None, Some("1")).unwrap(), "1");
    let p1 = h.m.presets().into_iter().find(|p| p.token == "1").unwrap();
    assert_eq!(p1.name, "door");
    assert!(near(p1.pan, h.m.status().pan));
    assert!(matches!(h.m.goto_preset("nope"), Err(PtzError::NoSuchPreset(_))));
    h.m.remove_preset("2").unwrap();
    assert!(matches!(h.m.remove_preset("2"), Err(PtzError::NoSuchPreset(_))));
    // persisted: a new Motion on the same dir sees them, and does not recreate home
    let dir = h.dir.clone();
    let h2 = mk_in(dir, true, [1353, 512], None).homed();
    let toks: Vec<_> = h2.m.presets().into_iter().map(|p| p.token).collect();
    assert_eq!(toks, vec!["home", "1"]);
}

#[test]
fn clean_marker_logic() {
    let dir = tmpdir("marker");
    // first boot, uncalibrated, no saved pos -> AUTOTEST pan then tilt, then backlash
    let mut h = mk_in(dir.clone(), false, [0, 0], None);
    h.core.home().unwrap();
    assert_eq!(
        h.dev.log(),
        vec![
            Ev::Autotest(Axis::Pan),
            Ev::Autotest(Axis::Tilt),
            Ev::Backlash(Axis::Pan, 0),
            Ev::Backlash(Axis::Tilt, 16),
        ]
    );
    assert_eq!(h.dev.pos(), [1353, 100]);
    h.m.relative_move(Lens::Wide, 0.0, 0.5, None).unwrap(); // 15 deg = 170.67
    h.settle();
    assert!(!dir.join("pos").exists(), "W3: no position saves while running");
    h.core.finish();
    assert!(dir.join("clean").exists());
    assert_eq!(h.dev.pos(), [1353, 271]);

    // reboot with marker + saved pos, motors uncalibrated -> INIT_POS, no motion
    let h2 = mk_in(dir.clone(), false, [0, 0], None).homed();
    let log = h2.dev.log();
    assert!(log.contains(&Ev::Init(Axis::Pan, 1353)) && log.contains(&Ev::Init(Axis::Tilt, 271)), "{log:?}");
    assert!(!log.iter().any(|e| matches!(e, Ev::Autotest(_))));
    // W2: valid for exactly one boot, deleted as home() consumes it.
    assert!(!dir.join("clean").exists());

    // second boot without a clean stop in between -> AUTOTEST again
    let h3 = mk_in(dir.clone(), false, [0, 0], None).homed();
    assert!(h3.dev.log().contains(&Ev::Autotest(Axis::Pan)));
}

#[test]
fn already_calibrated_adopts_position() {
    let h = mk("adopt", true, [900, 300]).homed();
    assert!(!h.dev.log().iter().any(|e| matches!(e, Ev::Autotest(_) | Ev::Init(..))));
    assert!(!h.m.status().moving);
    assert!(h.m.is_ready());
}

#[test]
fn wrong_model_refuses() {
    let mut h = mk("model", true, [900, 300]);
    h.dev.st.lock().unwrap().info[0].0 = 1234;
    assert!(h.core.home().is_err());
}

#[test]
fn continuous_goes_to_edge_at_mid_speed_then_timeout_stops() {
    let mut h = mk("cont", true, [1353, 512]).homed();
    h.m.continuous_move(0.5, 0.0, Some(Duration::from_millis(300))).unwrap();
    h.run(100);
    let sp = h.dev.setposes();
    assert_eq!(sp, vec![(Axis::Pan, 2680, MotionConfig::new("x").mid_speed)]);
    h.run(400);
    // timed out around 300 ms: stopped far from the edge
    let p = h.dev.pos()[0];
    assert!(p > 1353 && p < 1353 + 400, "{p}");
    h.settle();
    // Stop() ends an untimed one
    h.m.continuous_move(-1.0, 0.0, None).unwrap();
    h.run(200);
    h.m.stop().unwrap();
    h.settle();
    assert_eq!(h.dev.violations(), 0);
    let p2 = h.dev.pos()[0];
    assert!(p2 < p && p2 > 20);
}

#[test]
fn no_periodic_position_saves() {
    // W3: only shutdown writes `pos`.
    let mut h = mk("deb", true, [1353, 512]).homed();
    let pf = h.dir.join("pos");
    assert!(!pf.exists(), "homing must not write pos");
    h.m.relative_move(Lens::Wide, 0.5, 0.0, None).unwrap();
    h.settle();
    h.run(20_000);
    assert!(!pf.exists());
    h.core.finish();
    assert!(std::fs::read_to_string(&pf).unwrap().contains("1548"));
}

#[test]
fn daily_rehome_only_when_idle_for_a_while() {
    let mut h = mk_in(tmpdir("rehome"), true, [900, 300], Some(&|c| c.rehome_hour_utc = Some(3))).homed();
    h.run(20_000);
    assert!(!h.dev.log().iter().any(|e| matches!(e, Ev::Autotest(_))), "wrong hour");
    *h.clock.day_hour.lock().unwrap() = (100, 3);
    h.m.stop().unwrap(); // recent command
    h.run(1000);
    assert!(!h.dev.log().iter().any(|e| matches!(e, Ev::Autotest(_))), "not idle long enough");
    h.run(10 * 60 * 1000);
    let n = h.dev.log().iter().filter(|e| matches!(e, Ev::Autotest(_))).count();
    assert_eq!(n, 2);
    h.run(10 * 60 * 1000);
    let n = h.dev.log().iter().filter(|e| matches!(e, Ev::Autotest(_))).count();
    assert_eq!(n, 2, "once per day");
    assert_eq!(h.dev.pos(), [900, 300], "W4: back to the pre-re-home position");
}

#[test]
fn threaded_smoke() {
    // Real thread + real clock, fake dev on its own fake clock is not used here:
    // the fake motor reads the FakeClock, so drive it from a ticker instead.
    let dir = tmpdir("thread");
    let clock = FakeClock::new();
    let dev = Fake::new(clock.clone(), true, [1353, 512]);
    let rl = Arc::new(FakeRl::default());
    let mut cfg = MotionConfig::new(&dir);
    cfg.adopt_stable_interval = Duration::from_millis(1);
    let m = Motion::new(Box::new(dev.clone()), rl, cfg);
    for _ in 0..200 {
        if m.is_ready() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(m.is_ready());
    m.shutdown();
    assert!(dir.join("clean").exists());
    assert!(matches!(m.relative_move(Lens::Wide, 0.1, 0.0, None), Err(PtzError::NotReady(_))));
}

// ------------------------------------------------------------ log capture

static LOGS: Mutex<Vec<String>> = Mutex::new(Vec::new());
struct Cap;
impl log::Log for Cap {
    fn enabled(&self, _: &log::Metadata) -> bool {
        true
    }
    fn log(&self, r: &log::Record) {
        if let Ok(mut l) = LOGS.lock() {
            l.push(format!("{}", r.args()));
        }
    }
    fn flush(&self) {}
}
fn init_log() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = log::set_logger(&Cap);
        log::set_max_level(log::LevelFilter::Info);
    });
}
fn logged(needle: &str) -> bool {
    LOGS.lock().unwrap().iter().any(|l| l.contains(needle))
}

// ------------------------------------------- Guarded<KernelSim> harness (R3)

use ptzd_hw::{Guarded, GuardedConfig, KernelSim, Op, SimConfig};

struct SimClk(Arc<FakeClock>);
impl ptzd_hw::Clock for SimClk {
    fn now(&self) -> Duration {
        self.0.now()
    }
}

struct S {
    m: Arc<Motion>,
    core: Core,
    sim: KernelSim<SimClk>,
    clock: Arc<FakeClock>,
    _rl: Arc<FakeRl>,
}

/// `init`: Some(pos) = the driver is already calibrated there (as after
/// `device`'s boot homing), None = fresh insmod.
fn mks(tag: &str, init: Option<[u32; 2]>, tweak: Option<&dyn Fn(&mut MotionConfig)>) -> S {
    mks_in(tmpdir(tag), init, tweak)
}

fn mks_in(dir: PathBuf, init: Option<[u32; 2]>, tweak: Option<&dyn Fn(&mut MotionConfig)>) -> S {
    use ptzd_hw::RawZfi;
    let clock = FakeClock::new();
    let mut sim = KernelSim::new(SimClk(clock.clone()), SimConfig::default());
    if let Some(p) = init {
        for (a, v) in [(3u32, p[0]), (4, p[1])] {
            sim.init_pos(&ptzd_hw::abi::InitPosArg { motor: a, pos: v, ..Default::default() }).unwrap();
        }
        sim.clear_ops();
    }
    let rl = Arc::new(FakeRl::default());
    let mut cfg = test_cfg(&dir);
    if let Some(t) = tweak {
        t(&mut cfg);
    }
    let g = Guarded::new(sim.clone(), GuardedConfig::default());
    let (m, core) = Motion::build(Box::new(g), rl.clone(), cfg, clock.clone());
    S { m: Arc::new(m), core, sim, clock, _rl: rl }
}

impl S {
    fn homed(mut self) -> S {
        self.core.home().unwrap();
        self
    }
    fn run(&mut self, ms: u64) {
        for _ in 0..ms / 10 {
            self.clock.advance(10);
            self.core.pump();
        }
    }
    fn settle(&mut self) {
        self.core.pump();
        for _ in 0..3000 {
            if !self.m.status().moving {
                return;
            }
            self.run(10);
        }
        panic!("never settled");
    }
    fn pos(&self) -> [u32; 2] {
        let mut s = self.sim.clone();
        [s.raw_cur(3) as u32, s.raw_cur(4) as u32]
    }
    fn ops(&self) -> Vec<Op> {
        self.sim.ops()
    }
}

#[test]
fn sim_basic_moves_and_retarget_never_set_pos_while_moving() {
    let mut h = mks("sim1", Some([1353, 512]), None).homed();
    h.m.relative_move(Lens::Wide, 0.5, 0.0, Some(1.0)).unwrap();
    h.settle();
    assert_eq!(h.pos(), [1548, 512]);
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap();
    h.run(100);
    assert!(h.m.status().moving);
    h.m.relative_move(Lens::Wide, -1.0, 0.0, None).unwrap(); // retarget mid-move
    h.settle();
    // retarget is relative to where the STOP left the head
    assert!(h.pos()[0] < 1548 && h.pos()[0] > 1000, "{:?}", h.pos());
    let ops = h.ops();
    // the retarget (last pan SET_POS) follows a STOP
    let sets: Vec<usize> = (0..ops.len()).filter(|&i| matches!(ops[i], Op::SetPos { motor: 3, .. })).collect();
    for w in sets.windows(2).skip(sets.len() - 2) {
        assert!(ops[w[0]..w[1]].iter().any(|o| matches!(o, Op::Stop { motor: 3 })), "{ops:?}");
    }
    assert_eq!(h.sim.clone().min_cur_seen(3), 0, "no overtravel");
}

#[test]
fn w1_home_waits_for_device_boot_homing() {
    // `device` starts its boot AUTOTEST just before ptzd homes (plan §9 D1).
    let h = mks("w1", None, None);
    let dev = h.sim.device();
    dev.autotest(3).unwrap();
    dev.autotest(4).unwrap();
    let mut h = h;
    h.core.home().expect("home must wait for device's homing, not fail");
    let ops = h.ops();
    assert!(!ops.iter().any(|o| matches!(o, Op::Autotest { .. } | Op::InitPos { .. })), "{ops:?}");
    // adopted after the final goto, not a mid-move position
    assert_eq!(h.pos(), [1353, 100]);
    assert!(h.m.is_ready());
}

#[test]
fn w1_home_gives_up_at_the_deadline() {
    let mut h = mk_in(tmpdir("w1b"), true, [900, 300], Some(&|c| c.home_idle_timeout = Duration::from_secs(5)));
    h.dev.st.lock().unwrap().stuck = true;
    let t0 = h.clock.now();
    let e = h.core.home().unwrap_err();
    assert!(e.contains("deadline"), "{e}");
    let dt = h.clock.now() - t0;
    assert!(dt >= Duration::from_secs(5) && dt < Duration::from_secs(6), "{dt:?}");
}

#[test]
fn w1_ebusy_is_retried_not_fatal() {
    let mut h = mk("w1c", false, [0, 0]);
    h.dev.st.lock().unwrap().busy_left = 3;
    h.core.home().unwrap();
    assert!(h.dev.log().contains(&Ev::Autotest(Axis::Pan)));
}

#[test]
fn w2_r4_marker_only_after_idle_stop_and_one_boot_only() {
    let dir = tmpdir("w2");
    let mut h = mks_in(dir.clone(), Some([1353, 512]), None).homed();
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap();
    h.run(100);
    assert!(h.m.status().moving);
    h.core.finish(); // shutdown with a move in flight
    assert!(!dir.join("clean").exists(), "R4: no marker after an abrupt STOP");
    assert!(!dir.join("pos").exists());

    // clean shutdown while idle writes it ...
    let mut h = mks_in(dir.clone(), Some([1353, 512]), None).homed();
    h.core.finish();
    assert!(dir.join("clean").exists());
    // ... a boot consumes it (W2) even when the motors were already calibrated
    let _h = mks_in(dir.clone(), Some([1353, 512]), None).homed();
    assert!(!dir.join("clean").exists());
}

#[test]
fn r2_init_pos_readback_mismatch_falls_back_to_autotest() {
    let dir = tmpdir("r2");
    let store = crate::store::Store::new(&dir);
    store.write_pos([2500, 300]).unwrap();
    store.write_marker().unwrap();
    let mut h = mks_in(dir, None, None);
    // `device` set the pan move range to 0..2000: INIT_POS 2500 clamps to 2000
    h.sim.set_move_range(3, 0, 2000);
    h.core.home().unwrap();
    let ops = h.ops();
    assert!(ops.contains(&Op::InitPos { motor: 3, pos: 2500 }));
    assert!(ops.contains(&Op::Autotest { motor: 3 }), "{ops:?}");
    assert!(!ops.contains(&Op::Autotest { motor: 4 }), "tilt read back fine");
    assert_eq!(h.pos(), [1353, 300]);
    assert!(logged("falling back to AUTOTEST"));
}

#[test]
fn w4_rehome_reports_moving_throughout_and_returns() {
    let mut h = mks("w4", Some([900, 300]), Some(&|c| c.rehome_hour_utc = Some(3))).homed();
    let seen = Arc::new(Mutex::new(Vec::new()));
    {
        let (m, seen) = (h.m.clone(), seen.clone());
        *h.clock.hook.lock().unwrap() = Some(Box::new(move || seen.lock().unwrap().push(m.status().moving)));
    }
    h.m.rehome().unwrap();
    h.core.pump();
    *h.clock.hook.lock().unwrap() = None;
    let seen = seen.lock().unwrap().clone();
    assert!(seen.len() > 100, "AUTOTEST took {} polls", seen.len());
    assert!(seen.iter().all(|m| *m), "status reported IDLE during AUTOTEST");
    assert!(h.ops().contains(&Op::Autotest { motor: 3 }));
    h.settle();
    assert_eq!(h.pos(), [900, 300], "returned to the pre-re-home position");
}

#[test]
fn w5_never_stops_or_interrupts_foreign_motion() {
    let mut h = mk("w5", true, [1353, 512]).homed();
    h.dev.clear_log();
    h.dev.ext_move(Axis::Pan, 2000); // the Reolink app jogs
    // a request that races in before the worker noticed is dropped, no STOP
    h.m.relative_move(Lens::Wide, 0.1, 0.0, None).unwrap();
    h.run(50);
    assert_eq!(h.dev.stops(), 0, "{:?}", h.dev.log());
    assert!(h.dev.setposes().is_empty());
    assert!(h.m.status().moving, "the app's jog is reported as MOVING");
    // once noticed, requests are refused with a reason
    match h.m.relative_move(Lens::Wide, 0.1, 0.0, None) {
        Err(PtzError::NotReady(s)) => assert!(s.contains("Reolink app"), "{s}"),
        o => panic!("{o:?}"),
    }
    h.m.stop().unwrap();
    h.run(50);
    assert_eq!(h.dev.stops(), 0, "Stop must not touch the app's move");
    // after the jog ends, we serve again
    h.run(1000);
    h.m.relative_move(Lens::Wide, 0.1, 0.0, None).unwrap();
    h.settle();
    assert_eq!(h.dev.violations(), 0);
    // and our own move is still stopped before retargeting
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap();
    h.run(100);
    h.m.relative_move(Lens::Wide, -1.0, 0.0, None).unwrap();
    h.settle();
    assert!(h.dev.stops() >= 1);
}

#[test]
fn r8_hung_move_is_stopped_and_reported_idle() {
    init_log();
    let mut h = mk("r8", true, [1353, 512]).homed();
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap(); // 390 steps
    h.run(50);
    h.dev.st.lock().unwrap().stuck = true; // GET_STATE stuck at 1
    h.dev.clear_log();
    h.run(2000);
    assert!(h.m.status().moving, "still inside the deadline (390/357 s + margin)");
    h.run(3000);
    assert!(h.dev.stops() >= 1, "STOP on expiry");
    assert!(!h.m.status().moving, "reports IDLE");
    assert!(logged("exceeded its deadline"));
}

#[test]
fn r7_one_sided_limit_is_logged() {
    init_log();
    let mut h = mk("r7", true, [1353, 512]).homed();
    *h.rl.lim.lock().unwrap() = PanLimits { left: Some(1000), right: None };
    h.m.relative_move(Lens::Wide, 0.1, 0.0, None).unwrap();
    h.settle();
    assert!(logged("one-sided Reolink pan limit"));
}

#[test]
fn m1_huge_soft_margin_does_not_panic() {
    let h = mk_in(tmpdir("m1"), true, [1353, 512], Some(&|c| c.soft_margin = 5000));
    assert!(matches!(h.m.relative_move(Lens::Wide, 0.1, 0.0, None), Err(PtzError::NotReady(_))));
}

#[test]
fn m3_unparseable_presets_are_moved_aside() {
    let dir = tmpdir("m3");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("presets.json"), b"{garbage").unwrap();
    let h = mk_in(dir.clone(), true, [1353, 512], None).homed();
    assert_eq!(std::fs::read(dir.join("presets.json.bad")).unwrap(), b"{garbage");
    assert_eq!(h.m.presets().len(), 1);
}

#[test]
fn m4_home_preset_headroom_warning() {
    init_log();
    // tilt home 100 is closer to the edge than half the wide VFOV (341 steps)
    let _h = mks("m4", None, None).homed();
    assert!(logged("home preset tilt headroom"));
}

// ------------------------------------------------ review 2 (round 2) tests

fn count_ops(ops: &[Op], f: impl Fn(&Op) -> bool) -> usize {
    ops.iter().filter(|o| f(o)).count()
}

fn no_ptzd_homing(h: &S) {
    let ops = h.ops();
    assert!(!ops.iter().any(|o| matches!(o, Op::Autotest { .. } | Op::InitPos { .. })), "{ops:?}");
}

const SEC: Duration = Duration::from_secs(1);

#[test]
fn b2_device_first_ptzd_adopts_and_t14_counts_one_homing() {
    let mut h = mks("b2a", None, None);
    let dev = h.sim.device();
    dev.autotest_after(3, Duration::ZERO);
    dev.autotest_after(4, Duration::ZERO);
    h.core.home().unwrap();
    no_ptzd_homing(&h);
    assert_eq!(h.pos(), [1353, 100]);
    // T14: exactly one homing run per axis, by `device`.
    assert_eq!((h.sim.autotest_runs(3), h.sim.autotest_runs(4)), (1, 1));
    assert!(h.m.is_ready());
}

#[test]
fn b2_ptzd_first_waits_for_a_late_device_even_staggered() {
    let mut h = mks("b2b", None, None);
    let dev = h.sim.device();
    // pan calibrates and idles (~9 s) before tilt even starts (12 s)
    dev.autotest_after(3, 5 * SEC);
    dev.autotest_after(4, 12 * SEC);
    h.core.home().unwrap();
    no_ptzd_homing(&h);
    assert_eq!(h.pos(), [1353, 100]);
    assert_eq!((h.sim.autotest_runs(3), h.sim.autotest_runs(4)), (1, 1));
}

#[test]
fn b2_saved_position_is_not_used_while_device_may_still_home() {
    let dir = tmpdir("b2c");
    let store = crate::store::Store::new(&dir);
    store.write_pos([2000, 300]).unwrap();
    store.write_marker().unwrap();
    let mut h = mks_in(dir, None, None);
    h.sim.device().autotest_after(3, 20 * SEC);
    h.sim.device().autotest_after(4, 20 * SEC);
    h.core.home().unwrap();
    no_ptzd_homing(&h);
    assert_eq!(h.pos(), [1353, 100], "device's position, not the saved one");
}

#[test]
fn b2_nothing_calibrates_in_the_wait_then_ptzd_homes_itself() {
    let mut h = mks("b2d", None, Some(&|c| c.device_home_wait = Duration::from_secs(5)));
    let t0 = h.clock.now();
    h.core.home().unwrap();
    assert!(h.clock.now() - t0 >= Duration::from_secs(5), "waited first");
    assert_eq!(count_ops(&h.ops(), |o| matches!(o, Op::Autotest { .. })), 2);
    assert_eq!((h.sim.autotest_runs(3), h.sim.autotest_runs(4)), (1, 1));
}

#[test]
fn b2_marker_init_pos_only_after_the_wait() {
    let dir = tmpdir("b2e");
    let store = crate::store::Store::new(&dir);
    store.write_pos([2000, 300]).unwrap();
    store.write_marker().unwrap();
    let mut h = mks_in(dir, None, Some(&|c| c.device_home_wait = Duration::from_secs(5)));
    let t0 = h.clock.now();
    h.core.home().unwrap();
    assert!(h.clock.now() - t0 >= Duration::from_secs(5));
    assert!(h.ops().contains(&Op::InitPos { motor: 3, pos: 2000 }));
    assert_eq!(h.pos(), [2000, 300]);
}

#[test]
fn b2_position_must_be_stable_before_it_is_adopted() {
    // device homes (idle ~4 s after start), then a goto starts at 5 s and
    // ends ~5.9 s: the first idle position must not be adopted.
    let mut h = mks("b2f", None, None);
    let dev = h.sim.device();
    dev.autotest_after(3, Duration::ZERO);
    dev.autotest_after(4, Duration::ZERO);
    dev.set_pos_after(3, 2000, 0, 48, 5 * SEC);
    h.core.home().unwrap();
    no_ptzd_homing(&h);
    assert_eq!(h.pos(), [2000, 100]);
    assert!(!h.m.status().moving);
}

#[test]
fn w1_failure_is_reported_as_fatal_so_the_process_can_exit() {
    let dir = tmpdir("w1d");
    let clock = FakeClock::new();
    let dev = Fake::new(clock, true, [900, 300]);
    dev.st.lock().unwrap().stuck = true;
    let mut cfg = MotionConfig::new(&dir);
    cfg.home_idle_timeout = Duration::from_millis(200);
    let m = Motion::new(Box::new(dev), Arc::new(FakeRl::default()), cfg);
    let mut e = None;
    for _ in 0..300 {
        e = m.fatal();
        if e.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(e.unwrap_or_default().contains("deadline"));
    m.shutdown();
}

#[test]
fn w5_stop_is_filtered_to_our_own_axes() {
    let mut h = mk("w5b", true, [1353, 512]).homed();
    h.m.relative_move(Lens::Wide, 1.0, 0.0, None).unwrap(); // pan only
    h.run(50);
    h.dev.ext_move(Axis::Tilt, 900); // the app jogs tilt meanwhile
    h.dev.clear_log();
    h.m.relative_move(Lens::Wide, -1.0, 0.0, None).unwrap(); // retarget: STOP our axis
    h.run(50);
    h.m.stop().unwrap();
    h.run(50);
    let log = h.dev.log();
    assert!(log.contains(&Ev::Stop(Axis::Pan)), "{log:?}");
    assert!(!log.contains(&Ev::Stop(Axis::Tilt)), "tilt jog was STOPped: {log:?}");
}

#[test]
fn r1_foreign_retarget_in_the_reversing_direction_is_stopped() {
    init_log();
    let mut h = mks("r1", Some([1353, 512]), None).homed();
    h.m.relative_move(Lens::Wide, -1.0, 0.0, Some(1.0)).unwrap(); // down to 963
    h.core.pump();
    // `device` retargets upward into the running downward move: the driver
    // would run to the end stop (400 steps of overtravel)
    h.sim.device().set_pos_after(3, 2000, 0, 48, Duration::from_millis(200));
    h.run(3000);
    let min = h.sim.clone().min_cur_seen(3);
    assert_eq!(min, 0, "overtravel: ran into the end stop");
    assert!(h.pos()[0] > 900, "ran on to {:?}", h.pos());
    assert!(logged("retargeted"));
    assert!(h.ops().iter().any(|o| matches!(o, Op::Stop { motor: 3 })));
    h.settle();
    // moves are accepted again
    h.m.relative_move(Lens::Wide, 0.1, 0.0, None).unwrap();
    h.settle();
}

#[test]
fn device_stop_then_goto_ends_our_move_as_interrupted_not_retried() {
    init_log();
    let mut h = mks("r1b", Some([1353, 512]), None).homed();
    h.m.relative_move(Lens::Wide, 1.0, 0.0, Some(1.0)).unwrap(); // up to 1743
    h.core.pump();
    let dev = h.sim.device();
    dev.stop_after(3, Duration::from_millis(100));
    dev.set_pos_after(3, 600, 1, 8, Duration::from_millis(150)); // slow goto, long
    h.run(500);
    assert!(logged("interrupted, not retried"));
    let mine = |h: &S| count_ops(&h.ops(), |o| matches!(o, Op::SetPos { motor: 3, .. }));
    assert_eq!(mine(&h), 1);
    // the foreign goto is classified as foreign
    assert!(h.m.status().moving, "MOVING during the foreign move");
    match h.m.relative_move(Lens::Wide, 0.1, 0.0, None) {
        Err(PtzError::NotReady(s)) => assert!(s.contains("Reolink app"), "{s}"),
        o => panic!("{o:?}"),
    }
    h.run(500);
    assert_eq!(mine(&h), 1, "never re-issued");
    h.run(12_000);
    assert!(!h.m.status().moving);
    h.m.relative_move(Lens::Wide, 0.1, 0.0, None).unwrap();
    h.settle();
    assert_eq!(mine(&h), 2);
}

#[test]
fn queued_move_is_dropped_at_shutdown() {
    let mut h = mk("shut", true, [1353, 512]).homed();
    h.dev.clear_log();
    h.m.relative_move(Lens::Wide, 0.5, 0.0, None).unwrap();
    h.m.shutdown();
    h.core.pump();
    assert!(h.dev.setposes().is_empty(), "{:?}", h.dev.log());
}

#[test]
fn w2_status_follows_a_foreign_move() {
    let mut h = mks("stale", Some([1353, 512]), None).homed();
    let before = h.m.status().pan;
    h.sim.device().set_pos(3, 1800, 0, 48).unwrap();
    h.run(2000);
    let after = h.m.status().pan;
    assert!(before < 0.01 && after > 0.3 && after < 0.4, "{before} -> {after}");
}

#[test]
fn w2_set_preset_reads_the_live_position() {
    let dir = tmpdir("livepre");
    let clock = FakeClock::new();
    let dev = Fake::new(clock.clone(), true, [1353, 512]);
    let mut cfg = MotionConfig::new(&dir);
    cfg.adopt_stable_interval = Duration::from_millis(1);
    cfg.idle_tick = Duration::from_secs(3600); // no background refresh
    let m = Motion::new(Box::new(dev.clone()), Arc::new(FakeRl::default()), cfg);
    for _ in 0..300 {
        if m.is_ready() {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(m.is_ready());
    dev.ext_move(Axis::Pan, 2000);
    clock.advance(5000); // the app's jog is over, ptzd has not looked
    let tok = m.set_preset(Some("framed"), None).unwrap();
    let p = m.presets().into_iter().find(|p| p.token == tok).unwrap();
    assert!(p.pan > 0.4, "stored the stale pan {}", p.pan);
    m.shutdown();
}

#[test]
fn r8_deadline_errs_long_at_three_times_the_computed_time() {
    let h = mk("r8b", true, [1353, 512]);
    let c = MotionConfig::new("x");
    let d = h.core.travel_budget(714, 48) - c.move_margin;
    let nominal = 714.0 / c.step_rates[3];
    assert!(d.as_secs_f64() >= 3.0 * nominal - 1e-6, "{d:?} vs {nominal}");
}

#[test]
fn config_validation_rejects_nonsense() {
    assert!(MotionConfig::new("x").validate().is_ok());
    type Tweak = Box<dyn Fn(&mut MotionConfig)>;
    let bad: Vec<Tweak> = vec![
        Box::new(|c| c.wide.h = f64::NAN),
        Box::new(|c| c.wide.v = 0.0),
        Box::new(|c| c.tele.h = -3.0),
        Box::new(|c| c.tele.v = f64::INFINITY),
        Box::new(|c| c.wide.h = 400.0),
        Box::new(|c| c.pan_steps_per_deg = f64::INFINITY),
        Box::new(|c| c.tilt_steps_per_deg = -1.0),
        Box::new(|c| c.step_rates[2] = 0.0),
        Box::new(|c| c.move_slack = f64::NAN),
        Box::new(|c| c.move_margin = Duration::ZERO),
        Box::new(|c| c.soft_margin = 0),
        Box::new(|c| c.backlash_tilt = 1600),
        Box::new(|c| c.poll = Duration::ZERO),
    ];
    for (i, f) in bad.iter().enumerate() {
        let mut c = MotionConfig::new("x");
        f(&mut c);
        assert!(c.validate().is_err(), "case {i}");
    }
}
