//! The board-support crate for the Waveshare RP2350-Touch-LCD-3.5: everything specific to this
//! board but independent of which app runs on it. [`board`] is the pin wiring and peripheral
//! hand-over, and [`Touch35Power`] is the board's power mechanism (backlight, and the AXP2101 for
//! battery, external power and power-off) over the portable [`light_power_manager`] policy.
//!
//! The shell ABI glue and the generic touch/IMU runtime modules are framework-level -- the shell
//! helpers live in [`light_rp2::shell`] and the modules in [`light_input`] -- so this crate holds
//! only what is genuinely board-specific.

#![no_std]

pub mod board;
mod power;

pub use power::{Pmic, Touch35Power};
