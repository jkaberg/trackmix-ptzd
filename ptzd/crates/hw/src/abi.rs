//! ioctl numbers and argument layouts. Source: plan §2.2 table and
//! research/ptzd-hardware.md §2. All fields are u32. Only the allow-listed
//! ioctls exist here (plan §4.1); everything else is forbidden.

pub const GET_INFO: u32 = 0xc010_4c01; // {motor,total,min,step} 16 in/out
pub const GET_STATE: u32 = 0xc004_4c02; // {motor} 4, result = ioctl return
pub const GET_POS: u32 = 0xc010_4c03; // 16, pos in word 1
pub const SET_POS: u32 = 0xc010_4c04; // {motor,pos,dir,speed} 16
pub const STOP: u32 = 0xc004_4c06; // {motor} 4
pub const INIT_POS: u32 = 0xc010_4c08; // {motor,pos} 16
pub const MOTOR_AUTOTEST: u32 = 0xc004_4c0a; // needs >= 20 byte buffer (review B2)
pub const GET_CALIBRATION_STATE: u32 = 0x4008_4c15; // {motor,val} 8
pub const SET_BACKLASH: u32 = 0x4008_4c10; // {motor,val} 8

/// Bytes the kernel reads for AUTOTEST (hw doc H6).
pub const AUTOTEST_BUF_WORDS: usize = 5;

/// Kernel errno values (Linux, same on arm and x86).
pub const EFAULT: i32 = 14;
pub const EBUSY: i32 = 16;
pub const EINVAL: i32 = 22;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InfoArg {
    pub motor: u32,
    pub total: u32,
    pub min: u32,
    pub step: u32,
}

/// GET_POS: pos in word 1; words 2/3 unused by us.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PosArg {
    pub motor: u32,
    pub pos: u32,
    pub w2: u32,
    pub w3: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SetPosArg {
    pub motor: u32,
    pub pos: u32,
    pub dir: u32,
    pub speed: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct InitPosArg {
    pub motor: u32,
    pub pos: u32,
    pub w2: u32,
    pub w3: u32,
}

/// Used for GET_CALIBRATION_STATE and SET_BACKLASH: {motor,val}.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MotorVal {
    pub motor: u32,
    pub val: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateArg {
    pub motor: u32,
}
