use ptzd_core::{Axis, MotorDev};
use ptzd_hw::abi::*;
use ptzd_hw::{Guarded, GuardedConfig, KernelSim, ManualClock, Op, RawZfi, SimConfig};
use std::mem::size_of;
use std::time::Duration;

const PAN: u32 = 3;
const TILT: u32 = 4;

fn sim() -> (KernelSim<ManualClock>, ManualClock) {
    let c = ManualClock::new();
    (KernelSim::new(c.clone(), SimConfig::default()), c)
}
fn ticks(c: &ManualClock, n: u64) {
    c.advance(Duration::from_nanos(n * SimConfig::default().tick_ns));
}
fn sp(motor: u32, pos: u32, dir: u32, speed: u32) -> SetPosArg {
    SetPosArg { motor, pos, dir, speed }
}
fn init(s: &mut impl RawZfi, motor: u32, pos: u32) {
    s.init_pos(&InitPosArg { motor, pos, ..Default::default() }).unwrap();
}
fn state(s: &mut impl RawZfi, m: u32) -> i32 {
    s.get_state(m).unwrap()
}
fn pos(s: &mut impl RawZfi, m: u32) -> u32 {
    let mut p = PosArg { motor: m, ..Default::default() };
    s.get_pos(&mut p).unwrap();
    p.pos
}
fn errno(e: std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(-1)
}

// ------------------------------------------------------------ ABI table
#[test]
fn abi_numbers_and_sizes() {
    assert_eq!(GET_INFO, 0xc0104c01);
    assert_eq!(GET_STATE, 0xc0044c02);
    assert_eq!(GET_POS, 0xc0104c03);
    assert_eq!(SET_POS, 0xc0104c04);
    assert_eq!(STOP, 0xc0044c06);
    assert_eq!(INIT_POS, 0xc0104c08);
    assert_eq!(MOTOR_AUTOTEST, 0xc0044c0a);
    assert_eq!(GET_CALIBRATION_STATE, 0x40084c15);
    assert_eq!(SET_BACKLASH, 0x40084c10);
    assert_eq!(size_of::<InfoArg>(), 16);
    assert_eq!(size_of::<PosArg>(), 16);
    assert_eq!(size_of::<SetPosArg>(), 16);
    assert_eq!(size_of::<InitPosArg>(), 16);
    assert_eq!(size_of::<StateArg>(), 4);
    assert_eq!(size_of::<MotorVal>(), 8);
    assert_eq!(size_of::<[u32; AUTOTEST_BUF_WORDS]>(), 20);
    // size field of the number matches the struct where it is declared
    assert_eq!((GET_INFO >> 16) & 0x3fff, 16);
    assert_eq!((GET_STATE >> 16) & 0x3fff, 4);
    assert_eq!((GET_CALIBRATION_STATE >> 16) & 0x3fff, 8);
}

#[test]
fn sim_info_matches_plan() {
    let (mut s, _) = sim();
    let mut i = InfoArg { motor: PAN, ..Default::default() };
    s.get_info(&mut i).unwrap();
    assert_eq!((i.total, i.min, i.step), (2701, 0, 16));
    let mut i = InfoArg { motor: TILT, ..Default::default() };
    s.get_info(&mut i).unwrap();
    assert_eq!((i.total, i.min, i.step), (1025, 0, 16));
}

// ------------------------------------------------- simulator hazards (raw)
#[test]
fn sim_overtravel_to_min_then_cur_min() {
    let (mut s, c) = sim();
    init(&mut s, PAN, 100);
    s.set_pos(&sp(PAN, 0, 1, 64)).unwrap();
    ticks(&c, 100);
    assert_eq!(state(&mut s, PAN), 1); // at 0 but still driving into the stop
    ticks(&c, 400);
    assert_eq!(state(&mut s, PAN), 0);
    assert_eq!(s.min_cur_seen(PAN), -400);
    assert_eq!(pos(&mut s, PAN), 0);
    // tilt: 80
    init(&mut s, TILT, 50);
    s.set_pos(&sp(TILT, 0, 1, 64)).unwrap();
    ticks(&c, 200);
    assert_eq!(s.min_cur_seen(TILT), -80);
    assert_eq!(state(&mut s, TILT), 0);
}

#[test]
fn sim_live_set_pos_downward_reversal_runs_to_min() {
    // B1: going down, retarget above cur: dir stays 1, runs into the stop.
    let (mut s, c) = sim();
    init(&mut s, PAN, 1000);
    s.set_pos(&sp(PAN, 500, 1, 64)).unwrap();
    ticks(&c, 100);
    s.set_pos(&sp(PAN, 950, 0, 64)).unwrap(); // live retarget above cur (900)
    ticks(&c, 2000);
    assert_eq!(state(&mut s, PAN), 0);
    assert_eq!(s.min_cur_seen(PAN), -400);
    assert_eq!(pos(&mut s, PAN), 0);
}

#[test]
fn sim_live_set_pos_upward_reversal_halts_short() {
    let (mut s, c) = sim();
    init(&mut s, PAN, 1000);
    s.set_pos(&sp(PAN, 1500, 0, 64)).unwrap();
    ticks(&c, 100);
    s.set_pos(&sp(PAN, 500, 1, 64)).unwrap(); // target below cur, dir latched 0
    ticks(&c, 10);
    assert_eq!(state(&mut s, PAN), 0);
    assert_eq!(pos(&mut s, PAN), 1100);
}

#[test]
fn sim_uncalibrated_set_pos() {
    let (mut s, c) = sim();
    // {pos<=0, dir=0} drives DOWN, not up
    s.set_pos(&sp(PAN, 0, 0, 64)).unwrap();
    assert_eq!(state(&mut s, PAN), 1);
    ticks(&c, 1000);
    assert_eq!(state(&mut s, PAN), 0);
    assert_eq!(pos(&mut s, PAN), 0);
    // pos=rangemax dir0 drives full travel up
    s.set_pos(&sp(PAN, 2700, 0, 64)).unwrap();
    ticks(&c, 3000);
    assert_eq!(pos(&mut s, PAN), 2700);
    // unknown dir: Ok, no motion
    s.set_pos(&sp(TILT, 500, 7, 64)).unwrap();
    assert_eq!(state(&mut s, TILT), 0);
    // still uncalibrated
    let mut v = MotorVal { motor: PAN, val: 9 };
    s.get_calibration_state(&mut v).unwrap();
    assert_eq!(v.val, 0);
}

#[test]
fn sim_uncalibrated_set_pos_while_moving_rewrites_cur() {
    let (mut s, c) = sim();
    s.set_pos(&sp(PAN, 2700, 0, 64)).unwrap();
    ticks(&c, 500);
    assert!(s.raw_cur(PAN) > 400);
    s.set_pos(&sp(PAN, 2700, 0, 64)).unwrap(); // by_dir resets cur=0
    assert_eq!(s.raw_cur(PAN), 0);
}

#[test]
fn sim_speed_latches_only_at_idle() {
    let (mut s, c) = sim();
    init(&mut s, PAN, 100);
    s.set_pos(&sp(PAN, 2000, 0, 8)).unwrap(); // class 6: 1 step / 8 ticks
    ticks(&c, 800);
    assert_eq!(pos(&mut s, PAN), 200);
    s.set_pos(&sp(PAN, 2000, 0, 64)).unwrap(); // live: speed ignored
    ticks(&c, 800);
    assert_eq!(pos(&mut s, PAN), 300);
}

#[test]
fn sim_speed_classes() {
    for (speed, div) in [(15u32, 8u64), (16, 4), (31, 4), (32, 2), (47, 2), (48, 1), (64, 1)] {
        let (mut s, c) = sim();
        init(&mut s, PAN, 100);
        s.set_pos(&sp(PAN, 2000, 0, speed)).unwrap();
        ticks(&c, 800);
        assert_eq!(pos(&mut s, PAN) as u64, 100 + 800 / div, "speed {speed}");
    }
}

#[test]
fn sim_autotest_calibrates_before_final_goto() {
    let (mut s, c) = sim();
    let mut calib = MotorVal { motor: PAN, val: 0 };
    s.get_calibration_state(&mut calib).unwrap();
    assert_eq!(calib.val, 0);
    s.autotest(&mut [PAN, 0, 0, 0, 0]).unwrap();
    s.autotest(&mut [TILT, 0, 0, 0, 0]).unwrap();
    // during the seek STOP is refused
    assert_eq!(errno(s.stop(PAN).unwrap_err()), EBUSY);
    assert_eq!(state(&mut s, PAN), 1);
    let cfg = SimConfig::default();
    ticks(&c, cfg.autotest_seek_ticks + 10);
    // calibrated already, but the final goto is still running
    s.get_calibration_state(&mut calib).unwrap();
    assert_eq!(calib.val, 1);
    assert_eq!(state(&mut s, PAN), 1);
    ticks(&c, 2000);
    assert_eq!(state(&mut s, PAN), 0);
    assert_eq!(state(&mut s, TILT), 0);
    assert_eq!(pos(&mut s, PAN), 1353);
    assert_eq!(pos(&mut s, TILT), 100);
}

#[test]
fn sim_zeroed_autotest_buffer_efault_no_move() {
    let (mut s, c) = sim();
    let e = s.autotest(&mut [0; 5]).unwrap_err();
    assert_eq!(errno(e), EFAULT);
    ticks(&c, 5000);
    assert_eq!(state(&mut s, PAN), 0);
    assert_eq!(state(&mut s, TILT), 0);
    let mut v = MotorVal { motor: PAN, val: 0 };
    s.get_calibration_state(&mut v).unwrap();
    assert_eq!(v.val, 0);
    assert!(!s.oopsed());
}

#[test]
fn sim_stop_has_no_deceleration() {
    let (mut s, c) = sim();
    init(&mut s, PAN, 100);
    s.set_pos(&sp(PAN, 2000, 0, 64)).unwrap();
    ticks(&c, 500);
    s.stop(PAN).unwrap();
    assert_eq!(state(&mut s, PAN), 0);
    assert_eq!(pos(&mut s, PAN), 600);
    ticks(&c, 100);
    assert_eq!(pos(&mut s, PAN), 600);
}

#[test]
fn sim_bad_motor_index_flags_oops() {
    let (mut s, _) = sim();
    assert!(s.get_state(5).is_err());
    assert!(s.oopsed());
}

// ------------------------------------------------------------- Guarded
fn guarded() -> (Guarded<KernelSim<ManualClock>>, ManualClock) {
    let (s, c) = sim();
    (Guarded::new(s, GuardedConfig::default()), c)
}

#[test]
fn guarded_refuses_uncalibrated() {
    let (mut g, c) = guarded();
    assert!(g.set_pos(Axis::Pan, 500, 64).is_err());
    assert!(!g.is_moving(Axis::Pan).unwrap());
    ticks(&c, 100);
    assert_eq!(g.raw().raw_cur(PAN), 0);
}

#[test]
fn guarded_refuses_while_moving_and_no_live_retarget() {
    let (mut g, c) = guarded();
    g.init_pos(Axis::Pan, 1000).unwrap();
    g.set_pos(Axis::Pan, 500, 64).unwrap();
    ticks(&c, 100);
    let e = g.set_pos(Axis::Pan, 900, 64).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(EBUSY));
    assert_eq!(g.raw().raw_target(PAN), 500);
    // reversal sequence: stop, idle, then set
    g.stop(Axis::Pan).unwrap();
    assert!(!g.is_moving(Axis::Pan).unwrap());
    g.set_pos(Axis::Pan, 900, 64).unwrap();
    ticks(&c, 2000);
    assert_eq!(g.position(Axis::Pan).unwrap(), 900);
    assert_eq!(g.raw().min_cur_seen(PAN), 0);
}

#[test]
fn guarded_clamps_and_never_overtravels() {
    let (mut g, c) = guarded();
    g.init_pos(Axis::Pan, 300).unwrap();
    g.set_pos(Axis::Pan, 0, 64).unwrap();
    ticks(&c, 2000);
    assert_eq!(g.position(Axis::Pan).unwrap(), 20);
    assert_eq!(g.raw().min_cur_seen(PAN), 0);
    g.set_pos(Axis::Pan, 99_999, 64).unwrap();
    ticks(&c, 4000);
    assert_eq!(g.position(Axis::Pan).unwrap(), 2700 - 20);
    // tilt
    g.init_pos(Axis::Tilt, 200).unwrap();
    g.set_pos(Axis::Tilt, 0, 64).unwrap();
    ticks(&c, 1000);
    assert_eq!(g.position(Axis::Tilt).unwrap(), 20);
    assert_eq!(g.raw().min_cur_seen(TILT), 0);
}

#[test]
fn guarded_margin_zero_still_never_targets_min() {
    let (s, c) = sim();
    let mut g = Guarded::new(s, GuardedConfig { margin: 0 });
    g.init_pos(Axis::Tilt, 200).unwrap();
    g.set_pos(Axis::Tilt, 0, 64).unwrap();
    ticks(&c, 1000);
    assert_eq!(g.position(Axis::Tilt).unwrap(), 1);
    assert_eq!(g.raw().min_cur_seen(TILT), 0);
}

/// Wraps a sim and records what reaches the kernel.
struct Rec {
    inner: KernelSim<ManualClock>,
    set_pos: Vec<SetPosArg>,
    autotest: Vec<[u32; 5]>,
}
impl RawZfi for Rec {
    fn get_info(&mut self, a: &mut InfoArg) -> std::io::Result<()> { self.inner.get_info(a) }
    fn get_state(&mut self, m: u32) -> std::io::Result<i32> { self.inner.get_state(m) }
    fn get_pos(&mut self, a: &mut PosArg) -> std::io::Result<()> { self.inner.get_pos(a) }
    fn set_pos(&mut self, a: &SetPosArg) -> std::io::Result<()> {
        self.set_pos.push(*a);
        self.inner.set_pos(a)
    }
    fn stop(&mut self, m: u32) -> std::io::Result<()> { self.inner.stop(m) }
    fn init_pos(&mut self, a: &InitPosArg) -> std::io::Result<()> { self.inner.init_pos(a) }
    fn autotest(&mut self, b: &mut [u32; 5]) -> std::io::Result<()> {
        self.autotest.push(*b);
        self.inner.autotest(b)
    }
    fn get_calibration_state(&mut self, a: &mut MotorVal) -> std::io::Result<()> { self.inner.get_calibration_state(a) }
    fn set_backlash(&mut self, a: &MotorVal) -> std::io::Result<()> { self.inner.set_backlash(a) }
}

#[test]
fn guarded_dir_computed_and_never_pos0_dir0() {
    let (s, c) = sim();
    let mut g = Guarded::new(Rec { inner: s, set_pos: vec![], autotest: vec![] }, GuardedConfig::default());
    g.init_pos(Axis::Pan, 1000).unwrap();
    for (target, speed) in [(1200, 64), (0, 64), (700, 8), (5000, 64), (1, 64)] {
        g.set_pos(Axis::Pan, target, speed).unwrap();
        ticks(&c, 6000);
        assert!(!g.is_moving(Axis::Pan).unwrap());
    }
    let sent = g.raw().set_pos.clone();
    assert_eq!(sent.len(), 5);
    assert_eq!((sent[0].dir, sent[1].dir, sent[2].dir, sent[3].dir, sent[4].dir), (0, 1, 0, 0, 1));
    for a in &sent {
        assert_eq!(a.motor, 3);
        assert!(!(a.pos == 0 && a.dir == 0));
        assert!(a.pos >= 20 && a.pos <= 2680);
    }
}

#[test]
fn guarded_autotest_sends_index_and_20_bytes() {
    let (s, c) = sim();
    let mut g = Guarded::new(Rec { inner: s, set_pos: vec![], autotest: vec![] }, GuardedConfig::default());
    g.autotest(Axis::Pan).unwrap();
    g.autotest(Axis::Tilt).unwrap();
    assert_eq!(g.raw().autotest, vec![[3, 0, 0, 0, 0], [4, 0, 0, 0, 0]]);
    // second autotest on a moving axis is refused
    assert!(g.autotest(Axis::Pan).is_err());
    ticks(&c, 6000);
    assert!(g.is_calibrated(Axis::Pan).unwrap() && g.is_calibrated(Axis::Tilt).unwrap());
    assert_eq!(g.position(Axis::Pan).unwrap(), 1353);
    assert_eq!(g.position(Axis::Tilt).unwrap(), 100);
}

#[test]
fn guarded_init_pos_out_of_range_and_axes_only_3_4() {
    let (mut g, _) = guarded();
    assert!(g.init_pos(Axis::Tilt, 5000).is_err());
    assert_eq!(Axis::Pan.motor_index(), 3);
    assert_eq!(Axis::Tilt.motor_index(), 4);
    g.info(Axis::Pan).unwrap();
    assert!(!g.raw().oopsed());
}

#[test]
fn realtime_sim_moves() {
    let mut g = Guarded::new(KernelSim::realtime(), GuardedConfig::default());
    g.init_pos(Axis::Tilt, 100).unwrap();
    g.set_pos(Axis::Tilt, 130, 64).unwrap();
    assert!(g.is_moving(Axis::Tilt).unwrap());
    std::thread::sleep(Duration::from_millis(100));
    assert!(!g.is_moving(Axis::Tilt).unwrap());
    assert_eq!(g.position(Axis::Tilt).unwrap(), 130);
}

// ---------------------------------------------- R3: realism, second writer

const BUSY: i32 = 16;

#[test]
fn sim_autotest_clears_calibrated_until_seek_ends() {
    // hw doc §3: AUTOTEST clears calibrated (0x31a8), set again at seek end.
    let (mut s, c) = sim();
    s.init_pos(&InitPosArg { motor: PAN, pos: 500, ..Default::default() }).unwrap();
    assert!(s.calibrated(PAN));
    s.autotest(&mut [PAN, 0, 0, 0, 0]).unwrap();
    assert!(!s.calibrated(PAN));
    ticks(&c, 10);
    assert!(!s.calibrated(PAN));
    ticks(&c, SimConfig::default().autotest_seek_ticks + 5);
    assert!(s.calibrated(PAN));
}

#[test]
fn sim_init_pos_needs_idle_enabled_and_clamps_to_move_range() {
    // hw doc §2 INIT_POS: 0x2bf8 idle (-16), 0x2be8 enabled (-14),
    // 0x2c0c clamp to the move range.
    let (mut s, c) = sim();
    s.set_move_range(PAN, 0, 2000);
    s.init_pos(&InitPosArg { motor: PAN, pos: 2500, ..Default::default() }).unwrap();
    assert_eq!(s.raw_cur(PAN), 2000);
    s.set_pos(&SetPosArg { motor: PAN, pos: 100, dir: 1, speed: 64 }).unwrap();
    let e = s.init_pos(&InitPosArg { motor: PAN, pos: 50, ..Default::default() }).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(BUSY));
    ticks(&c, 4000);
    s.set_enabled(PAN, false);
    let e = s.init_pos(&InitPosArg { motor: PAN, pos: 50, ..Default::default() }).unwrap_err();
    assert_eq!(e.raw_os_error(), Some(14));
}

#[test]
fn sim_second_writer_shares_state_and_is_not_in_ops() {
    let (mut s, c) = sim();
    let dev = s.device();
    dev.init_pos(PAN, 800).unwrap();
    assert_eq!(s.raw_cur(PAN), 800);
    dev.set_pos(PAN, 1000, 0, 64).unwrap();
    ticks(&c, 50);
    assert_eq!(s.get_state(PAN).unwrap(), 1);
    dev.stop(PAN).unwrap();
    assert_eq!(s.get_state(PAN).unwrap(), 0);
    assert!(s.ops().is_empty());
}

/// Fires a device AUTOTEST right before the first GET_POS, as if `device`
/// started homing inside Guarded::set_pos's read window.
struct Race {
    inner: KernelSim<ManualClock>,
    armed: bool,
}
impl RawZfi for Race {
    fn get_info(&mut self, a: &mut InfoArg) -> std::io::Result<()> { self.inner.get_info(a) }
    fn get_state(&mut self, m: u32) -> std::io::Result<i32> { self.inner.get_state(m) }
    fn get_pos(&mut self, a: &mut PosArg) -> std::io::Result<()> {
        if self.armed {
            self.armed = false;
            self.inner.device().autotest(a.motor).unwrap();
        }
        self.inner.get_pos(a)
    }
    fn set_pos(&mut self, a: &SetPosArg) -> std::io::Result<()> { self.inner.set_pos(a) }
    fn stop(&mut self, m: u32) -> std::io::Result<()> { self.inner.stop(m) }
    fn init_pos(&mut self, a: &InitPosArg) -> std::io::Result<()> { self.inner.init_pos(a) }
    fn autotest(&mut self, b: &mut [u32; 5]) -> std::io::Result<()> { self.inner.autotest(b) }
    fn get_calibration_state(&mut self, a: &mut MotorVal) -> std::io::Result<()> { self.inner.get_calibration_state(a) }
    fn set_backlash(&mut self, a: &MotorVal) -> std::io::Result<()> { self.inner.set_backlash(a) }
}

#[test]
fn guarded_rechecks_idle_and_calibrated_right_before_set_pos() {
    // R1: a device AUTOTEST that starts while Guarded reads GET_POS must be
    // seen; otherwise SET_POS reaches the driver uncalibrated and by_dir
    // fakes cur/target mid-seek.
    let (s, _c) = sim();
    let ops = s.clone();
    let mut g = Guarded::new(Race { inner: s, armed: false }, GuardedConfig::default());
    g.init_pos(Axis::Pan, 1000).unwrap();
    g.raw().armed = true;
    assert!(g.set_pos(Axis::Pan, 500, 64).is_err());
    assert!(!ops.ops().iter().any(|o| matches!(o, Op::SetPos { .. })), "{:?}", ops.ops());
}
