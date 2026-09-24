//! Reaching the storage a product's assets were written to.
//!
//! Assets kept out of the firmware image live in a region of flash the map sets aside (see
//! `light-assets` for what is written there, and documents/11 for the map). Getting at that region
//! takes two things this port can supply and portable code cannot: asking the boot ROM where the
//! region is, and making it addressable.
//!
//! WHY IT IS NOT SIMPLY THERE. The chip reads flash through four address-translation windows, and
//! a bootloader that hands over to an image narrows the first of them to the slot that image was
//! loaded from -- deliberately, so a running image cannot reach past its own partition. The three
//! remaining windows are closed. So an application chained by a bootloader sees its own slot and
//! nothing else, and reading the assets means opening one of the closed windows onto them. This
//! does that: one window, onto the region the map named, and nothing else.
//!
//! The same call works on a board flashed straight to the start of storage, with no map and no
//! bootloader -- there is no region to find, and it says so rather than reading whatever happens
//! to be at that address.

#[cfg(feature = "rp2350")]
use crate::pac;

/// Why the assets could not be reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionError {
        /// This device's flash map sets no region aside for assets -- or there is no map, which is
        /// a board flashed without a bootloader.
        NoRegion,
        /// The map names a region the address-translation window cannot cover.
        TooLarge,
}

/// The window this port lends to the asset region: the second of the four, left closed by a
/// bootloader's hand-over, and covering the addresses just past the first.
#[cfg(feature = "rp2350")]
const WINDOW_BASE: usize = 0x1040_0000;
/// A window is described in units of the flash sector, and spans at most this many of them.
#[cfg(feature = "rp2350")]
const WINDOW_MAX_SECTORS: u32 = 0x400;
#[cfg(feature = "rp2350")]
const SECTOR: u32 = 0x1000;

#[cfg(feature = "rp2350")]
unsafe extern "C" {
        /// The shell's boot-ROM surface: the region the flash map sets aside for data, as a byte
        /// offset from the start of storage and a size. False when this device has none.
        fn light_shell_data_region(out_offset: *mut u32, out_size: *mut u32) -> bool;
}

/// Make the product's asset region readable, and hand back what is in it.
///
/// The slice is the whole region, which is larger than the assets written to it -- a reader is
/// expected to take the length from what it finds there, not from this.
///
/// Calling twice re-opens the same window onto the same region and is harmless. The region is
/// read-only in the sense that nothing here writes to it; the window is a view of storage, so a
/// slice into it stays valid for as long as the firmware runs, which is why it is `'static`.
#[cfg(feature = "rp2350")]
pub fn region() -> Result<&'static [u8], RegionError> {
        let (mut offset, mut size) = (0u32, 0u32);
        if !unsafe { light_shell_data_region(&mut offset, &mut size) } {
                return Err(RegionError::NoRegion);
        }
        if size == 0 || size.div_ceil(SECTOR) > WINDOW_MAX_SECTORS {
                return Err(RegionError::TooLarge);
        }

        //   a window is a base and a size, both counted in sectors. The map's regions are whole
        // sectors already, so nothing is being rounded away here
        let qmi = unsafe { &*pac::QMI::ptr() };
        let base = offset / SECTOR;
        let sectors = size / SECTOR;
        qmi.atrans1().write(|w| unsafe { w.bits((sectors << 16) | base) });

        Ok(unsafe { core::slice::from_raw_parts(WINDOW_BASE as *const u8, size as usize) })
}

/// Chips without the facility have no region to find.
#[cfg(not(feature = "rp2350"))]
pub fn region() -> Result<&'static [u8], RegionError> {
        Err(RegionError::NoRegion)
}
