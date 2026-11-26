//! Real `/dev/lens_zfi`. The only module allowed unsafe code.
use crate::abi::*;
use crate::raw::RawZfi;
use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::Path;

mod ioc {
    use super::*;
    // Raw numbers: the declared direction bits are unreliable (plan §2.2).
    nix::ioctl_readwrite_bad!(get_info, GET_INFO, InfoArg);
    nix::ioctl_readwrite_bad!(get_state, GET_STATE, StateArg);
    nix::ioctl_readwrite_bad!(get_pos, GET_POS, PosArg);
    nix::ioctl_readwrite_bad!(set_pos, SET_POS, SetPosArg);
    nix::ioctl_readwrite_bad!(stop, STOP, StateArg);
    nix::ioctl_readwrite_bad!(init_pos, INIT_POS, InitPosArg);
    nix::ioctl_readwrite_bad!(autotest, MOTOR_AUTOTEST, [u32; AUTOTEST_BUF_WORDS]);
    nix::ioctl_readwrite_bad!(calib, GET_CALIBRATION_STATE, MotorVal);
    nix::ioctl_readwrite_bad!(backlash, SET_BACKLASH, MotorVal);
}

fn e(err: nix::errno::Errno) -> io::Error {
    io::Error::from_raw_os_error(err as i32)
}

pub struct LensZfiFd {
    file: File,
}

impl LensZfiFd {
    pub const DEFAULT_PATH: &'static str = "/dev/lens_zfi";

    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(LensZfiFd { file })
    }
    fn fd(&self) -> i32 {
        self.file.as_raw_fd()
    }
}

impl RawZfi for LensZfiFd {
    fn get_info(&mut self, arg: &mut InfoArg) -> io::Result<()> {
        // SAFETY: arg is a valid repr(C) 16-byte struct for this ioctl.
        unsafe { ioc::get_info(self.fd(), arg) }.map(|_| ()).map_err(e)
    }
    fn get_state(&mut self, motor: u32) -> io::Result<i32> {
        let mut a = StateArg { motor };
        // SAFETY: 4-byte struct; the return value is the state.
        unsafe { ioc::get_state(self.fd(), &mut a) }.map_err(e)
    }
    fn get_pos(&mut self, arg: &mut PosArg) -> io::Result<()> {
        // SAFETY: valid 16-byte struct.
        unsafe { ioc::get_pos(self.fd(), arg) }.map(|_| ()).map_err(e)
    }
    fn set_pos(&mut self, arg: &SetPosArg) -> io::Result<()> {
        let mut a = *arg;
        // SAFETY: valid 16-byte struct.
        unsafe { ioc::set_pos(self.fd(), &mut a) }.map(|_| ()).map_err(e)
    }
    fn stop(&mut self, motor: u32) -> io::Result<()> {
        let mut a = StateArg { motor };
        // SAFETY: 4-byte struct.
        unsafe { ioc::stop(self.fd(), &mut a) }.map(|_| ()).map_err(e)
    }
    fn init_pos(&mut self, arg: &InitPosArg) -> io::Result<()> {
        let mut a = *arg;
        // SAFETY: valid 16-byte struct.
        unsafe { ioc::init_pos(self.fd(), &mut a) }.map(|_| ()).map_err(e)
    }
    fn autotest(&mut self, buf: &mut [u32; AUTOTEST_BUF_WORDS]) -> io::Result<()> {
        // SAFETY: the kernel reads 20 bytes; the buffer is exactly 20.
        unsafe { ioc::autotest(self.fd(), buf) }.map(|_| ()).map_err(e)
    }
    fn get_calibration_state(&mut self, arg: &mut MotorVal) -> io::Result<()> {
        // SAFETY: valid 8-byte struct.
        unsafe { ioc::calib(self.fd(), arg) }.map(|_| ()).map_err(e)
    }
    fn set_backlash(&mut self, arg: &MotorVal) -> io::Result<()> {
        let mut a = *arg;
        // SAFETY: valid 8-byte struct.
        unsafe { ioc::backlash(self.fd(), &mut a) }.map(|_| ()).map_err(e)
    }
}
