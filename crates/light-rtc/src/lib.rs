//! Real-time clocks behind [`light_core::hal::I2cBus`]. The [`Rtc`] trait is the driver contract,
//! the reference driver is the PCF85063A, and [`RtcMod`] is the runtime module that keeps and
//! reports the clock generically over the driver and an application's event type.

#![no_std]

pub mod module;
pub mod pcf85063a;

pub use module::RtcMod;
pub use pcf85063a::{Datetime, Pcf85063a};

use light_core::hal::I2cError;

/// A real-time clock driver. The reference driver ([`Pcf85063a`]) implements it; a consumer's own
/// RTC implements the same three calls and drops into [`RtcMod`].
pub trait Rtc {
        /// Bring the part up (mode, format, oscillator load); doubles as the presence probe.
        fn init(&mut self) -> Result<(), I2cError>;
        /// The current time, and whether it has been *kept* since last set (`false` = the
        /// oscillator stopped, so the fields are not to be trusted until a `set`).
        fn now(&mut self) -> Result<(Datetime, bool), I2cError>;
        /// Set every field and clear the oscillator-stop flag.
        fn set(&mut self, t: &Datetime) -> Result<(), I2cError>;
}
