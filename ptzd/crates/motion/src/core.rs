//! Worker: owns the MotorDev. Single-threaded state machine, driven by
//! `run()` (real thread) or `pump()` (tests).
use std::sync::atomic::Ordering;
use std::io;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Duration;

use ptzd_core::{Axis, MotorDev, RawSpeed, ReolinkState};

use crate::clock::Clock;
use crate::config::MotionConfig;
use crate::store::{Store, StoredPreset};
use crate::{aim, clamp_to, from_generic, ix, sgn, usable, Range, Shared, EXPECT_INFO};

/// errno EBUSY (hw doc §2: INIT_POS/AUTOTEST need idle, -16).
const EBUSY: i32 = 16;

pub(crate) enum Kind {
    /// Image angles in degrees, turned into steps at the position read at
    /// issue time.
    Rel { right: f64, up: f64 },
    Abs { pan: f32, tilt: f32 },
    Steps { pan: u32, tilt: u32 },
    /// dir per axis in steps sign: -1 low edge, +1 high edge, 0 untouched.
    Cont { dir: [i8; 2], timeout: Option<Duration> },
}

pub(crate) struct MoveReq {
    pub kind: Kind,
    pub speed: RawSpeed,
}

pub(crate) enum Cmd {
    Move(MoveReq),
    Stop,
    Rehome,
    Shutdown,
    /// Fresh GET_POS for `set_preset` (review 2 W2).
    Pos(std::sync::mpsc::Sender<Option<[u32; 2]>>),
}

/// Steps past the target, or backwards from the best progress so far, that
/// still count as our own move. Beyond it `device` retargeted us (review 2
/// R1). A move needs 1 step per 1.4 ms tick at most, so one 10 ms poll is
/// about 7 steps; the soft margin of 20 stays ahead of detection.
// UNPROVEN (T10): class 0 deceleration overshoot on the real driver.
const RETARGET_TOL: i64 = 4;

/// Progress of one owned axis toward its target.
#[derive(Clone, Copy)]
struct Track {
    target: i64,
    /// +1 target above start, -1 below.
    dir: i64,
    /// Smallest remaining distance seen (positive = still to go).
    best: i64,
}

enum Phase {
    Idle,
    /// `cont`: a ContinuousMove, whose expiry is its normal end. For every
    /// other move the deadline is a hang guard (R8) and expiry is an error.
    Moving { deadline: Option<Duration>, cont: bool },
    /// STOP sent, polling until idle, then `pending` is issued (review B1).
    Stopping,
}

pub(crate) struct Core {
    dev: Box<dyn MotorDev>,
    reolink: Arc<dyn ReolinkState>,
    cfg: Arc<MotionConfig>,
    clock: Arc<dyn Clock>,
    shared: Arc<Shared>,
    rx: Receiver<Cmd>,
    store: Store,
    create_home: bool,
    phase: Phase,
    pending: Option<MoveReq>,
    stop_retry: bool,
    marker_present: bool,
    /// Axes ptzd issued SET_POS on and has not seen idle since (W5). ptzd
    /// never STOPs or waits on motion outside this set.
    owned: [bool; 2],
    track: [Option<Track>; 2],
    /// When our own motion last went idle, for the settle (Frigate fit).
    idle_at: Option<Duration>,
    last_rehome_day: Option<u64>,
}

impl Core {
    pub fn new(
        dev: Box<dyn MotorDev>,
        reolink: Arc<dyn ReolinkState>,
        cfg: Arc<MotionConfig>,
        clock: Arc<dyn Clock>,
        shared: Arc<Shared>,
        rx: Receiver<Cmd>,
        create_home: bool,
    ) -> Core {
        let store = Store::new(&cfg.state_dir);
        let marker_present = store.marker_exists();
        Core {
            dev,
            reolink,
            cfg,
            clock,
            shared,
            rx,
            store,
            create_home,
            phase: Phase::Idle,
            pending: None,
            stop_retry: false,
            marker_present,
            owned: [false; 2],
            track: [None; 2],
            idle_at: None,
            last_rehome_day: None,
        }
    }

    /// Thread body: home, then serve commands until shutdown.
    pub fn run(&mut self) {
        match self.home() {
            Ok(()) => {}
            Err(e) => {
                log::error!("homing failed: {e}");
                self.shared.lock().fail = Some(e);
            }
        }
        loop {
            let busy = self.busy();
            let wait = if busy { self.cfg.poll } else { self.cfg.idle_tick };
            match self.rx.recv_timeout(wait) {
                Ok(c) => {
                    if !self.process(c) {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    self.finish();
                    return;
                }
            }
            if !self.drain() {
                return;
            }
            self.tick();
        }
    }

    /// Test driver: process queued commands then one tick.
    #[cfg(test)]
    pub fn pump(&mut self) -> bool {
        let go = self.drain();
        if go {
            self.tick();
        }
        go
    }

    fn drain(&mut self) -> bool {
        while let Ok(c) = self.rx.try_recv() {
            if !self.process(c) {
                return false;
            }
        }
        true
    }

    fn busy(&self) -> bool {
        let i = self.shared.lock();
        !matches!(self.phase, Phase::Idle) || i.moving || i.foreign
    }

    /// false = worker should exit.
    fn process(&mut self, c: Cmd) -> bool {
        match c {
            Cmd::Pos(tx) => {
                let p = self.read_pos();
                if let Some(p) = p {
                    self.shared.lock().pos = p;
                }
                let _ = tx.send(p);
            }
            Cmd::Move(req) => {
                if self.shared.abort.load(Ordering::SeqCst) {
                    // Shutdown is queued behind it: never send a move that
                    // finish() would cut off with an abrupt STOP (review 2 R6).
                    log::info!("move dropped: shutting down");
                } else if self.homed_ok() {
                    if matches!(self.phase, Phase::Idle) {
                        // issue() refuses when the app is moving the head.
                        self.issue(req);
                    } else {
                        // Our own move: STOP, wait idle, then SET_POS (B1).
                        self.stop_all();
                        self.pending = Some(req);
                        self.phase = Phase::Stopping;
                    }
                }
            }
            Cmd::Stop => {
                self.pending = None;
                // Only our own motion is stopped (W5, plan §9 D1).
                if !matches!(self.phase, Phase::Idle) {
                    self.stop_all();
                    self.phase = Phase::Stopping;
                }
            }
            Cmd::Rehome => {
                if matches!(self.phase, Phase::Idle) {
                    self.do_rehome();
                }
            }
            Cmd::Shutdown => {
                self.finish();
                return false;
            }
        }
        true
    }

    fn homed_ok(&self) -> bool {
        let i = self.shared.lock();
        i.homed && i.fail.is_none()
    }

    // ----------------------------------------------------------- device

    fn axis_moving(&mut self, a: Axis) -> bool {
        match self.dev.is_moving(a) {
            Ok(v) => v,
            Err(e) => {
                log::warn!("GET_STATE {a:?}: {e}");
                true // unknown: never SET_POS on it
            }
        }
    }

    fn any_moving(&mut self) -> bool {
        let mut m = false;
        for a in Axis::ALL {
            m |= self.axis_moving(a);
        }
        m
    }

    /// Motion on axes we started.
    fn own_moving(&mut self) -> bool {
        let mut m = false;
        for a in Axis::ALL {
            if self.owned[ix(a)] {
                m |= self.axis_moving(a);
            }
        }
        m
    }

    /// Motion on axes we did not start (the Reolink app, `device`).
    fn foreign_moving(&mut self) -> bool {
        let mut m = false;
        for a in Axis::ALL {
            if !self.owned[ix(a)] {
                m |= self.axis_moving(a);
            }
        }
        m
    }

    /// STOP our own axes only (W5).
    fn stop_all(&mut self) {
        self.stop_retry = false;
        for a in Axis::ALL {
            if !self.owned[ix(a)] {
                continue;
            }
            if let Err(e) = self.dev.stop(a) {
                log::warn!("STOP {a:?}: {e}");
                self.stop_retry = true; // EBUSY after 1 s: try again next tick
            }
        }
    }

    fn own_done(&mut self, now: Duration) {
        self.owned = [false; 2];
        self.track = [None; 2];
        self.idle_at = Some(now);
    }

    fn read_pos(&mut self) -> Option<[u32; 2]> {
        let mut p = [0u32; 2];
        for a in Axis::ALL {
            match self.dev.position(a) {
                Ok(v) => p[ix(a)] = v,
                Err(e) => {
                    log::warn!("GET_POS {a:?}: {e}");
                    return None;
                }
            }
        }
        Some(p)
    }

    fn sample_pos(&mut self) {
        if let Some(p) = self.read_pos() {
            self.shared.lock().pos = p;
        }
    }

    fn ranges(&self) -> Result<[Range; 2], String> {
        let hw = self.shared.lock().range_hw;
        usable(hw, self.reolink.pan_limits(), self.cfg.pan_left_is_low)
    }

    fn remove_marker_file(&mut self) {
        self.remove_marker();
    }

    fn remove_marker(&mut self) {
        if self.marker_present || self.store.marker_exists() {
            if let Err(e) = self.store.remove_marker() {
                log::warn!("remove clean marker: {e}");
            }
            self.marker_present = false;
        }
    }

    // ------------------------------------------------------------ moves

    /// Device is idle here. Limits are re-read per move.
    fn issue(&mut self, req: MoveReq) {
        if self.foreign_moving() {
            // W5: never interrupt a jog we did not start.
            log::warn!("move dropped: camera is being moved by the Reolink app");
            let mut i = self.shared.lock();
            i.moving = false;
            i.foreign = true;
            return;
        }
        let r = match self.ranges() {
            Ok(r) => r,
            Err(e) => {
                log::warn!("move dropped: {e}");
                return;
            }
        };
        let cur = match self.read_pos() {
            Some(c) => c,
            None => return,
        };
        let mut deadline = None;
        let mut cont = false;
        let t: [Option<u32>; 2] = match req.kind {
            Kind::Rel { right, up } => {
                let c = &self.cfg;
                let (sp, st) = (sgn(c.pan_reversed), sgn(c.tilt_reversed));
                let depression = -st * (cur[1] as f64 - c.tilt_level_step) / c.tilt_steps_per_deg;
                let (pan, tilt) = aim(right, up, depression.clamp(-90.0, 90.0));
                let dp = pan * c.pan_steps_per_deg * sp;
                let dt = tilt * c.tilt_steps_per_deg * st;
                [
                    Some(clamp_to(cur[0] as f64 + dp.round(), r[0])),
                    Some(clamp_to(cur[1] as f64 + dt.round(), r[1])),
                ]
            }
            Kind::Abs { pan, tilt } => [
                // reversed flags are applied by the generic mapping in lib
                Some(from_generic(pan, r[0], self.cfg.pan_reversed)),
                Some(from_generic(tilt, r[1], self.cfg.tilt_reversed)),
            ],
            Kind::Steps { pan, tilt } => {
                [Some(clamp_to(pan as f64, r[0])), Some(clamp_to(tilt as f64, r[1]))]
            }
            Kind::Cont { dir, timeout } => {
                cont = true;
                deadline = Some(self.clock.now() + timeout.unwrap_or(self.cfg.continuous_timeout));
                let e = |d: i8, r: Range| match d {
                    0 => None,
                    d if d > 0 => Some(r.1),
                    _ => Some(r.0),
                };
                [e(dir[0], r[0]), e(dir[1], r[1])]
            }
        };
        let mut any = false;
        let mut dist = 0u32;
        for a in Axis::ALL {
            let i = ix(a);
            if let Some(tp) = t[i] {
                if tp == cur[i] {
                    continue;
                }
                match self.dev.set_pos(a, tp, req.speed) {
                    Ok(()) => {
                        any = true;
                        self.owned[i] = true;
                        let dir = if tp > cur[i] { 1 } else { -1 };
                        self.track[i] = Some(Track { target: tp as i64, dir, best: (tp as i64 - cur[i] as i64).abs() });
                        dist = dist.max(tp.abs_diff(cur[i]));
                    }
                    Err(e) => log::warn!("SET_POS {a:?} {tp}: {e}"),
                }
            }
        }
        if any {
            if !cont {
                // R8: hang guard from distance / class rate + margin.
                deadline = Some(self.clock.now() + self.travel_budget(dist, req.speed));
            }
            self.idle_at = None;
            self.phase = Phase::Moving { deadline, cont };
        }
    }

    /// Expected travel time with slack, plus the margin (R8).
    pub(crate) fn travel_budget(&self, dist: u32, speed: RawSpeed) -> Duration {
        // Speed classes per plan §2.2: <=15, 16..31, 32..47, >=48.
        let c = match speed {
            0..=15 => 0,
            16..=31 => 1,
            32..=47 => 2,
            _ => 3,
        };
        let rate = (self.cfg.step_rates[c] / self.cfg.move_slack.max(1.0)).max(1.0);
        Duration::from_secs_f64(dist as f64 / rate) + self.cfg.move_margin
    }

    pub fn tick(&mut self) {
        let now = self.clock.now();
        match self.phase {
            Phase::Stopping => {
                if self.own_moving() {
                    if self.stop_retry {
                        self.stop_all();
                    }
                } else {
                    self.phase = Phase::Idle;
                    self.own_done(now);
                    self.sample_pos();
                    if let Some(req) = self.pending.take() {
                        self.issue(req);
                    }
                }
            }
            Phase::Moving { deadline, cont } => {
                if !self.own_moving() {
                    self.phase = Phase::Idle;
                    self.sample_pos();
                    self.log_interrupted();
                    self.own_done(now);
                } else {
                    self.sample_pos();
                    if self.retargeted() {
                        // Safety beats politeness: a live retarget in the
                        // reversing direction would run to the end stop
                        // (review B1, 0x1d28). STOP now, treat the axis as
                        // foreign-moved, do not re-issue.
                        log::error!("our move was retargeted by another writer (device?): STOP");
                        self.stop_all();
                        self.pending = None;
                        self.phase = Phase::Stopping;
                        return;
                    }
                    if deadline.is_some_and(|d| now >= d) {
                        self.stop_all();
                        if cont {
                            self.phase = Phase::Stopping;
                        } else {
                            // R8: STOP, log, report IDLE. Anything still
                            // moving afterwards counts as foreign.
                            log::error!("move exceeded its deadline, stopped (hung GET_STATE or lost steps?)");
                            self.phase = Phase::Idle;
                            self.own_done(now);
                            let mut i = self.shared.lock();
                            i.moving = false;
                            i.hung = true;
                        }
                    }
                }
            }
            Phase::Idle => {}
        }
        if !matches!(self.phase, Phase::Idle) {
            return;
        }
        let foreign = self.any_moving();
        {
            let mut i = self.shared.lock();
            i.foreign = foreign;
            if !foreign {
                i.hung = false;
            }
        }
        // Review 2 W2: keep the cached position live while the app moves the head.
        if self.shared.lock().homed {
            self.sample_pos();
        }
        // MOVING lasts from acceptance until idle + settle, and at least
        // `dwell` from acceptance for zero-distance moves (plan §4.3).
        {
            let mut i = self.shared.lock();
            let settled = self.idle_at.is_none_or(|t| now >= t + self.cfg.settle);
            if i.moving && now >= i.accepted_at + self.cfg.dwell && settled && !foreign {
                i.moving = false;
                drop(i);
                self.sample_pos();
            }
        }
        if self.shared.lock().moving || foreign {
            return;
        }
        self.maybe_rehome(now);
    }

    /// Position moved away from our target, or past it (review 2 R1).
    fn retargeted(&mut self) -> bool {
        let pos = self.shared.lock().pos;
        let mut hit = false;
        for a in Axis::ALL {
            let i = ix(a);
            let Some(t) = self.track[i].as_mut() else { continue };
            let rem = (t.target - pos[i] as i64) * t.dir;
            if rem < -RETARGET_TOL || rem > t.best + RETARGET_TOL {
                hit = true;
            }
            t.best = t.best.min(rem);
        }
        hit
    }

    /// The `device` STOPs our move before its own goto (research/
    /// ptzd-hardware.md §6): idle short of the target is interruption, not
    /// an error. Never retried.
    fn log_interrupted(&self) {
        let pos = self.shared.lock().pos;
        for (i, t) in self.track.iter().enumerate() {
            if let Some(t) = t {
                if (t.target - pos[i] as i64).abs() > RETARGET_TOL {
                    log::info!("our move ended short of its target on axis {i} ({} vs {}): interrupted, not retried", pos[i], t.target);
                }
            }
        }
    }

    fn maybe_rehome(&mut self, now: Duration) {
        let h = match self.cfg.rehome_hour_utc {
            Some(h) => h,
            None => return,
        };
        let (day, hour) = self.clock.utc_day_hour();
        let (idle_since, ok) = {
            let i = self.shared.lock();
            (i.last_cmd, i.homed && i.fail.is_none())
        };
        if !ok
            || hour != h
            || self.last_rehome_day == Some(day)
            || now < idle_since + self.cfg.rehome_idle
            || !self.reolink.self_movers().enabled.is_empty()
        {
            return;
        }
        self.last_rehome_day = Some(day);
        if !self.do_rehome() {
            self.last_rehome_day = None; // try again next tick
        }
    }

    /// false = not started (the head was moving).
    fn do_rehome(&mut self) -> bool {
        if self.any_moving() {
            log::warn!("re-home postponed: head is moving");
            return false;
        }
        log::info!("re-homing");
        let before = self.read_pos();
        {
            // W4: MOVING for the whole AUTOTEST.
            let mut i = self.shared.lock();
            i.rehoming = true;
            i.moving = true;
            i.accepted_at = self.clock.now();
        }
        self.remove_marker();
        let r = self.autotest_all();
        let now = self.clock.now();
        {
            let mut i = self.shared.lock();
            i.rehoming = false;
            i.accepted_at = now;
        }
        self.idle_at = Some(now);
        match r {
            Ok(()) => {
                self.sample_pos();
                // W4: AUTOTEST ends at cfg home; go back where Frigate left it.
                if let Some(b) = before {
                    self.issue(MoveReq { kind: Kind::Steps { pan: b[0], tilt: b[1] }, speed: self.cfg.fast_speed });
                }
            }
            Err(e) => {
                log::error!("re-home failed: {e}");
                self.shared.lock().fail = Some(e);
            }
        }
        true
    }

    // ----------------------------------------------------------- homing

    fn wait_idle(&mut self, a: Axis) -> Result<(), String> {
        let start = self.clock.now();
        loop {
            self.clock.sleep(self.cfg.poll);
            if self.shared.abort.load(Ordering::SeqCst) {
                return Err("aborted by shutdown".into());
            }
            match self.dev.is_moving(a) {
                Ok(false) => return Ok(()),
                Ok(true) => {}
                Err(e) => return Err(format!("GET_STATE {a:?}: {e}")),
            }
            if self.clock.now() > start + self.cfg.home_timeout {
                return Err(format!("{a:?} homing timed out"));
            }
        }
    }

    /// AUTOTEST per motor, pan then tilt (review B2, plan §4.4 step 3).
    /// Idle (GET_STATE == 0), not the calibrated flag, marks completion (§2.2).
    fn autotest_all(&mut self) -> Result<(), String> {
        for a in Axis::ALL {
            self.dev.autotest(a).map_err(|e| format!("AUTOTEST {a:?}: {e}"))?;
            self.wait_idle(a)?;
            if !self.dev.is_calibrated(a).map_err(|e| format!("calibrated {a:?}: {e}"))? {
                return Err(format!("{a:?} not calibrated after AUTOTEST"));
            }
        }
        Ok(())
    }

    /// Boot order (review 2 B2). `device` homes the motors at boot, so wait
    /// for it. Returns Ok when every axis is calibrated, idle and unchanged
    /// for `adopt_stable_polls` polls, or when the wait ran out with the
    /// head idle (the caller then homes the uncalibrated axes itself). Errors
    /// when the head stays busy for `home_idle_timeout` (W1) or on abort.
    fn await_device(&mut self) -> Result<(), String> {
        let wait_end = self.clock.now() + self.cfg.device_home_wait;
        let slow = self.cfg.poll.max(Duration::from_millis(100));
        let mut busy_since: Option<Duration> = None;
        let mut last: Option<[u32; 2]> = None;
        let mut same = 0u32;
        loop {
            if self.shared.abort.load(Ordering::SeqCst) {
                return Err("aborted by shutdown".into());
            }
            let now = self.clock.now();
            let mut pause = slow;
            if self.any_moving() {
                last = None;
                same = 0;
                let since = *busy_since.get_or_insert(now);
                if now >= since + self.cfg.home_idle_timeout {
                    return Err("head still moving at the homing deadline (Reolink `device` homing?)".into());
                }
            } else {
                busy_since = None;
                let mut cal = true;
                for a in Axis::ALL {
                    cal &= self.dev.is_calibrated(a).map_err(|e| format!("calibrated {a:?}: {e}"))?;
                }
                if cal {
                    let p = self.read_pos();
                    if p.is_some() && p == last {
                        same += 1;
                    } else {
                        same = 0;
                        last = p;
                    }
                    if same >= self.cfg.adopt_stable_polls {
                        return Ok(());
                    }
                    pause = self.cfg.adopt_stable_interval;
                } else {
                    last = None;
                    same = 0;
                    if now >= wait_end {
                        log::warn!("no calibration from `device` within the wait, homing here");
                        return Ok(());
                    }
                }
            }
            self.clock.sleep(pause);
        }
    }

    /// Run a device call, retrying EBUSY (-16, hw doc §2: INIT_POS/AUTOTEST
    /// need idle) until `deadline`. Other errors fail at once.
    fn retry_busy(
        &mut self,
        deadline: Duration,
        what: &str,
        mut f: impl FnMut(&mut dyn MotorDev) -> io::Result<()>,
    ) -> Result<(), String> {
        loop {
            match f(self.dev.as_mut()) {
                Ok(()) => return Ok(()),
                Err(e) if e.raw_os_error() == Some(EBUSY) => {
                    if self.clock.now() >= deadline {
                        return Err(format!("{what}: EBUSY until the homing deadline"));
                    }
                    if self.shared.abort.load(Ordering::SeqCst) {
                        return Err("aborted by shutdown".into());
                    }
                    self.clock.sleep(self.cfg.poll);
                }
                Err(e) => return Err(format!("{what}: {e}")),
            }
        }
    }

    /// Plan §4.4. Blocking; commands queue meanwhile. `device` may be
    /// homing at boot (plan §9 D1), so wait for idle first (W1).
    pub fn home(&mut self) -> Result<(), String> {
        for a in Axis::ALL {
            let i = self.dev.info(a).map_err(|e| format!("GET_INFO {a:?}: {e}"))?;
            let (t, m, s) = EXPECT_INFO[ix(a)];
            if (i.total, i.min, i.step) != (t, m, s) {
                return Err(format!("{a:?} GET_INFO {i:?} != expected {{{t},{m},{s}}}, wrong model or firmware"));
            }
        }
        // W2: the marker is valid for exactly one boot. Consume it now, on
        // every path; shutdown writes a new one only after a clean stop.
        let saved = self.store.read_pos();
        let marker = self.store.marker_exists();
        self.remove_marker_file();
        let hw = self.shared.lock().range_hw;
        self.await_device()?;
        // From here we may write to the motors (INIT_POS, AUTOTEST); EBUSY
        // means `device` woke up meanwhile and is retried for a bounded time.
        let deadline = self.clock.now() + self.cfg.home_idle_timeout;
        let mut auto = [false; 2];
        for a in Axis::ALL {
            let i = ix(a);
            if self.dev.is_calibrated(a).map_err(|e| format!("calibrated {a:?}: {e}"))? {
                log::info!("{a:?} already calibrated, adopting GET_POS");
                continue; // adopt GET_POS below
            }
            match saved {
                Some(p) if marker && p[i] >= hw[i].0 && p[i] <= hw[i].1 => {
                    let want = p[i];
                    log::info!("{a:?} uncalibrated after the wait, INIT_POS {want} from the clean marker");
                    self.retry_busy(deadline, &format!("INIT_POS {a:?}"), |d| d.init_pos(a, want))?;
                    // R2: the driver clamps to the move range; verify.
                    match self.dev.position(a) {
                        Ok(v) if v == want => {}
                        other => {
                            log::warn!("INIT_POS {a:?} {want} read back as {other:?}, falling back to AUTOTEST");
                            auto[i] = true;
                        }
                    }
                }
                _ => {
                    log::info!("{a:?} uncalibrated after the wait, no clean marker: AUTOTEST");
                    auto[i] = true
                }
            }
        }
        for a in Axis::ALL {
            if auto[ix(a)] {
                self.retry_busy(deadline, &format!("AUTOTEST {a:?}"), |d| d.autotest(a))?;
                self.wait_idle(a)?;
            }
        }
        for a in Axis::ALL {
            if !self.dev.is_calibrated(a).map_err(|e| e.to_string())? {
                return Err(format!("{a:?} not calibrated, refusing to serve"));
            }
        }
        self.dev.set_backlash(Axis::Pan, self.cfg.backlash_pan).map_err(|e| format!("SET_BACKLASH pan: {e}"))?;
        self.dev.set_backlash(Axis::Tilt, self.cfg.backlash_tilt).map_err(|e| format!("SET_BACKLASH tilt: {e}"))?;
        let p = self.read_pos().ok_or("GET_POS failed after homing")?;
        self.shared.lock().pos = p;
        if self.create_home {
            // Plan §3: Frigate's default return_preset is "home".
            self.warn_headroom(p, hw);
            let mut i = self.shared.lock();
            i.presets.push(StoredPreset { token: "home".into(), name: "home".into(), pan: p[0], tilt: p[1] });
            if let Err(e) = self.store.write_presets(&i.presets) {
                log::warn!("save presets: {e}");
            }
            self.create_home = false;
        }
        self.shared.lock().homed = true;
        Ok(())
    }

    /// M4: Frigate calibration needs about half a FOV of room each way.
    fn warn_headroom(&self, p: [u32; 2], hw: [Range; 2]) {
        let need = [
            self.cfg.wide.h / 2.0 * self.cfg.pan_steps_per_deg,
            self.cfg.wide.v / 2.0 * self.cfg.tilt_steps_per_deg,
        ];
        for (i, n) in ["pan", "tilt"].iter().enumerate() {
            let lo = p[i].saturating_sub(hw[i].0) as f64;
            let hi = hw[i].1.saturating_sub(p[i]) as f64;
            if lo < need[i] || hi < need[i] {
                log::warn!(
                    "home preset {n} headroom {lo:.0}/{hi:.0} steps is below half a wide FOV ({:.0}); Frigate calibration may hit the limits",
                    need[i]
                );
            }
        }
    }

    // --------------------------------------------------------- shutdown

    /// Plan §7a: STOP our own move, save position, write the clean marker.
    /// R4/W2: the marker only goes out when both axes were already idle, so
    /// a position after an abrupt STOP (step loss UNPROVEN, T10) is never
    /// trusted at the next boot. Position is saved only here (W3).
    pub fn finish(&mut self) {
        let was_idle = matches!(self.phase, Phase::Idle) && self.pending.is_none() && !self.any_moving();
        self.pending = None;
        self.stop_all();
        let start = self.clock.now();
        while self.clock.now() <= start + Duration::from_secs(5) {
            if !self.own_moving() {
                break;
            }
            self.clock.sleep(self.cfg.poll);
        }
        if self.homed_ok() {
            if was_idle {
                if let Some(p) = self.read_pos() {
                    self.shared.lock().pos = p;
                    match self.store.write_pos(p) {
                        Ok(()) => match self.store.write_marker() {
                            Ok(()) => self.marker_present = true,
                            Err(e) => log::warn!("write clean marker: {e}"),
                        },
                        Err(e) => log::warn!("save position: {e}"),
                    }
                }
            } else {
                log::info!("shutdown during motion: no clean marker, next boot re-homes");
                self.remove_marker();
            }
        }
        self.shared.lock().moving = false;
    }
}
