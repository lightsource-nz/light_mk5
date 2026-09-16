//! The reference hardware drivers that ship with the framework: touch controllers and an IMU, each
//! implementing the driver-agnostic contract its module defines ([`crate::touch::TouchController`],
//! [`crate::imu::ImuDriver`]) and producing the shared [`crate::TouchSample`]. A consumer adds their
//! own hardware by implementing the same contract, in their own crate; nothing here is privileged.

pub mod axs15231b;
pub mod cst328;
pub mod cst816t;
pub mod gt911;
pub mod qmi8658;
