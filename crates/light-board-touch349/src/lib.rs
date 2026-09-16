//! The board-support crate for the Waveshare RP2350-Touch-LCD-3.49: everything specific to this
//! board but independent of which app runs on it. [`board`] is the pin wiring and peripheral
//! hand-over, and [`PowerManager`] is the board's power mechanism over the portable
//! [`light_power_manager`] policy (dim after an idle; power off on battery after a longer one).
//!
//! The shell ABI glue and the generic touch/IMU runtime modules are framework-level: the shell
//! helpers live in [`light_rp2::shell`] and the modules in [`light_input`], so this crate holds only
//! what is genuinely board-specific -- the pins, the coordinate and axis maps, the peripheral
//! hand-over, and the power mechanism.

#![no_std]

pub mod board;
mod power;

pub use power::PowerManager;
