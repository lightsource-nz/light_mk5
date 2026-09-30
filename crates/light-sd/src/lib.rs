//! SD/TF cards behind [`light_core::hal::SpiBus`]: the SPI-mode block layer -- card
//! identification and 512-byte block reads and writes, exposed as a
//! [`light_core::hal::BlockDevice`]. A filesystem is a separate crate (`light-fs`); this
//! one stops at "the card answers and blocks move". [`shared`] lets several users share one card:
//! a per-operation borrowing block device, and the [`light_core::hal::Store`] over it.

#![no_std]

pub mod shared;
pub mod spi_card;
pub use shared::{CardStore, SharedCard};
pub use spi_card::{CardInfo, SdError, SpiSd};
