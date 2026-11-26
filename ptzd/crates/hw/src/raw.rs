//! The kernel ioctls, mirrored 1:1. No safety logic lives here.
use crate::abi::*;
use std::io;

pub trait RawZfi: Send {
    fn get_info(&mut self, arg: &mut InfoArg) -> io::Result<()>;
    /// Ok(1) moving, Ok(0) idle; negative kernel return becomes Err.
    fn get_state(&mut self, motor: u32) -> io::Result<i32>;
    fn get_pos(&mut self, arg: &mut PosArg) -> io::Result<()>;
    /// `dir` is part of the argument: 1 = downward, 0 = upward (the
    /// kernel derives it itself at idle as pos<=cur, review B1).
    fn set_pos(&mut self, arg: &SetPosArg) -> io::Result<()>;
    fn stop(&mut self, motor: u32) -> io::Result<()>;
    fn init_pos(&mut self, arg: &InitPosArg) -> io::Result<()>;
    /// Buffer is `[idx,0,0,0,0]` (review B2).
    fn autotest(&mut self, buf: &mut [u32; AUTOTEST_BUF_WORDS]) -> io::Result<()>;
    fn get_calibration_state(&mut self, arg: &mut MotorVal) -> io::Result<()>;
    fn set_backlash(&mut self, arg: &MotorVal) -> io::Result<()>;
}
