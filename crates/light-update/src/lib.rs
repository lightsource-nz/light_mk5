//! Replacing a device's own firmware, from wherever the bytes come from.
//!
//! A device with two application slots can be given a new image while it runs out of the other
//! one, and then be told to start it. This is the half of that which has nothing to do with where
//! the image came from: bytes in, a staged image out, and a request to the hardware to run it.
//! A radio, a cable, a card and a console all end here.
//!
//! WHAT IT IS CAREFUL ABOUT, in the order the care matters:
//!
//! - **The slot is never the one running.** That is the port's to guarantee
//!   ([`UpdateTarget`]), and it is why this is a trait and not an address.
//! - **Every page is read back before the next one is written.** Storage that accepted a write
//!   and did not keep it is a real failure, and one that otherwise surfaces as a device which
//!   reboots into an image the hardware then refuses -- true, safe, and very hard to read.
//!   Caught here it names the offset while the device is still running the firmware that can say
//!   so.
//! - **Nothing is asked to boot until the whole image is there.** A session that is abandoned
//!   part-way leaves a slot that is erased or partial, which is an image the hardware will not
//!   run and the next attempt simply overwrites.
//!
//! WHAT PROTECTS A BAD IMAGE THAT IS NONETHELESS WRITTEN CORRECTLY is not here: it is the
//! hardware, which verifies an image before running it and falls back to the other slot when it
//! does not like what it finds. An image that boots but is *wrong* -- one that passes verification
//! and then fails at its job -- is the other case, and the answer to that is
//! [`Update::begin_on_approval`]: the image runs once, and a reset is all it takes to be rid of it
//! unless the firmware that started buys itself.

#![no_std]

pub use light_core::hal::{UpdateError, UpdateTarget};

/// The unit a slot is written in. Storage is programmed a page at a time, and 256 bytes is the
/// page every part this runs on agrees to; a part with a finer granularity takes a whole one
/// happily, and the last page of an image is padded out with the value erased storage already
/// holds, so padding writes nothing.
pub const PAGE: usize = 256;
/// What erased storage reads as, and therefore what a partial last page is filled with.
const ERASED: u8 = 0xff;

/// An update being staged: bytes arrive, pages land in the slot.
pub struct Update<T: UpdateTarget> {
        target: T,
        /// What the sender said the image is. An image that arrives short is not an image.
        expected: u32,
        written: u32,
        page: [u8; PAGE],
        page_len: usize,
}

impl<T: UpdateTarget> Update<T> {
        /// Take the slot and erase it, ready for an image of `total_len` bytes.
        ///
        /// Erasing here rather than page by page is deliberate: a slot is erased once, and a
        /// sender that gives up half way leaves it visibly unfinished rather than a mixture of
        /// two images.
        pub fn begin(mut target: T, total_len: u32) -> Result<Self, UpdateError> {
                if total_len == 0 {
                        return Err(UpdateError::WrongLength);
                }
                if total_len > target.capacity() {
                        return Err(UpdateError::TooLarge);
                }
                target.erase()?;
                Ok(Self { target, expected: total_len, written: 0, page: [ERASED; PAGE], page_len: 0 })
        }

        /// The same, but the image will run ON APPROVAL: started once, and reverted to by the next
        /// reset unless it buys itself.
        ///
        /// The mark goes in before the first byte is written, because it is part of the image and
        /// storage takes a bit one way only. `Refused` where the part cannot do it, which is an
        /// answer with two honest responses -- stage it permanently, or do not stage it.
        ///
        /// BEFORE REACHING FOR THIS, CHECK THAT THE PART CAN BUY. Marking an image and starting it
        /// are the easy half; clearing the mark afterwards is a rewrite of the storage the running
        /// image sits in, and a part that gets that wrong leaves an image that is damaged rather
        /// than kept -- after which every reset reverts, which is worse than an update that simply
        /// sticks. A port that cannot do the second half should refuse the first, so that a
        /// product stages permanently and relies on the hardware's own verification instead.
        pub fn begin_on_approval(mut target: T, total_len: u32) -> Result<Self, UpdateError> {
                target.set_on_approval(true)?;
                Self::begin(target, total_len)
        }

        /// How much of the image has been taken, and how much was promised -- for a caller that
        /// wants to say so out loud while a slow transport runs.
        pub fn progress(&self) -> (u32, u32) {
                (self.written + self.page_len as u32, self.expected)
        }

        /// Take the next bytes of the image. They arrive in whatever sizes the transport deals
        /// in; pages are assembled here.
        pub fn write(&mut self, mut bytes: &[u8]) -> Result<(), UpdateError> {
                if self.written + self.page_len as u32 + bytes.len() as u32 > self.expected {
                        return Err(UpdateError::WrongLength);
                }
                while !bytes.is_empty() {
                        let take = core::cmp::min(PAGE - self.page_len, bytes.len());
                        self.page[self.page_len..self.page_len + take].copy_from_slice(&bytes[..take]);
                        self.page_len += take;
                        bytes = &bytes[take..];
                        if self.page_len == PAGE {
                                self.flush()?;
                        }
                }
                Ok(())
        }

        /// The whole image has arrived: write what is left and check the length.
        pub fn finish(mut self) -> Result<Staged<T>, UpdateError> {
                if self.page_len > 0 {
                        //   the tail of the last page is left as erased storage reads, so the
                        // write puts nothing there and the image ends where it ends
                        self.page[self.page_len..].fill(ERASED);
                        self.page_len = PAGE;
                        self.flush()?;
                }
                if self.written < self.expected {
                        return Err(UpdateError::WrongLength);
                }
                self.target.accept(self.expected)?;
                Ok(Staged { target: self.target, len: self.expected })
        }

        /// Give up, leaving the slot erased-or-partial. Nothing will be asked to boot it.
        pub fn abandon(self) -> T {
                self.target
        }

        fn flush(&mut self) -> Result<(), UpdateError> {
                let at = self.written;
                self.target.program(at, &mut self.page)?;
                //   read it back NOW, not at the end: an error here names the page that failed,
                // and a slot that cannot be written is not worth filling the rest of
                let mut back = [0u8; PAGE];
                self.target.read(at, &mut back)?;
                if back != self.page {
                        return Err(UpdateError::Corrupt);
                }
                self.written += PAGE as u32;
                self.page_len = 0;
                self.page = [ERASED; PAGE];
                Ok(())
        }
}

/// An image that is in the slot, whole and read back.
pub struct Staged<T: UpdateTarget> {
        target: T,
        len: u32,
}

impl<T: UpdateTarget> Staged<T> {
        /// How long the staged image is.
        pub fn len(&self) -> u32 {
                self.len
        }

        pub fn is_empty(&self) -> bool {
                self.len == 0
        }

        /// Start it. Returns only if the hardware would not.
        ///
        /// The hardware verifies the image before running it and falls back to the other slot if
        /// it does not like what it finds, so an image that is damaged or unsigned cannot take
        /// the device down. An image that boots and is nonetheless WRONG is the other case, and
        /// the answer to that is [`Update::begin_on_approval`]: the new firmware then has to buy
        /// itself, and a reset is all it takes to be rid of it.
        pub fn boot(mut self) -> UpdateError {
                self.target.boot()
        }

        /// Hand the slot back without starting anything.
        pub fn release(self) -> T {
                self.target
        }
}

#[cfg(test)]
mod tests {
        extern crate alloc;

        use super::*;
        use alloc::vec;
        use alloc::vec::Vec;

        /// A slot in memory, with the faults a real one can have.
        struct Slot {
                bytes: Vec<u8>,
                /// An offset whose write is silently dropped -- storage that said yes and did not.
                deaf_at: Option<u32>,
                /// Whether this slot marks what it stores, the way a real one may.
                marks: bool,
                erased: bool,
                booted: bool,
                accepts: bool,
        }

        impl Slot {
                fn new(capacity: usize) -> Self {
                        Self {
                                bytes: vec![ERASED; capacity],
                                deaf_at: None,
                                marks: false,
                                erased: false,
                                booted: false,
                                accepts: true,
                        }
                }
        }

        impl UpdateTarget for &mut Slot {
                fn capacity(&self) -> u32 {
                        self.bytes.len() as u32
                }

                fn erase(&mut self) -> Result<(), UpdateError> {
                        self.bytes.fill(ERASED);
                        self.erased = true;
                        Ok(())
                }

                fn program(&mut self, offset: u32, page: &mut [u8]) -> Result<(), UpdateError> {
                        if Some(offset) == self.deaf_at {
                                return Ok(());
                        }
                        //   a port that alters what it stores, which the contract allows so long
                        // as the buffer it leaves is what the storage holds
                        if self.marks {
                                page[0] |= 0x80;
                        }
                        let at = offset as usize;
                        self.bytes[at..at + page.len()].copy_from_slice(page);
                        Ok(())
                }

                fn read(&mut self, offset: u32, out: &mut [u8]) -> Result<(), UpdateError> {
                        let at = offset as usize;
                        out.copy_from_slice(&self.bytes[at..at + out.len()]);
                        Ok(())
                }

                fn accept(&mut self, _len: u32) -> Result<(), UpdateError> {
                        if self.accepts { Ok(()) } else { Err(UpdateError::NotAnImage) }
                }

                fn boot(&mut self) -> UpdateError {
                        self.booted = true;
                        UpdateError::Refused
                }

                fn commit(&mut self) -> Result<(), UpdateError> {
                        Ok(())
                }
        }

        fn image(len: usize) -> Vec<u8> {
                (0..len).map(|i| (i % 251) as u8).collect()
        }

        #[test]
        fn an_image_fed_in_arbitrary_pieces_lands_whole() {
                let img = image(PAGE * 3 + 17);
                let mut slot = Slot::new(PAGE * 8);
                let mut up = Update::begin(&mut slot, img.len() as u32).expect("it fits");
                //   sizes a transport really deals in: one byte, a part page, more than a page
                for chunk in [1, 5, 200, 300, 1, PAGE * 2] {
                        let done = up.progress().0 as usize;
                        let take = core::cmp::min(chunk, img.len() - done);
                        if take == 0 { break; }
                        up.write(&img[done..done + take]).expect("takes it");
                }
                let staged = up.finish().expect("the whole image arrived");
                assert_eq!(staged.len(), img.len() as u32);
                assert_eq!(&slot.bytes[..img.len()], &img[..]);
                //   and the rest of the slot is still erased, so the image ends where it ends
                assert!(slot.bytes[img.len()..].iter().all(|&b| b == ERASED));
        }

        #[test]
        fn the_slot_is_erased_once_at_the_start() {
                let mut slot = Slot::new(PAGE * 4);
                slot.bytes.fill(0x5a);
                let up = Update::begin(&mut slot, 10).expect("it fits");
                drop(up.abandon());
                assert!(slot.erased);
                assert!(slot.bytes.iter().all(|&b| b == ERASED), "an abandoned session leaves nothing to boot");
        }

        #[test]
        fn storage_that_does_not_keep_a_page_is_caught_at_that_page() {
                let img = image(PAGE * 4);
                let mut slot = Slot::new(PAGE * 8);
                slot.deaf_at = Some(PAGE as u32 * 2);
                let mut up = Update::begin(&mut slot, img.len() as u32).expect("it fits");
                let err = up.write(&img).expect_err("the third page does not read back");
                assert_eq!(err, UpdateError::Corrupt);
        }

        #[test]
        fn an_image_that_does_not_fit_is_refused_before_anything_is_erased() {
                let mut slot = Slot::new(PAGE * 2);
                assert_eq!(Update::begin(&mut slot, PAGE as u32 * 3).err(), Some(UpdateError::TooLarge));
                assert!(!slot.erased, "nothing was touched");
                assert_eq!(Update::begin(&mut slot, 0).err(), Some(UpdateError::WrongLength));
        }

        #[test]
        fn more_or_fewer_bytes_than_promised_is_not_an_image() {
                let mut slot = Slot::new(PAGE * 8);
                let mut up = Update::begin(&mut slot, 300).expect("it fits");
                assert_eq!(up.write(&[0u8; 301]), Err(UpdateError::WrongLength), "more than promised");
                up.write(&[0u8; 100]).expect("some of it");
                assert_eq!(up.finish().err(), Some(UpdateError::WrongLength), "and it stopped short");
        }

        #[test]
        fn what_is_staged_is_offered_to_the_port_before_anything_is_booted() {
                let img = image(PAGE);
                let mut slot = Slot::new(PAGE * 4);
                slot.accepts = false;
                let mut up = Update::begin(&mut slot, img.len() as u32).expect("it fits");
                up.write(&img).expect("takes it");
                assert_eq!(up.finish().err(), Some(UpdateError::NotAnImage));
                assert!(!slot.booted, "and nothing was asked to run it");
        }

        #[test]
        fn a_port_that_alters_what_it_stores_still_verifies() {
                //   a real one marks an image as it goes past, so the storage does not hold quite
                // what arrived. The contract is that it alters the caller's buffer, and this is
                // what would fail if it altered a copy instead
                let img = image(PAGE * 2);
                let mut slot = Slot::new(PAGE * 4);
                slot.marks = true;
                let mut up = Update::begin(&mut slot, img.len() as u32).expect("it fits");
                up.write(&img).expect("takes it, marks and all");
                up.finish().expect("and the read-back agrees with what was written");
                assert_eq!(slot.bytes[0], img[0] | 0x80, "the stored image carries the mark");
        }

        #[test]
        fn a_port_that_cannot_run_on_approval_says_so_before_erasing_anything() {
                let mut slot = Slot::new(PAGE * 4);
                //   the mock takes the default, which is a refusal
                assert_eq!(Update::begin_on_approval(&mut slot, PAGE as u32).err(), Some(UpdateError::Refused));
                assert!(!slot.erased, "and the slot it would have gone in is untouched");
        }

        #[test]
        fn a_staged_image_is_what_boots_and_what_commits() {
                let img = image(PAGE * 2);
                let mut slot = Slot::new(PAGE * 4);
                {
                        let mut up = Update::begin(&mut slot, img.len() as u32).expect("it fits");
                        up.write(&img).expect("takes it");
                        let mut staged = up.finish().expect("whole");
                        assert_eq!(staged.boot(), UpdateError::Refused, "boot answers only on failure");
                }
                assert!(slot.booted);
        }
}
