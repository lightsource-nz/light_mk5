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
/// A block's last word. The word before it is the block's link: where the next block in the
/// image's loop of blocks begins, in bytes from the start of this one.
const BLOCK_END: [u8; 4] = [0x79, 0x35, 0x12, 0xab];
/// The unit storage is written in, and the alignment a write starts at.
const PAGE: u32 = 256;
/// How far into an image the first block may begin. The hardware looks no further than this
/// either, so an image whose block is past it is one this device could not boot.
const BLOCK_WINDOW: usize = 4096;
/// The item that says what kind of image this is. It is the word immediately after a block's
/// magic -- not a convention this code invented, but where the boot ROM itself reads it from.
const ITEM_IMAGE_TYPE: u8 = 0x42;
/// Within that item's upper halfword: run this image once, on approval.
const IMAGE_TYPE_ON_APPROVAL: u32 = 0x8000_0000;

/// Marks an image to run on approval as its pages go by: the image-type item of the block the
/// boot ROM will run, and no other.
///
/// ONLY THAT BLOCK, BECAUSE MARKING ANY OTHER BREAKS THE SIGNATURE. A sealed image carries more
/// than one block: the one the build put near its start, and the signed one sealing adds at its
/// end, linked into a loop. The ROM runs the last block in the loop, and leaves that block's own
/// mark out of the hash it checks -- but the earlier block lies inside the signed range like any
/// other bytes, so a mark there changes what was signed. With secure boot off nothing checks, and
/// an image marked in every block boots; with it on, the hardware refuses it.
///
///   WHICH BLOCK IS LAST IS KNOWN ONLY AT ITS END, from its link: a block that links forward has
/// another after it, and the last one links back to the first (or to itself, when it is the only
/// one). The mark, though, has to be in a page before that page is written -- it is a bit left
/// set, and storage only takes bits the other way -- and a block may begin in one page and end in
/// the next. So every image-type item is marked as it passes, and the mark is withdrawn once its
/// block turns out to link forward: in the page in hand when the whole block is in it, or by
/// clearing the bit in the page already written, which storage does take without an erase.
#[derive(Default)]
struct Marker {
        /// The previous page ended on a block magic, so its item is this page's first word.
        item_carries: bool,
        /// Where the item of the block now open was marked, as an offset into the slot.
        open: Option<u32>,
        /// The previous page's last word: the link, when this page begins with a block's end.
        previous: [u8; 4],
}

impl Marker {
        /// Mark this page, which goes at `offset` in the slot. Returns where an item in an
        /// EARLIER page has to have its mark withdrawn -- at most one, since blocks do not nest.
        fn mark(&mut self, offset: u32, page: &mut [u8]) -> Option<u32> {
                let mut earlier = None;
                for word in 0..page.len() / 4 {
                        let at = word * 4;
                        if core::mem::take(&mut self.item_carries) && page[at] == ITEM_IMAGE_TYPE {
                                set_mark(&mut page[at..at + 4], true);
                                self.open = Some(offset + at as u32);
                        }
                        if page[at..at + 4] == BLOCK_MAGIC {
                                self.open = None;
                                match page.get(at + 4..at + 8) {
                                        Some(next) if next[0] == ITEM_IMAGE_TYPE => {
                                                set_mark(&mut page[at + 4..at + 8], true);
                                                self.open = Some(offset + at as u32 + 4);
                                        }
                                        //   the magic ended the page: the item is the next page's first word
                                        None => self.item_carries = true,
                                        _ => {}
                                }
                        } else if page[at..at + 4] == BLOCK_END {
                                let link = if at >= 4 { [page[at - 4], page[at - 3], page[at - 2], page[at - 1]] } else { self.previous };
                                let item = self.open.take();
                                match item {
                                        Some(item) if i32::from_le_bytes(link) > 0 && item >= offset => {
                                                let i = (item - offset) as usize;
                                                set_mark(&mut page[i..i + 4], false);
                                        }
                                        Some(item) if i32::from_le_bytes(link) > 0 => earlier = Some(item),
                                        _ => {}
                                }
                        }
                }
                if let Some(last) = page.len().checked_sub(4) {
                        self.previous = [page[last], page[last + 1], page[last + 2], page[last + 3]];
                }
                earlier
        }
}

/// Set or clear the on-approval mark in an image-type item, in place.
fn set_mark(item: &mut [u8], on: bool) {
        let word = u32::from_le_bytes([item[0], item[1], item[2], item[3]]);
        let word = if on { word | IMAGE_TYPE_ON_APPROVAL } else { word & !IMAGE_TYPE_ON_APPROVAL };
        item.copy_from_slice(&word.to_le_bytes());
}

/// The application slot that is not running.
pub struct FlashSlot {
        offset: u32,
        size: u32,
        /// Whether to mark the image as it is written, and where the marking has got to.
        on_approval: bool,
        marker: Marker,
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
                Ok(Self { offset, size, on_approval: false, marker: Marker::default() })
        }

        //   Withdraw the mark from an item in a page already written: write that page again, all
        // of it erased-value except the item without its mark. Writing the erased value changes
        // nothing, and the item loses only the bit -- the one direction storage goes without an
        // erase. Then read the item back, as every other write here is.
        fn withdraw(&mut self, item: u32) -> Result<(), UpdateError> {
                let mut word = [0u8; 4];
                self.read(item, &mut word)?;
                set_mark(&mut word, false);
                let page_at = item & !(PAGE - 1);
                let mut buf = [0xffu8; PAGE as usize];
                let i = (item - page_at) as usize;
                buf[i..i + 4].copy_from_slice(&word);
                Self::rom(unsafe { light_shell_flash_program(self.offset + page_at, buf.as_ptr(), PAGE) })?;
                let mut back = [0u8; 4];
                self.read(item, &mut back)?;
                if back != word { Err(UpdateError::Corrupt) } else { Ok(()) }
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
                self.marker = Marker::default();
                Ok(())
        }

        fn program(&mut self, offset: u32, page: &mut [u8]) -> Result<(), UpdateError> {
                if offset + page.len() as u32 > self.size {
                        return Err(UpdateError::TooLarge);
                }
                //   in the caller's buffer, which is the contract: what is left there is what the
                // storage now holds, so the read-back still means something
                let earlier = if self.on_approval { self.marker.mark(offset, page) } else { None };
                Self::rom(unsafe { light_shell_flash_program(self.offset + offset, page.as_ptr(), page.len() as u32) })?;
                match earlier {
                        Some(item) => self.withdraw(item),
                        None => Ok(()),
                }
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
                //   READ IN PIECES. The window is four kilobytes and the application core's whole
                // stack is eight, so taking it in one buffer would put half the stack into a
                // function that only wants to look at the bytes once -- and this runs underneath
                // a fetch, which has frames of its own below it.
                //   The join between two reads is the whole difficulty: a magic lying across it
                // is in neither piece. So the last few bytes of each piece are carried to the
                // front of the next, which is exactly the overlap a four-byte pattern needs.
                const CHUNK: usize = 256;
                const OVERLAP: usize = BLOCK_MAGIC.len() - 1;
                let window = core::cmp::min(len as usize, BLOCK_WINDOW);
                let mut buf = [0u8; CHUNK + OVERLAP];
                let mut carried = 0usize;
                let mut at = 0usize;
                while at < window {
                        let take = core::cmp::min(CHUNK, window - at);
                        self.read(at as u32, &mut buf[carried..carried + take])?;
                        at += take;
                        let have = carried + take;
                        if buf[..have].windows(BLOCK_MAGIC.len()).any(|w| w == BLOCK_MAGIC) {
                                return Ok(());
                        }
                        carried = core::cmp::min(have, OVERLAP);
                        buf.copy_within(have - carried..have, 0);
                }
                Err(UpdateError::NotAnImage)
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

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use std::vec::Vec;

        /// The image-type item a build writes: an executable for this chip, not on approval.
        const ITEM: u32 = 0x1021_0142;

        /// A block at `at` in `image`, `len_words` long, linking `link` bytes on. The words
        /// between the item and the last item are filler the marker has no reason to read.
        fn block(image: &mut [u8], at: usize, len_words: usize, link: i32) {
                let mut words = std::vec![u32::from_le_bytes(BLOCK_MAGIC), ITEM];
                while words.len() < len_words - 3 {
                        words.push(0x0000_0248);
                }
                words.extend([0x0000_03ff, link as u32, u32::from_le_bytes(BLOCK_END)]);
                for (i, w) in words.iter().enumerate() {
                        image[at + i * 4..at + i * 4 + 4].copy_from_slice(&w.to_le_bytes());
                }
        }

        /// Write `image` a page at a time, as the slot does, withdrawing marks in pages already
        /// written as the slot does too. Returns what storage would hold.
        fn write(image: &[u8]) -> Vec<u8> {
                let mut marker = Marker::default();
                let mut storage = Vec::new();
                for (n, chunk) in image.chunks(PAGE as usize).enumerate() {
                        let mut page = chunk.to_vec();
                        let earlier = marker.mark(n as u32 * PAGE, &mut page);
                        storage.extend_from_slice(&page);
                        if let Some(item) = earlier {
                                assert!(item < n as u32 * PAGE, "a withdrawal names a page already written");
                                set_mark(&mut storage[item as usize..item as usize + 4], false);
                        }
                }
                storage
        }

        fn marked(storage: &[u8], item: usize) -> bool {
                u32::from_le_bytes(storage[item..item + 4].try_into().unwrap()) & IMAGE_TYPE_ON_APPROVAL != 0
        }

        #[test]
        fn a_block_alone_is_marked() {
                let mut image = std::vec![0u8; 1024];
                block(&mut image, 0x138, 7, 0);
                assert!(marked(&write(&image), 0x13c));
        }

        #[test]
        fn a_sealed_image_marks_only_its_last_block() {
                //   the layout a sealed build has: the build's block near the start linking forward,
                // the signed block at the end linking back -- and that one across a page boundary
                let mut image = std::vec![0u8; 0x800];
                block(&mut image, 0x138, 7, 0x6c0 - 0x138);
                block(&mut image, 0x6c0, 54, 0x138 - 0x6c0);
                let storage = write(&image);
                assert!(!marked(&storage, 0x13c), "the block inside the signed range");
                assert!(marked(&storage, 0x6c4), "the block the ROM runs");
                let mut expected = image.clone();
                set_mark(&mut expected[0x6c4..0x6c8], true);
                assert_eq!(storage, expected, "nothing but the one bit changes");
        }

        #[test]
        fn a_forward_block_across_a_page_boundary_is_withdrawn_from_the_page_before() {
                let mut image = std::vec![0u8; 0x800];
                block(&mut image, 0xe0, 20, 0x600 - 0xe0);
                block(&mut image, 0x600, 7, 0xe0 - 0x600);
                let storage = write(&image);
                assert!(!marked(&storage, 0xe4));
                assert!(marked(&storage, 0x604));
        }

        #[test]
        fn a_magic_ending_a_page_carries_its_item_into_the_next() {
                let mut image = std::vec![0u8; 0x800];
                block(&mut image, 0xfc, 7, 0x500 - 0xfc);
                block(&mut image, 0x4fc, 7, 0xfc - 0x4fc);
                let storage = write(&image);
                assert!(!marked(&storage, 0x100));
                assert!(marked(&storage, 0x500));
        }

        #[test]
        fn an_end_beginning_a_page_takes_its_link_from_the_page_before() {
                let mut image = std::vec![0u8; 0x800];
                //   seven words from 0xe8 put the end magic at 0x100, the link at 0xfc
                block(&mut image, 0xe8, 7, 0x400 - 0xe8);
                block(&mut image, 0x400, 7, 0xe8 - 0x400);
                let storage = write(&image);
                assert!(!marked(&storage, 0xec));
                assert!(marked(&storage, 0x404));
        }
}
