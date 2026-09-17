//! The board-support crate for the Waveshare RP2350-Touch-LCD-2.8: everything specific to this
//! board but independent of which app runs on it. [`board`] is the pin wiring and peripheral
//! hand-over, and [`Touch28Power`] is the board's power mechanism (backlight, battery latch, side
//! key) over the portable [`light_power_manager`] policy.
//!
//! The shell ABI glue and the generic touch/IMU runtime modules are framework-level -- the shell
//! helpers live in [`light_rp2::shell`] and the modules in [`light_input`] -- so this crate holds
//! only what is genuinely board-specific.

#![no_std]

pub mod board;
mod power;

pub use power::Touch28Power;
