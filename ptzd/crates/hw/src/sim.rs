//! Simulator of the VERIFIED lens_zfi driver behaviour, including the
//! hazards. Lazy: every call first advances the model to `clock.now()`.
//! Sources: plan §2.2, plan review B1/B2/W4, research/ptzd-hardware.md
//! §3 and §10.
use crate::abi::*;
use crate::clock::{Clock, RealClock};
use crate::raw::RawZfi;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

/// -14 from INIT_POS when the motor is not enabled (hw doc 0x2be8).
const EFAULT_ENABLED: i32 = 14;

fn errno(e: i32) -> io::Error {
    io::Error::from_raw_os_error(e)
}

#[derive(Clone, Copy, Debug)]
pub struct MotorCfg {
    pub total: u32,
    pub step: u32,
    /// Steps past min before cur=min (plan §2.2: 400 pan, 80 tilt).
    pub overtravel: i64,
    /// AUTOTEST final goto target (plan §2.2: pan 1353, tilt 100).
    pub home: i64,
}

#[derive(Clone, Copy, Debug)]
pub struct SimConfig {
    pub pan: MotorCfg,
    pub tilt: MotorCfg,
    /// Timer tick: g_timer_interval 1400 us (hw doc §3, start_timer 0x2264).
    pub tick_ns: u64,
    /// Ticks per step for speed classes 6/5/4/0 (plan §2.2: dividers
    /// 8/4/2/1). UNPROVEN (T5): real steps/s per class; one gonext is
    /// assumed to be one step, and class 0 ramping is not modelled.
    pub class_div: [u32; 4],
    /// AUTOTEST homing-seek duration in ticks. UNPROVEN (T5/T10).
    pub autotest_seek_ticks: u64,
}

impl Default for SimConfig {
    fn default() -> Self {
        SimConfig {
            pan: MotorCfg { total: 2701, step: 16, overtravel: 400, home: 1353 },
            tilt: MotorCfg { total: 1025, step: 16, overtravel: 80, home: 100 },
            tick_ns: 1_400_000,
            class_div: [8, 4, 2, 1],
            autotest_seek_ticks: 1500,
        }
    }
}

/// speed field -> ticks per step (plan §2.2: <=15 class 6, 16..31 class 5,
/// 32..47 class 4, >=48 class 0).
pub fn speed_div(cfg: &SimConfig, speed: u32) -> u32 {
    match speed {
        0..=15 => cfg.class_div[0],
        16..=31 => cfg.class_div[1],
        32..=47 => cfg.class_div[2],
        _ => cfg.class_div[3],
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Goto,
    /// AUTOTEST homing seek: not interruptible by STOP (hw doc H9).
    Seek(u64),
}

#[derive(Clone, Debug)]
struct Motor {
    cfg: MotorCfg,
    cur: i64,
    target: i64,
    /// Latched at idle only (review B1): 1 = down.
    dir: u32,
    div: u32,
    counter: u32,
    phase: Phase,
    calibrated: bool,
    /// dev+0x4 enable flag; INIT_POS needs it (hw doc 0x2be8).
    enabled: bool,
    /// Move range [+0x68,+0x6c]; INIT_POS clamps to it (hw doc 0x2c0c).
    range: (i64, i64),
    stop_req: bool,
    backlash: u32,
    /// Lowest cur seen (tests: proves overtravel / its absence).
    min_cur_seen: i64,
}

impl Motor {
    fn new(cfg: MotorCfg) -> Self {
        Motor {
            cfg,
            cur: 0,
            target: 0,
            dir: 0,
            div: 1,
            counter: 0,
            phase: Phase::Idle,
            calibrated: false,
            enabled: true,
            range: (0, cfg.total as i64 - 1),
            stop_req: false,
            backlash: 0,
            min_cur_seen: 0,
        }
    }
    fn max(&self) -> i64 {
        self.cfg.total as i64 - 1
    }
    fn moving(&self) -> bool {
        self.phase != Phase::Idle
    }
    fn latch_start(&mut self, pos: i64, speed: u32, sc: &SimConfig) {
        self.dir = (pos <= self.cur) as u32;
        self.div = speed_div(sc, speed);
        self.counter = 0;
        self.phase = Phase::Goto;
    }

    fn tick(&mut self, sc: &SimConfig) {
        match self.phase {
            Phase::Idle => return,
            Phase::Seek(left) => {
                if left <= 1 {
                    // calibrated is set BEFORE the final goto (plan §2.2).
                    self.calibrated = true;
                    self.cur = 0;
                    self.target = self.cfg.home;
                    self.dir = 0;
                    self.div = sc.class_div[3]; // full speed
                    self.counter = 0;
                    self.phase = Phase::Goto;
                } else {
                    self.phase = Phase::Seek(left - 1);
                }
                return;
            }
            Phase::Goto => {}
        }
        if self.stop_req {
            // STOP: idle on the next tick, no deceleration (review W4).
            self.stop_req = false;
            self.target = self.cur;
            self.phase = Phase::Idle;
            return;
        }
        self.counter += 1;
        if self.counter < self.div {
            return;
        }
        self.counter = 0;
        let over = self.cfg.overtravel;
        if self.cur > self.target && self.dir == 0 {
            // Reversed downward move at dir=0: halts at once (0x1d28).
            self.target = self.cur;
            self.phase = Phase::Idle;
        } else if self.cur < self.target && self.dir == 0 {
            self.cur += 1;
            if self.cur >= self.target {
                self.phase = Phase::Idle;
            }
        } else if self.cur == self.target && self.target > 0 {
            self.phase = Phase::Idle;
        } else {
            // Stepping down: dir==1 and (cur>target, or cur<target
            // reversal that ignores dir, review B1), or target==min.
            let to_min = self.target <= 0 || self.cur < self.target;
            self.cur -= 1;
            self.min_cur_seen = self.min_cur_seen.min(self.cur);
            if to_min {
                if self.cur <= -over {
                    self.cur = 0; // cur = min after overtravel
                    self.target = 0;
                    self.phase = Phase::Idle;
                }
            } else if self.cur <= self.target {
                self.phase = Phase::Idle;
            }
        }
    }
}

/// One recorded ioctl issued through the `RawZfi` path (ptzd's own calls;
/// the `DeviceHandle` writer is not recorded).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    SetPos { motor: u32, pos: u32, dir: u32 },
    Stop { motor: u32 },
    InitPos { motor: u32, pos: u32 },
    Autotest { motor: u32 },
}

struct Sim<C: Clock> {
    clock: C,
    ops: Vec<Op>,
    sc: SimConfig,
    m: [Motor; 2],
    ticks_done: u64,
    t0: std::time::Duration,
    oopsed: bool,
    /// Scheduled `device` actions (absolute clock time), applied at the tick
    /// they fall due so a lazy sync cannot reorder them against motion.
    sched: Vec<(std::time::Duration, Sched)>,
    /// AUTOTEST starts per motor from any writer (T14: boot homing count).
    autotest_runs: [u32; 2],
}

#[derive(Clone, Copy, Debug)]
enum Sched {
    Autotest(usize),
    SetPos(usize, SetPosArg),
    Stop(usize),
}

/// The simulated kernel driver. Clones share one device, like several
/// processes opening /dev/lens_zfi (plan §9 D1: `device` is a second writer).
pub struct KernelSim<C: Clock> {
    s: Arc<Mutex<Sim<C>>>,
}

impl<C: Clock> Clone for KernelSim<C> {
    fn clone(&self) -> Self {
        KernelSim { s: self.s.clone() }
    }
}

/// Second writer: issues the same ioctls as the Reolink `device` process,
/// on the same simulated driver. Not recorded in `ops()`.
pub struct DeviceHandle<C: Clock> {
    s: Arc<Mutex<Sim<C>>>,
}

impl KernelSim<RealClock> {
    /// Real-time mode for the mock daemon.
    pub fn realtime() -> Self {
        Self::new(RealClock::new(), SimConfig::default())
    }
}

/// SET_POS on one motor as the driver does it (hw doc §3, review B1).
fn set_pos_motor(m: &mut Motor, arg: &SetPosArg, sc: &SimConfig) {
    let max = m.max();
    let pos = arg.pos as i64;
    if !m.calibrated {
        // set_pos_by_dir runs BEFORE the idle check (hw doc §3 "uncalibrated
        // SET_POS"): it fakes cur/target and ignores pos.
        match arg.dir {
            0 => {
                m.cur = 0;
                m.target = max;
            }
            1 => {
                m.cur = max;
                m.target = 0;
            }
            _ => return, // by_dir -1, ioctl still returns 0
        }
        if m.moving() {
            return; // mid-move rewrite of cur/target, position lost
        }
        // Idle path: dir = pos<=cur (unclamped pos). {pos<=0,dir=0} gives
        // cur=1, dir=1 and drives DOWN (hw doc §3).
        if pos <= m.cur {
            if m.cur == 0 {
                m.cur = 1;
            }
            m.latch_start(pos, arg.speed, sc);
            m.dir = 1;
        } else {
            m.latch_start(pos, arg.speed, sc);
        }
        return;
    }
    let target = pos.min(max);
    if m.moving() {
        // Live SET_POS: only the target changes; dir and speed stay latched
        // (review B1).
        m.target = target;
        return;
    }
    m.target = target;
    m.latch_start(target, arg.speed, sc);
}

/// STOP on an idle or goto motor (not AUTOTEST seeks).
fn stop_motor(m: &mut Motor) {
    if m.phase == Phase::Goto {
        m.target = m.cur;
        m.stop_req = false;
        m.phase = Phase::Idle;
    }
}

/// Starts AUTOTEST when idle; true if it started.
fn start_autotest(m: &mut Motor, sc: &SimConfig) -> bool {
    if m.moving() {
        return false;
    }
    // hw doc §3: AUTOTEST clears calibrated (0x31a8); it is set again only when
    // the seek completes (plan §2.2).
    m.calibrated = false;
    m.counter = 0;
    m.phase = Phase::Seek(sc.autotest_seek_ticks.max(1));
    true
}

impl<C: Clock> Sim<C> {
    fn new(clock: C, sc: SimConfig) -> Self {
        let t0 = clock.now();
        Sim {
            ops: Vec::new(),
            clock,
            sc,
            m: [Motor::new(sc.pan), Motor::new(sc.tilt)],
            ticks_done: 0,
            t0,
            oopsed: false,
            sched: Vec::new(),
            autotest_runs: [0; 2],
        }
    }

    fn sync(&mut self) {
        let el = self.clock.now().saturating_sub(self.t0).as_nanos() as u64;
        let want = el / self.sc.tick_ns;
        while self.ticks_done < want {
            self.run_sched(self.t0 + std::time::Duration::from_nanos(self.ticks_done * self.sc.tick_ns));
            self.ticks_done += 1;
            let sc = self.sc;
            for m in self.m.iter_mut() {
                m.tick(&sc);
            }
        }
    }

    fn run_sched(&mut self, t: std::time::Duration) {
        if self.sched.is_empty() {
            return;
        }
        let (due, rest): (Vec<_>, Vec<_>) = self.sched.drain(..).partition(|(at, _)| *at <= t);
        self.sched = rest;
        let sc = self.sc;
        for (_, op) in due {
            match op {
                Sched::Autotest(i) => {
                    if start_autotest(&mut self.m[i], &sc) {
                        self.autotest_runs[i] += 1;
                    }
                }
                Sched::SetPos(i, a) => set_pos_motor(&mut self.m[i], &a, &sc),
                Sched::Stop(i) => stop_motor(&mut self.m[i]),
            }
        }
    }

    /// Kernel behaviour for an index outside 3/4: 0..=2 are NULL slots
    /// (-EFAULT, review B2); 5/6 and up oops the kernel (plan §2.2).
    fn slot(&mut self, motor: u32) -> io::Result<usize> {
        match motor {
            3 => Ok(0),
            4 => Ok(1),
            0..=2 => Err(errno(EFAULT)),
            _ => {
                self.oopsed = true;
                Err(errno(EFAULT))
            }
        }
    }

    // ---- test/inspection helpers (not part of RawZfi)
    /// True if any ioctl was given a motor index that would oops the kernel.
    pub fn oopsed(&self) -> bool {
        self.oopsed
    }
    /// Internal cur (can be negative during overtravel).
    pub fn raw_cur(&mut self, motor: u32) -> i64 {
        self.sync();
        self.m[(motor == 4) as usize].cur
    }
    pub fn raw_target(&mut self, motor: u32) -> i64 {
        self.sync();
        self.m[(motor == 4) as usize].target
    }
    /// Lowest cur ever reached (negative means overtravel happened).
    pub fn min_cur_seen(&mut self, motor: u32) -> i64 {
        self.sync();
        self.m[(motor == 4) as usize].min_cur_seen
    }
    pub fn backlash(&self, motor: u32) -> u32 {
        self.m[(motor == 4) as usize].backlash
    }
}

impl<C: Clock> RawZfi for Sim<C> {
    fn get_info(&mut self, arg: &mut InfoArg) -> io::Result<()> {
        let i = self.slot(arg.motor)?;
        let c = self.m[i].cfg;
        // Plan §2.2: {motor,total,min,step}; min is 0.
        *arg = InfoArg { motor: arg.motor, total: c.total, min: 0, step: c.step };
        Ok(())
    }

    fn get_state(&mut self, motor: u32) -> io::Result<i32> {
        self.sync();
        let i = self.slot(motor)?;
        Ok(self.m[i].moving() as i32)
    }

    fn get_pos(&mut self, arg: &mut PosArg) -> io::Result<()> {
        self.sync();
        let i = self.slot(arg.motor)?;
        // UNPROVEN: GET_POS during overtravel is reported clamped at 0.
        arg.pos = self.m[i].cur.clamp(0, u32::MAX as i64) as u32;
        Ok(())
    }

    fn set_pos(&mut self, arg: &SetPosArg) -> io::Result<()> {
        self.sync();
        let i = self.slot(arg.motor)?;
        let sc = self.sc;
        set_pos_motor(&mut self.m[i], arg, &sc);
        Ok(())
    }

    fn stop(&mut self, motor: u32) -> io::Result<()> {
        self.sync();
        let i = self.slot(motor)?;
        let m = &mut self.m[i];
        match m.phase {
            Phase::Idle => Ok(()),
            // Does not stop states 5/6; the kernel waits <= 1 s then -16.
            // The sim returns -16 at once (cannot sleep on a manual clock).
            Phase::Seek(_) => Err(errno(EBUSY)),
            Phase::Goto => {
                // The kernel waits for the next tick; model it as done.
                m.target = m.cur;
                m.stop_req = false;
                m.phase = Phase::Idle;
                Ok(())
            }
        }
    }

    fn init_pos(&mut self, arg: &InitPosArg) -> io::Result<()> {
        self.sync();
        let i = self.slot(arg.motor)?;
        let m = &mut self.m[i];
        // hw doc §2 INIT_POS: 0x2be8 needs enabled (else -14), 0x2bf8
        // needs idle (else -16), 0x2c0c sets calibrated=1 and
        // cur=target=clamp(pos, move range [+0x68,+0x6c]).
        if !m.enabled {
            return Err(errno(EFAULT_ENABLED));
        }
        if m.moving() {
            return Err(errno(EBUSY));
        }
        let p = (arg.pos as i64).clamp(m.range.0.min(m.range.1), m.range.1.max(m.range.0));
        m.cur = p;
        m.target = p;
        m.calibrated = true;
        Ok(())
    }

    fn autotest(&mut self, buf: &mut [u32; AUTOTEST_BUF_WORDS]) -> io::Result<()> {
        self.sync();
        // Index is word 0; slots other than 3/4 are NULL -> -EFAULT, no
        // motion (review B2).
        let i = self.slot(buf[0])?;
        let sc = self.sc;
        if start_autotest(&mut self.m[i], &sc) {
            self.autotest_runs[i] += 1;
        }
        Ok(())
    }

    fn get_calibration_state(&mut self, arg: &mut MotorVal) -> io::Result<()> {
        self.sync();
        let i = self.slot(arg.motor)?;
        arg.val = self.m[i].calibrated as u32;
        Ok(())
    }

    fn set_backlash(&mut self, arg: &MotorVal) -> io::Result<()> {
        let i = self.slot(arg.motor)?;
        // UNPROVEN: effect on motion is not modelled.
        self.m[i].backlash = arg.val;
        Ok(())
    }
}

fn lock<C: Clock>(s: &Mutex<Sim<C>>) -> MutexGuard<'_, Sim<C>> {
    s.lock().unwrap_or_else(|e| e.into_inner())
}

impl<C: Clock> KernelSim<C> {
    pub fn new(clock: C, sc: SimConfig) -> Self {
        KernelSim { s: Arc::new(Mutex::new(Sim::new(clock, sc))) }
    }
    /// A second writer on the same device (plan §9 D1).
    pub fn device(&self) -> DeviceHandle<C> {
        DeviceHandle { s: self.s.clone() }
    }
    pub fn oopsed(&self) -> bool {
        lock(&self.s).oopsed()
    }
    pub fn raw_cur(&mut self, motor: u32) -> i64 {
        lock(&self.s).raw_cur(motor)
    }
    pub fn raw_target(&mut self, motor: u32) -> i64 {
        lock(&self.s).raw_target(motor)
    }
    pub fn min_cur_seen(&mut self, motor: u32) -> i64 {
        lock(&self.s).min_cur_seen(motor)
    }
    pub fn backlash(&self, motor: u32) -> u32 {
        lock(&self.s).backlash(motor)
    }
    /// Every ioctl issued through the `RawZfi` path, in order.
    pub fn ops(&self) -> Vec<Op> {
        lock(&self.s).ops.clone()
    }
    /// Forget recorded ops (test setup).
    pub fn clear_ops(&mut self) {
        lock(&self.s).ops.clear();
    }
    /// SET_MOTOR_MOVE_RANGE effect (hw doc 0x300c): INIT_POS clamps to
    /// it; calibrated SET_POS does not. `device` sets the pan range at boot
    /// (0x69044, exact bounds unknown), so tests choose their own.
    pub fn set_move_range(&mut self, motor: u32, lo: i64, hi: i64) {
        lock(&self.s).m[(motor == 4) as usize].range = (lo, hi);
    }
    /// Clear the enable flag (INIT_POS then fails with -14).
    pub fn set_enabled(&mut self, motor: u32, on: bool) {
        lock(&self.s).m[(motor == 4) as usize].enabled = on;
    }
    /// AUTOTEST starts seen on this motor from any writer (T14).
    pub fn autotest_runs(&mut self, motor: u32) -> u32 {
        lock(&self.s).autotest_runs[(motor == 4) as usize]
    }
    pub fn calibrated(&mut self, motor: u32) -> bool {
        let mut s = lock(&self.s);
        s.sync();
        s.m[(motor == 4) as usize].calibrated
    }
}

impl<C: Clock> RawZfi for KernelSim<C> {
    fn get_info(&mut self, arg: &mut InfoArg) -> io::Result<()> {
        lock(&self.s).get_info(arg)
    }
    fn get_state(&mut self, motor: u32) -> io::Result<i32> {
        lock(&self.s).get_state(motor)
    }
    fn get_pos(&mut self, arg: &mut PosArg) -> io::Result<()> {
        lock(&self.s).get_pos(arg)
    }
    fn set_pos(&mut self, arg: &SetPosArg) -> io::Result<()> {
        let mut s = lock(&self.s);
        s.ops.push(Op::SetPos { motor: arg.motor, pos: arg.pos, dir: arg.dir });
        s.set_pos(arg)
    }
    fn stop(&mut self, motor: u32) -> io::Result<()> {
        let mut s = lock(&self.s);
        s.ops.push(Op::Stop { motor });
        s.stop(motor)
    }
    fn init_pos(&mut self, arg: &InitPosArg) -> io::Result<()> {
        let mut s = lock(&self.s);
        s.ops.push(Op::InitPos { motor: arg.motor, pos: arg.pos });
        s.init_pos(arg)
    }
    fn autotest(&mut self, buf: &mut [u32; AUTOTEST_BUF_WORDS]) -> io::Result<()> {
        let mut s = lock(&self.s);
        s.ops.push(Op::Autotest { motor: buf[0] });
        s.autotest(buf)
    }
    fn get_calibration_state(&mut self, arg: &mut MotorVal) -> io::Result<()> {
        lock(&self.s).get_calibration_state(arg)
    }
    fn set_backlash(&mut self, arg: &MotorVal) -> io::Result<()> {
        lock(&self.s).set_backlash(arg)
    }
}

impl<C: Clock> DeviceHandle<C> {
    /// SET_POS as `device` sends it (no guard, no idle check from our side).
    pub fn set_pos(&self, motor: u32, pos: u32, dir: u32, speed: u32) -> io::Result<()> {
        lock(&self.s).set_pos(&SetPosArg { motor, pos, dir, speed })
    }
    /// AUTOTEST as `device` runs it at boot (need_pt_autotest).
    pub fn autotest(&self, motor: u32) -> io::Result<()> {
        let mut b = [0u32; AUTOTEST_BUF_WORDS];
        b[0] = motor;
        lock(&self.s).autotest(&mut b)
    }
    pub fn stop(&self, motor: u32) -> io::Result<()> {
        lock(&self.s).stop(motor)
    }
    /// `device` boot homing that starts `delay` from now (clock domain of the sim).
    pub fn autotest_after(&self, motor: u32, delay: std::time::Duration) {
        let mut s = lock(&self.s);
        let at = s.clock.now() + delay;
        let i = (motor == 4) as usize;
        s.sched.push((at, Sched::Autotest(i)));
    }
    /// A `device` SET_POS (goto, retarget) `delay` from now.
    pub fn set_pos_after(&self, motor: u32, pos: u32, dir: u32, speed: u32, delay: std::time::Duration) {
        let mut s = lock(&self.s);
        let at = s.clock.now() + delay;
        let i = (motor == 4) as usize;
        s.sched.push((at, Sched::SetPos(i, SetPosArg { motor, pos, dir, speed })));
    }
    /// A `device` STOP `delay` from now (the RE: it STOPs, then SET_POS).
    pub fn stop_after(&self, motor: u32, delay: std::time::Duration) {
        let mut s = lock(&self.s);
        let at = s.clock.now() + delay;
        let i = (motor == 4) as usize;
        s.sched.push((at, Sched::Stop(i)));
    }
    pub fn init_pos(&self, motor: u32, pos: u32) -> io::Result<()> {
        lock(&self.s).init_pos(&InitPosArg { motor, pos, ..Default::default() })
    }
}
