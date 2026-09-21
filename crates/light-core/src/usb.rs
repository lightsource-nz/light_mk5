//! The identity every port's USB console device presents: one vendor and product, so the
//! framework's scripts find a board by them whichever chip it is. The serial string is the port's
//! (its chip family), so two boards on one host stay distinct devices. The ids are the ones the
//! SDK's console presented, so the tooling's detection never changed.

pub const VID: u16 = 0x2E8A;
pub const PID: u16 = 0x0009;
pub const MANUFACTURER: &str = "lightsource";
pub const PRODUCT: &str = "Light Framework console";
