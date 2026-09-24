//! The slot this chip's next firmware goes into.
//!
//! [`light_core::hal::UpdateTarget`] over the boot ROM: find the application slot that is not the
//! one running, write an image into it, and ask the hardware to start it. The transport-free half
//! of an update lives in `light-update`; this is the part that knows what a slot is on this part.
//!
//! WHY THE ROM AND NOT THE FLASH DIRECTLY. Its routines check the write against the flash map's
//! permissions, so a write that strays outside the slot is refused rather than performed; they
//! address storage rather than the running image's window, which matters because the slot being
//! written is by definition the one that window does not cover; and they park the other core for
//! the duration, which is not optional -- storage cannot be read while it is being written, and
//! the other core is running its code out of it.

use light_core::hal::{UpdateError, UpdateTarget};

unsafe extern "C" {
        fn light_shell_update_slot(out_offset: *mut u32, out_size: *mut u32) -> bool;
        fn light_shell_flash_erase(offset: u32, len: u32) -> i32;
        fn light_shell_flash_program(offset: u32, buf: *const u8, len: u32) -> i32;
        fn light_shell_flash_read(offset: u32, buf: *mut u8, len: u32) -> i32;
        fn light_shell_reboot_update(offset: u32) -> i32;
        fn light_shell_commit(scratch: *mut u8, scratch_len: u32) -> i32;
}

/// A block's first word, which is how an image says it is one.
const BLOCK_MAGIC: [u8; 4] = [0xd3, 0xde, 0xff, 0xff];
/// How far into an image the first block may begin. The hardware looks no further than this
/// either, so an image whose block is past it is one this device could not boot.
const BLOCK_WINDOW: usize = 4096;
/// The item that says what kind of image this is. It is the word immediately after a block's
/// magic -- not a convention this code invented, but where the boot ROM itself reads it from.
const ITEM_IMAGE_TYPE: u8 = 0x42;
/// Within that item's upper halfword: run this image once, on approval.
const IMAGE_TYPE_ON_APPROVAL: u32 = 0x8000_0000;

/// The application slot that is not running.
pub struct FlashSlot {
        offset: u32,
        size: u32,
        /// Whether to mark the image as it is written, and whether the previous page ended on a
        /// block magic -- in which case the item to mark is the first word of the next one.
        on_approval: bool,
        item_carries: bool,
}

impl FlashSlot {
        /// Find it: the other half of the A/B pair this image booted from.
        ///
        /// `NoSlot` on a device with no such pair, which is a device that cannot replace its own
        /// firmware while running it.
        pub fn inactive() -> Result<Self, UpdateError> {
                let (mut offset, mut size) = (0u32, 0u32);
                if !unsafe { light_shell_update_slot(&mut offset, &mut size) } {
                        return Err(UpdateError::NoSlot);
                }
                Ok(Self { offset, size, on_approval: false, item_carries: false })
        }

        //   Mark every image-type item in this page, if the image is to run on approval.
        //
        //   HERE, AND NOT AFTERWARDS, because the mark is a bit going from clear to set and
        // storage only takes bits the other way: putting it in after the page is written would
        // mean erasing what was just written. The page is still in memory, so it costs a scan.
        //
        //   Blocks are word-aligned, which is what makes the boundary case one line: a magic can
        // be the last word of a page, and then the item to mark is the first word of the next.
        fn mark(&mut self, page: &mut [u8]) {
                for word in 0..page.len() / 4 {
                        let at = word * 4;
                        if core::mem::take(&mut self.item_carries) && page[at] == ITEM_IMAGE_TYPE {
                                let item = u32::from_le_bytes([page[at], page[at + 1], page[at + 2], page[at + 3]]);
                                page[at..at + 4].copy_from_slice(&(item | IMAGE_TYPE_ON_APPROVAL).to_le_bytes());
                        }
                        if page[at..at + 4] != BLOCK_MAGIC {
                                continue;
                        }
                        match page.get(at + 4..at + 8) {
                                Some(next) if next[0] == ITEM_IMAGE_TYPE => {
                                        let item = u32::from_le_bytes([next[0], next[1], next[2], next[3]]);
                                        page[at + 4..at + 8].copy_from_slice(&(item | IMAGE_TYPE_ON_APPROVAL).to_le_bytes());
                                }
                                //   the magic ended the page: the item is the next page's first word
                                None => self.item_carries = true,
                                _ => {}
                        }
                }
        }

        fn rom(rc: i32) -> Result<(), UpdateError> {
                if rc < 0 { Err(UpdateError::Storage) } else { Ok(()) }
        }
}

impl UpdateTarget for FlashSlot {
        fn capacity(&self) -> u32 {
                self.size
        }

        fn erase(&mut self) -> Result<(), UpdateError> {
                Self::rom(unsafe { light_shell_flash_erase(self.offset, self.size) })
        }

        fn set_on_approval(&mut self, on: bool) -> Result<(), UpdateError> {
                self.on_approval = on;
                self.item_carries = false;
                Ok(())
        }

        fn program(&mut self, offset: u32, page: &mut [u8]) -> Result<(), UpdateError> {
                if offset + page.len() as u32 > self.size {
                        return Err(UpdateError::TooLarge);
                }
                if self.on_approval {
                        //   in the caller's buffer, which is the contract: what is left there is
                        // what the storage now holds, so the read-back still means something
                        self.mark(page);
                }
                Self::rom(unsafe { light_shell_flash_program(self.offset + offset, page.as_ptr(), page.len() as u32) })
        }

        fn read(&mut self, offset: u32, out: &mut [u8]) -> Result<(), UpdateError> {
                if offset + out.len() as u32 > self.size {
                        return Err(UpdateError::TooLarge);
                }
                Self::rom(unsafe { light_shell_flash_read(self.offset + offset, out.as_mut_ptr(), out.len() as u32) })
        }

        fn accept(&mut self, len: u32) -> Result<(), UpdateError> {
                //   does it begin like an image? The hardware asks the same question again before
                // it runs anything, but it asks after a reboot, from a device that can no longer
                // say what it found. Asking here costs one read and answers while this firmware is
                // still the one running
                let window = core::cmp::min(len as usize, BLOCK_WINDOW);
                let mut head = [0u8; BLOCK_WINDOW];
                self.read(0, &mut head[..window])?;
                let found = head[..window].windows(4).any(|w| w == BLOCK_MAGIC);
                if found { Ok(()) } else { Err(UpdateError::NotAnImage) }
        }

        fn boot(&mut self) -> UpdateError {
                //   naming the window is the point: it tells the bootloader to prefer the slot
                // just written over the comparison it would otherwise make between the two
                unsafe { light_shell_reboot_update(self.offset) };
                UpdateError::Refused
        }

        fn commit(&mut self) -> Result<(), UpdateError> {
                commit(&mut [])
        }
}

/// The scratch space clearing an on-approval mark needs, because doing it means rewriting the
/// sector the mark sits in.
///
/// TWICE THE SECTOR, AND BOTH HALVES ARE THERE FOR A MEASURED REASON. The published requirement is
/// a word-aligned buffer of at least one sector, and both ways of reading that loosely cost a day.
///
/// *Word-aligned* is not advice: hand the routine an odd address and it does not come back as an
/// error -- it takes flash out of service, never returns, and leaves a board stopped with its
/// other core still parked and its console silent. Hence WORDS here, and a `u32` element type, so
/// a caller cannot allocate this as bytes and land where it landed once.
///
/// *At least one sector* is the copy of the sector alone. The same call also has its own
/// bookkeeping to keep, and given exactly a sector it keeps it inside the copy: the routine then
/// answers success and programs the spoiled copy back over the image. Measured at exactly one
/// sector, what came back was two thirds zeros, a third erased and a little real data, with the
/// image's own header among the zeros -- an image destroyed by the call that was meant to keep it,
/// reported as a success. At two sectors the same call clears the mark and leaves everything
/// around it intact.
pub const COMMIT_SCRATCH_WORDS: usize = 2048;

/// Keep the image that is RUNNING, where it was started on approval.
///
/// Nothing to do with any slot, which is why it is here and not on one: a device has one such
/// facility and it acts on what is executing. The new firmware calls this once it is satisfied
/// with itself; until it does, the next reset is all it takes to be rid of it.
///
/// `scratch` wants [`COMMIT_SCRATCH_WORDS`] words, and that constant's notes are worth reading
/// before shortening it: this is a routine that answers success after destroying the image, if it
/// is given less room than it needs. A shorter buffer is safe only where there is no mark to clear
/// -- which is every board that never started an image on approval, and none that did.
pub fn commit(scratch: &mut [u32]) -> Result<(), UpdateError> {
        let rc = unsafe { light_shell_commit(scratch.as_mut_ptr().cast(), (scratch.len() * 4) as u32) };
        //   the code itself, because "it said yes" and "it did the thing" are different claims and
        // only one of them is observable from here
        light_core::debug!("commit: the boot facility answered {rc}");
        FlashSlot::rom(rc)
}
