//! Safety wrapper over any `RawZfi` (plan §4.1, §4.3). Both the real and
//! the mock daemon run through this code.
use crate::abi::*;
use crate::raw::RawZfi;
use ptzd_core::{Axis, MotorDev, MotorInfo, RawSpeed};
use std::io;

#[derive(Clone, Copy, Debug)]
pub struct GuardedConfig {
    /// Soft margin from both range ends. Plan §4.3 default 20.
    /// UNPROVEN (T3): the real margin needed to avoid end-stop grinding.
    /// Values below 1 are raised to 1 so min is never targeted (hw doc
    /// H1: moving to 0 overtravels by design).
    pub margin: u32,
}

impl Default for GuardedConfig {
    fn default() -> Self {
        GuardedConfig { margin: 20 }
    }
}

pub struct Guarded<R: RawZfi> {
    raw: R,
    cfg: GuardedConfig,
    info: [Option<MotorInfo>; 2],
}

fn idx(a: Axis) -> usize {
    match a {
        Axis::Pan => 0,
        Axis::Tilt => 1,
    }
}

fn refuse(kind: io::ErrorKind, msg: &str) -> io::Error {
    io::Error::new(kind, msg.to_string())
}

impl<R: RawZfi> Guarded<R> {
    pub fn new(raw: R, cfg: GuardedConfig) -> Self {
        Guarded { raw, cfg, info: [None, None] }
    }
    pub fn raw(&mut self) -> &mut R {
        &mut self.raw
    }
    pub fn into_raw(self) -> R {
        self.raw
    }

    fn cached_info(&mut self, a: Axis) -> io::Result<MotorInfo> {
        if let Some(i) = self.info[idx(a)] {
            return Ok(i);
        }
        let i = self.info(a)?;
        Ok(i)
    }

    /// Usable window [min+margin, max-margin]; max = total-1 (hw doc
    /// H10 drives pan to 2700 of total 2701).
    fn window(&mut self, a: Axis) -> io::Result<(u32, u32)> {
        let i = self.cached_info(a)?;
        let m = self.cfg.margin.max(1);
        let max = i.total.saturating_sub(1);
        let lo = i.min.saturating_add(m);
        let hi = max.saturating_sub(m);
        if lo > hi {
            return Err(refuse(io::ErrorKind::InvalidInput, "margin leaves no range"));
        }
        Ok((lo, hi))
    }

    fn pos_raw(&mut self, a: Axis) -> io::Result<u32> {
        let mut p = PosArg { motor: a.motor_index(), ..Default::default() };
        self.raw.get_pos(&mut p)?;
        Ok(p.pos)
    }

    fn need_idle(&mut self, a: Axis) -> io::Result<()> {
        if self.is_moving(a)? {
            return Err(io::Error::from_raw_os_error(EBUSY));
        }
        Ok(())
    }
}

impl<R: RawZfi> MotorDev for Guarded<R> {
    fn info(&mut self, a: Axis) -> io::Result<MotorInfo> {
        let mut arg = InfoArg { motor: a.motor_index(), ..Default::default() };
        self.raw.get_info(&mut arg)?;
        let i = MotorInfo { total: arg.total, min: arg.min, step: arg.step };
        self.info[idx(a)] = Some(i);
        Ok(i)
    }

    fn is_moving(&mut self, a: Axis) -> io::Result<bool> {
        Ok(self.raw.get_state(a.motor_index())? > 0)
    }

    fn position(&mut self, a: Axis) -> io::Result<u32> {
        self.pos_raw(a)
    }

    fn is_calibrated(&mut self, a: Axis) -> io::Result<bool> {
        let mut v = MotorVal { motor: a.motor_index(), val: 0 };
        self.raw.get_calibration_state(&mut v)?;
        Ok(v.val != 0)
    }

    fn set_pos(&mut self, a: Axis, pos: u32, speed: RawSpeed) -> io::Result<()> {
        // No live retarget (review B1); no SET_POS uncalibrated
        // (hw doc H2, H3).
        // Order matters (review 1 R1): all reads first, then the idle
        // and calibrated checks as the last two ioctls before SET_POS, so a
        // `device` AUTOTEST that starts during the reads is seen.
        // RESIDUAL RACE: `device` can still start AUTOTEST between the
        // calibrated check and SET_POS. Then by_dir runs with our dir (0 or
        // 1) and fakes cur/target (hw doc §3). dir=2 would make that
        // inert but is UNPROVEN (T13), so it is not sent.
        let (lo, hi) = self.window(a)?;
        let target = pos.clamp(lo, hi);
        let cur = self.pos_raw(a)?;
        // Same rule the kernel latches at idle: pos <= cur means down.
        let dir = (target <= cur) as u32;
        self.need_idle(a)?;
        if !self.is_calibrated(a)? {
            return Err(refuse(io::ErrorKind::PermissionDenied, "axis not calibrated"));
        }
        // lo >= 1 so target >= 1; kept as a hard invariant anyway.
        if target == 0 && dir == 0 {
            return Err(refuse(io::ErrorKind::InvalidInput, "{pos<=0,dir=0} refused"));
        }
        self.raw.set_pos(&SetPosArg { motor: a.motor_index(), pos: target, dir, speed })
    }

    fn stop(&mut self, a: Axis) -> io::Result<()> {
        self.raw.stop(a.motor_index())
    }

    fn init_pos(&mut self, a: Axis, pos: u32) -> io::Result<()> {
        self.need_idle(a)?;
        let i = self.cached_info(a)?;
        if pos < i.min || pos > i.total.saturating_sub(1) {
            return Err(refuse(io::ErrorKind::InvalidInput, "init pos out of range"));
        }
        self.raw.init_pos(&InitPosArg { motor: a.motor_index(), pos, ..Default::default() })
    }

    fn autotest(&mut self, a: Axis) -> io::Result<()> {
        self.need_idle(a)?;
        // Review B2: word 0 is the motor index, buffer is 20 bytes.
        let mut buf = [0u32; AUTOTEST_BUF_WORDS];
        buf[0] = a.motor_index();
        self.raw.autotest(&mut buf)
    }

    fn set_backlash(&mut self, a: Axis, steps: u32) -> io::Result<()> {
        self.raw.set_backlash(&MotorVal { motor: a.motor_index(), val: steps })
    }
}
