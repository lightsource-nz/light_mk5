/// HostController implementation for Raspberry Pi Pico / RP2040
#[cfg(feature = "rp2040")]
pub mod rp2040;

/// HostController implementation for Raspberry Pi Pico 2 / RP2350 (light_mk5: the RP2040 module
/// retargeted at its pac; the controller is the same block, plus a PHY isolation bit to clear)
#[cfg(feature = "rp2350")]
pub mod rp2350;
