//! Image sensors: their register-level drivers, over [`light_core::hal::I2cBus`] (the sensors'
//! SCCB control bus is I2C-compatible). The pixels themselves arrive over a port's
//! [`light_core::hal::CameraBus`]; a sensor driver only configures what that bus will carry. The
//! reference driver is the OV5640.

#![no_std]

pub mod ov5640;

pub use ov5640::{Ov5640, Pll};

use light_core::hal::I2cError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraError {
        /// The control bus failed.
        Bus(I2cError),
        /// A part answered, but not the one this driver is for: the id it reported.
        WrongChip(u16),
        /// The requested output size is beyond what this driver configures.
        Unsupported,
}

impl From<I2cError> for CameraError {
        fn from(e: I2cError) -> Self {
                CameraError::Bus(e)
        }
}
