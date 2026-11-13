//! An image over a line console: the wired transport.
//!
//! A device that has a console has a way in for a new image that needs no radio, no card and no
//! debugger -- the cable it is already plugged in by. This is that transport's half of the work on
//! the device: what a line of image looks like, and a [`Receiver`] that turns a stream of them into
//! the same [`Update`] session every other transport ends at. The host's half is the framework's
//! update script, which reads the same format.
//!
//! THE FORMAT, one console line per piece:
//!
//! - `<offset> <base64>` -- [`Chunk::parse`]: where in the image the piece goes, in decimal, and up
//!   to [`CHUNK`] bytes of it in standard base64 with its padding. The command word in front is
//!   the application's to choose, like every other console command.
//! - The image as a whole is announced first by its length and a CRC-32 ([`parse_crc`]), and the
//!   receiver checks that CRC over what it took before anything is handed to the hardware.
//!
//! WHY IT IS SHAPED THE WAY IT IS:
//!
//! - **One line at a time, acknowledged.** A console is not flow-controlled: the device reads
//!   whatever arrives into a small buffer and drops what does not fit, and while it is writing
//!   storage it reads nothing at all. So the sender waits for each piece to be taken before sending
//!   the next, and what can be in flight is one line -- which the device's buffers are sized to
//!   hold.
//! - **Every piece says where it goes, and the answer says what is wanted next.** A line can still
//!   be lost -- a dropped byte makes it unparseable, and a reply can be dropped by a busy log -- and
//!   the cure for both is the same: the sender asks where the receiver is and carries on from
//!   there. A piece that was already taken is acknowledged again rather than written twice, so a
//!   resend is always safe.
//! - **Text, not binary.** The console is a line protocol on every transport a board has, a UART
//!   as much as USB, and base64 rides it unchanged. A third more bytes on the wire is the price, and
//!   storage, not the link, is what limits an update anyway.
//! - **A CRC, not a cryptographic digest.** What it guards is the transfer -- a bit that changed on
//!   a UART with no parity -- and it is checked before the hardware is asked to start anything, so a
//!   damaged transfer is reported as one rather than as an image the hardware refused. What makes an
//!   image *trustworthy* is not here: it is the signature the hardware verifies at every boot. And a
//!   chip's hash block is no help in any case, because on the part that has one the boot ROM shares
//!   it, and staging calls the boot ROM between every page.

use crate::{Staged, Update, UpdateError, UpdateTarget};

/// The most one line carries. 96 bytes is 128 characters of base64, which with the command word
/// and the offset is a line of under 150 -- inside what a console's line reader is sized to take.
pub const CHUNK: usize = 96;

/// A piece of an image, as one line delivered it.
#[derive(Clone, Copy)]
pub struct Chunk {
        offset: u32,
        len: u8,
        bytes: [u8; CHUNK],
}

impl Chunk {
        /// Read a piece from its two words: the offset in decimal, the bytes in base64. `None` for
        /// anything that is not exactly that -- a line that lost a byte on the way is refused here
        /// rather than written somewhere wrong.
        pub fn parse(offset: &str, data: &str) -> Option<Self> {
                let offset = offset.parse().ok()?;
                let mut bytes = [0u8; CHUNK];
                let len = base64_decode(data.as_bytes(), &mut bytes)?;
                if len == 0 {
                        return None;
                }
                Some(Self { offset, len: len as u8, bytes })
        }

        pub fn offset(&self) -> u32 {
                self.offset
        }

        pub fn bytes(&self) -> &[u8] {
                &self.bytes[..self.len as usize]
        }
}

//   by hand, because a piece is carried on an event and events are printed by whatever is
// debugging at the time: where it goes and how much, not ninety-six bytes of image
impl core::fmt::Debug for Chunk {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                write!(f, "Chunk {{ offset: {}, len: {} }}", self.offset, self.len)
        }
}

/// A CRC-32 as eight hex digits, the way the sender writes it.
pub fn parse_crc(word: &str) -> Option<u32> {
        if word.len() != 8 {
                return None;
        }
        u32::from_str_radix(word, 16).ok()
}

/// The CRC-32 the rest of the world means by the name (IEEE 802.3, reflected, as zip and PNG use
/// it), carried along as bytes arrive.
#[derive(Clone, Copy)]
pub struct Crc32(u32);

impl Default for Crc32 {
        fn default() -> Self {
                Self::new()
        }
}

impl Crc32 {
        pub const fn new() -> Self {
                Self(0xffff_ffff)
        }

        pub fn update(&mut self, bytes: &[u8]) {
                //   bitwise, with no table: a kilobyte of table in every image for a check that runs
                // once per update is the wrong trade, and a whole image costs milliseconds this way
                for &b in bytes {
                        self.0 ^= u32::from(b);
                        for _ in 0..8 {
                                let carry = self.0 & 1;
                                self.0 >>= 1;
                                if carry != 0 {
                                        self.0 ^= 0xedb8_8320;
                                }
                        }
                }
        }

        pub fn value(&self) -> u32 {
                !self.0
        }
}

/// Why a received image was not handed over.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiveError {
        /// The session itself refused: the slot, the storage, the length.
        Update(UpdateError),
        /// Everything arrived and was stored, and it is not what was sent.
        Checksum { expected: u32, got: u32 },
}

impl From<UpdateError> for ReceiveError {
        fn from(e: UpdateError) -> Self {
                Self::Update(e)
        }
}

/// An image arriving a line at a time, into an [`Update`] session.
pub struct Receiver<T: UpdateTarget> {
        session: Update<T>,
        crc: Crc32,
        expected_crc: u32,
}

impl<T: UpdateTarget> Receiver<T> {
        /// Open a session for an image of `len` bytes whose CRC-32 is `crc`, to run ON APPROVAL
        /// once it is whole -- see [`Update::begin_on_approval`] for why that is the form a
        /// transport should want.
        pub fn begin_on_approval(target: T, len: u32, crc: u32) -> Result<Self, UpdateError> {
                Ok(Self { session: Update::begin_on_approval(target, len)?, crc: Crc32::new(), expected_crc: crc })
        }

        /// Where the next piece is wanted from.
        pub fn next(&self) -> u32 {
                self.session.progress().0
        }

        /// How much has been taken, of how much was promised.
        pub fn progress(&self) -> (u32, u32) {
                self.session.progress()
        }

        /// Whether every byte that was promised has been taken.
        pub fn complete(&self) -> bool {
                let (done, total) = self.session.progress();
                done == total
        }

        /// Take a piece, and answer where the next one is wanted from.
        ///
        /// A piece that is not the one wanted is not an error: one already taken is a resend whose
        /// acknowledgement was lost, and one further on means something in between was. Either way
        /// nothing is written, and the answer is the same as it would have been -- the offset the
        /// sender should carry on from. Only a piece that does not fit is refused, and storage that
        /// fails.
        pub fn take(&mut self, chunk: &Chunk) -> Result<u32, UpdateError> {
                if chunk.offset() != self.next() {
                        return Ok(self.next());
                }
                self.session.write(chunk.bytes())?;
                self.crc.update(chunk.bytes());
                Ok(self.next())
        }

        /// The whole image has arrived: check it is what was sent, then stage it.
        ///
        /// THE CHECK COMES FIRST, while the last part page is still unwritten -- so an image that
        /// does not match is never complete in storage, and the slot it was going into stays
        /// something the hardware will not start.
        pub fn finish(self) -> Result<Staged<T>, ReceiveError> {
                let got = self.crc.value();
                if got != self.expected_crc {
                        return Err(ReceiveError::Checksum { expected: self.expected_crc, got });
                }
                Ok(self.session.finish()?)
        }

        /// Give up, leaving the slot erased or partial.
        pub fn abandon(self) -> T {
                self.session.abandon()
        }
}

/// Standard base64 with its padding, into `out`; the length written, or `None` for anything that
/// is not exactly that or does not fit.
fn base64_decode(text: &[u8], out: &mut [u8]) -> Option<usize> {
        fn value(c: u8) -> Option<u32> {
                Some(match c {
                        b'A'..=b'Z' => c - b'A',
                        b'a'..=b'z' => c - b'a' + 26,
                        b'0'..=b'9' => c - b'0' + 52,
                        b'+' => 62,
                        b'/' => 63,
                        _ => return None,
                } as u32)
        }
        if !text.len().is_multiple_of(4) {
                return None;
        }
        let mut n = 0;
        let quads = text.len() / 4;
        for (i, q) in text.chunks(4).enumerate() {
                //   padding only in the last group, and only at its end
                let pad = match (q[2], q[3]) {
                        (b'=', b'=') => 2,
                        (_, b'=') => 1,
                        _ => 0,
                };
                if pad > 0 && i + 1 != quads {
                        return None;
                }
                let mut word = 0u32;
                for &c in &q[..4 - pad] {
                        word = (word << 6) | value(c)?;
                }
                word <<= 6 * pad as u32;
                let take = 3 - pad;
                if n + take > out.len() {
                        return None;
                }
                let bytes = word.to_be_bytes();
                out[n..n + take].copy_from_slice(&bytes[1..1 + take]);
                n += take;
        }
        Some(n)
}

#[cfg(test)]
mod tests {
        extern crate alloc;

        use super::*;
        use crate::PAGE;
        use alloc::string::String;
        use alloc::vec;
        use alloc::vec::Vec;

        /// The sender's half, so the tests speak the format the host does.
        fn encode(bytes: &[u8]) -> String {
                const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
                let mut s = String::new();
                for c in bytes.chunks(3) {
                        let w = (u32::from(c[0]) << 16) | (u32::from(*c.get(1).unwrap_or(&0)) << 8) | u32::from(*c.get(2).unwrap_or(&0));
                        for i in 0..4 {
                                if i <= c.len() {
                                        s.push(A[(w >> (18 - 6 * i)) as usize & 63] as char);
                                } else {
                                        s.push('=');
                                }
                        }
                }
                s
        }

        fn crc(bytes: &[u8]) -> u32 {
                let mut c = Crc32::new();
                c.update(bytes);
                c.value()
        }

        struct Slot {
                bytes: Vec<u8>,
        }

        impl UpdateTarget for &mut Slot {
                fn capacity(&self) -> u32 {
                        self.bytes.len() as u32
                }
                fn erase(&mut self) -> Result<(), UpdateError> {
                        self.bytes.fill(0xff);
                        Ok(())
                }
                fn set_on_approval(&mut self, _on: bool) -> Result<(), UpdateError> {
                        Ok(())
                }
                fn program(&mut self, offset: u32, page: &mut [u8]) -> Result<(), UpdateError> {
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
                        Ok(())
                }
                fn boot(&mut self) -> UpdateError {
                        UpdateError::Refused
                }
                fn commit(&mut self) -> Result<(), UpdateError> {
                        Ok(())
                }
        }

        fn image(len: usize) -> Vec<u8> {
                (0..len).map(|i| (i * 7 % 253) as u8).collect()
        }

        #[test]
        fn the_crc_is_the_one_everyone_else_computes() {
                //   the standard check value, which is what makes the host's CRC and this one agree
                assert_eq!(crc(b"123456789"), 0xcbf4_3926);
                assert_eq!(crc(b""), 0);
        }

        #[test]
        fn base64_round_trips_every_tail_length() {
                for len in 1..=CHUNK {
                        let data = image(len);
                        let chunk = Chunk::parse("0", &encode(&data)).expect("well-formed");
                        assert_eq!(chunk.bytes(), &data[..], "{len} bytes");
                }
        }

        #[test]
        fn a_damaged_line_is_refused_rather_than_written() {
                let good = encode(&image(30));
                assert!(Chunk::parse("12", &good).is_some());
                //   a byte lost on the way
                assert!(Chunk::parse("12", &good[1..]).is_none());
                //   a character outside the alphabet, padding in the middle, an offset that is not one
                assert!(Chunk::parse("12", &alloc::format!("*{}", &good[1..])).is_none());
                assert!(Chunk::parse("12", "AA==AAAA").is_none());
                assert!(Chunk::parse("x", &good).is_none());
                assert!(Chunk::parse("-1", &good).is_none());
                //   nothing at all, and more than a line may carry
                assert!(Chunk::parse("0", "").is_none());
                assert!(Chunk::parse("0", &encode(&image(CHUNK + 3))).is_none());
        }

        #[test]
        fn the_crc_word_is_eight_hex_digits() {
                assert_eq!(parse_crc("cbf43926"), Some(0xcbf4_3926));
                assert_eq!(parse_crc("CBF43926"), Some(0xcbf4_3926));
                assert_eq!(parse_crc("cbf4392"), None);
                assert_eq!(parse_crc("cbf43926a"), None);
                assert_eq!(parse_crc("xbf43926"), None);
        }

        /// Send `img` the way the script does, losing what `lose` says, and answer what the slot holds.
        fn send(img: &[u8], mut lose: impl FnMut(u32) -> bool) -> (Result<u32, ReceiveError>, Vec<u8>) {
                let mut slot = Slot { bytes: vec![0; PAGE * 8] };
                let mut rx = Receiver::begin_on_approval(&mut slot, img.len() as u32, crc(img)).expect("it fits");
                let mut at = 0u32;
                let mut lines = 0;
                while !rx.complete() {
                        lines += 1;
                        assert!(lines < 1000, "the transfer never finished");
                        let end = core::cmp::min(at as usize + CHUNK, img.len());
                        let chunk = Chunk::parse(&alloc::format!("{at}"), &encode(&img[at as usize..end])).unwrap();
                        if lose(at) {
                                //   the line or its answer went missing: the sender asks where to go on from
                                at = rx.next();
                                continue;
                        }
                        at = rx.take(&chunk).expect("storage works");
                }
                let len = img.len();
                let result = rx.finish().map(|s| s.len());
                (result, slot.bytes[..len].to_vec())
        }

        #[test]
        fn an_image_sent_a_line_at_a_time_lands_whole() {
                let img = image(PAGE * 3 + 41);
                let (result, stored) = send(&img, |_| false);
                assert_eq!(result, Ok(img.len() as u32));
                assert_eq!(stored, img);
        }

        #[test]
        fn lost_lines_are_sent_again_from_where_the_receiver_is() {
                let img = image(PAGE * 2 + 5);
                let mut dropped = vec![];
                let (result, stored) = send(&img, |at| {
                        //   each of these offsets loses its line once
                        let lose = [0, 192, 480].contains(&at) && !dropped.contains(&at);
                        if lose {
                                dropped.push(at);
                        }
                        lose
                });
                assert_eq!(result, Ok(img.len() as u32));
                assert_eq!(stored, img);
        }

        #[test]
        fn a_resent_piece_is_acknowledged_and_not_written_twice() {
                let img = image(CHUNK * 3);
                let mut slot = Slot { bytes: vec![0; PAGE * 4] };
                let mut rx = Receiver::begin_on_approval(&mut slot, img.len() as u32, crc(&img)).unwrap();
                let first = Chunk::parse("0", &encode(&img[..CHUNK])).unwrap();
                assert_eq!(rx.take(&first), Ok(CHUNK as u32));
                //   its answer was lost, so it comes again
                assert_eq!(rx.take(&first), Ok(CHUNK as u32), "the same answer");
                assert_eq!(rx.progress().0, CHUNK as u32, "and nothing more taken");
                //   and one from further on says where the gap is
                let third = Chunk::parse(&alloc::format!("{}", CHUNK * 2), &encode(&img[CHUNK * 2..])).unwrap();
                assert_eq!(rx.take(&third), Ok(CHUNK as u32));
        }

        #[test]
        fn an_image_that_is_not_what_was_sent_is_never_completed_in_storage() {
                let img = image(PAGE + 10);
                let mut slot = Slot { bytes: vec![0; PAGE * 4] };
                let mut rx = Receiver::begin_on_approval(&mut slot, img.len() as u32, crc(&img) ^ 1).unwrap();
                let mut at = 0usize;
                while at < img.len() {
                        let end = core::cmp::min(at + CHUNK, img.len());
                        let chunk = Chunk::parse(&alloc::format!("{at}"), &encode(&img[at..end])).unwrap();
                        at = rx.take(&chunk).unwrap() as usize;
                }
                assert!(matches!(rx.finish(), Err(ReceiveError::Checksum { .. })));
                //   the tail that would have finished it was never written
                assert!(slot.bytes[PAGE..PAGE + 10].iter().all(|&b| b == 0xff));
        }

        #[test]
        fn more_than_was_promised_is_refused() {
                let img = image(100);
                let mut slot = Slot { bytes: vec![0; PAGE * 4] };
                let mut rx = Receiver::begin_on_approval(&mut slot, 50, crc(&img)).unwrap();
                let chunk = Chunk::parse("0", &encode(&img[..CHUNK])).unwrap();
                assert_eq!(rx.take(&chunk), Err(UpdateError::WrongLength));
        }
}
