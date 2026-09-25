//! SHA-256 in silicon.
//!
//! The RP2350 has the algorithm as a peripheral, so firmware that needs a digest -- checking an
//! asset pack against the one its image was built with, for instance -- can have it for a few
//! hundred bytes of code instead of the several kilobytes a portable implementation costs. This is
//! [`light_core::hal::Sha256`] over that block.
//!
//! WHAT THE HARDWARE DOES AND DOES NOT DO. It compresses: fed 64-byte blocks, it keeps the running
//! state and exposes it as eight sum registers. Everything around that is this driver's -- gathering
//! bytes into whole words, waiting for the block to accept each one, and appending the standard
//! padding (a set bit, zeros, and the message length in bits) at the end. The block byte-swaps
//! written words for us, so words are assembled little-endian from the incoming bytes and the sums
//! are read back big-endian, which is the order a digest is written in.
//!
//! THE BLOCK IS SHARED WITH THE BOOT ROM, which uses it to verify images and partition tables. One
//! hash at a time, and no boot-ROM routine called part-way through one: the engine holds the state
//! of whatever was last fed to it, and a hash interleaved with someone else's is a wrong answer
//! that looks like a right one. Starting an engine resets the block, so an abandoned hash costs
//! the next one nothing.

use light_core::hal::Sha256;

use crate::pac;

/// The algorithm's block: what the hardware compresses at a time.
const BLOCK: usize = 64;
/// The padding that always goes on the end: one set bit, then the message length as 64 bits.
const PADDING_MIN: usize = 1 + 8;

/// The chip's SHA-256 accelerator, for the duration of one hash.
pub struct Sha256Hw {
        /// Bytes gathered but not yet written -- the block takes whole words.
        partial: [u8; 4],
        partial_len: usize,
        /// The message length, which the padding has to state.
        total: u64,
}

impl Default for Sha256Hw {
        fn default() -> Self {
                Self::new()
        }
}

impl Sha256Hw {
        /// Take the block and start a hash.
        ///
        /// Releasing the block's reset first is not ceremony: a block left in reset never reports
        /// itself ready, and the wait below has nothing to time out on.
        pub fn new() -> Self {
                crate::reset_cycle(false, |w| w.sha256().set_bit(), |w| w.sha256().clear_bit(), |r| r.sha256().bit_is_set());
                let sha = Self::hw();
                //   byte swapping on, so a word assembled from bytes in the order they arrive is
                // the big-endian word the algorithm is defined over; the stale not-ready error
                // cleared, so the one raised below means this hash; and then start, which loads
                // the algorithm's initial state
                sha.csr().modify(|_, w| w.bswap().set_bit().err_wdata_not_rdy().clear_bit_by_one());
                sha.csr().modify(|_, w| w.start().set_bit());
                Self { partial: [0; 4], partial_len: 0, total: 0 }
        }

        fn hw() -> &'static pac::sha256::RegisterBlock {
                // SAFETY: the caller holds this engine, and holding it is what claims the block
                unsafe { &*pac::SHA256::ptr() }
        }

        /// Hand one word to the block, once it can take it.
        fn put_word(word: u32) {
                let sha = Self::hw();
                while sha.csr().read().wdata_rdy().bit_is_clear() {
                        core::hint::spin_loop();
                }
                sha.wdata().write(|w| unsafe { w.bits(word) });
        }

        /// Bytes in, whole words out: the block has no notion of a partial word, so one is held
        /// here until the fourth byte of it arrives.
        ///
        ///   THE MIDDLE IS TAKEN FOUR BYTES AT A TIME, and the ragged ends one at a time, because
        /// what is handed to this is almost always a large run and the block wants words. Done
        /// byte by byte throughout -- which is what this was -- the gathering costs more than
        /// everything else here put together: the silicon compresses a sixty-four byte block in
        /// under sixty cycles, and assembling those sixty-four bytes was taking several times
        /// that. Measured over half a megabyte, the loop in front of the block was most of the
        /// time the hash took.
        fn feed(&mut self, bytes: &[u8]) {
                self.total += bytes.len() as u64;
                let mut rest = bytes;

                //   whatever was held from last time, brought up to a whole word
                while self.partial_len != 0 {
                        let Some((&b, tail)) = rest.split_first() else {
                                return;
                        };
                        rest = tail;
                        self.partial[self.partial_len] = b;
                        self.partial_len += 1;
                        if self.partial_len == 4 {
                                Self::put_word(u32::from_le_bytes(self.partial));
                                self.partial_len = 0;
                        }
                }

                let mut whole = rest.chunks_exact(4);
                for w in &mut whole {
                        Self::put_word(u32::from_le_bytes([w[0], w[1], w[2], w[3]]));
                }

                //   fewer than four bytes: held for whatever comes next, or for the padding
                for &b in whole.remainder() {
                        self.partial[self.partial_len] = b;
                        self.partial_len += 1;
                }
        }
}

impl Sha256 for Sha256Hw {
        fn update(&mut self, bytes: &[u8]) {
                self.feed(bytes);
        }

        fn finish(mut self) -> [u8; 32] {
                //   the padding: a set bit, then zeros, then the length in bits, ending on a block
                // boundary. The message length is taken before any of it is fed, because feeding
                // moves it
                let message = self.total;
                let padded = (message as usize + PADDING_MIN).next_multiple_of(BLOCK);
                let zeros = padded - message as usize - PADDING_MIN;
                self.feed(&[0x80]);
                //   in one go rather than a byte at a time: there are fewer than sixty-four of
                // them, so they fit a block's worth of nothing
                const NOTHING: [u8; BLOCK] = [0; BLOCK];
                self.feed(&NOTHING[..zeros]);
                self.feed(&(message * 8).to_be_bytes());
                debug_assert_eq!(self.partial_len, 0, "padding ends on a word boundary");

                let sha = Self::hw();
                while sha.csr().read().sum_vld().bit_is_clear() {
                        core::hint::spin_loop();
                }
                //   the sums are the algorithm's state words; a digest is those words big-endian
                let words = [
                        sha.sum0().read().bits(),
                        sha.sum1().read().bits(),
                        sha.sum2().read().bits(),
                        sha.sum3().read().bits(),
                        sha.sum4().read().bits(),
                        sha.sum5().read().bits(),
                        sha.sum6().read().bits(),
                        sha.sum7().read().bits(),
                ];
                let mut out = [0u8; 32];
                for (chunk, word) in out.chunks_exact_mut(4).zip(words) {
                        chunk.copy_from_slice(&word.to_be_bytes());
                }
                out
        }
}
