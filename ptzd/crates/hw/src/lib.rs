//! Hardware layer: raw ioctl trait, the real fd impl, a kernel simulator
//! and the `Guarded` safety wrapper (plan §4.1, §4.3).
//! Only `fd` may use unsafe code.
#![deny(unsafe_code)]

pub mod abi;
pub mod clock;
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub mod fd;
pub mod guarded;
pub mod raw;
pub mod sim;

pub use clock::{Clock, ManualClock, RealClock};
#[cfg(target_os = "linux")]
pub use fd::LensZfiFd;
pub use guarded::{Guarded, GuardedConfig};
pub use raw::RawZfi;
pub use sim::{DeviceHandle, KernelSim, Op, SimConfig};
