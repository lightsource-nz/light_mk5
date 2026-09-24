//! LAP: the light asset pack.
//!
//! A font, a theme and a UI design are already data (see `light-font`, light-ui's `theme` and
//! `lui`). This is the container that lets that data live somewhere OTHER THAN the firmware image:
//! a directory of named blobs, written once to a region of storage the image does not cover, and
//! read in place. What the firmware gets back from a lookup is the same `&[u8]` an
//! `include_bytes!` would have given it, so everything above this layer is unchanged -- the
//! difference is that restyling or re-lettering a product no longer means building, signing and
//! shipping a new application.
//!
//! WHAT IT COSTS is a second thing that can be wrong, so the reader is suspicious. The pack is
//! read out of storage that nothing verified on the way in; it may be blank, half-written, left
//! over from another product, or edited by someone who wants the interface to say something it
//! should not. So [`Pack::open`] takes the digest the firmware image carries -- the image being
//! the artefact that IS verified -- and refuses a pack that does not hash to it. A caller that
//! cannot open the pack has no font, and an interface with no font is not an interface: the
//! contract is to say so and stop, not to limp on.
//!
//! Layout (all little-endian):
//!
//! ```text
//!  0  "LAP1"                magic
//!  4  u8   version          1
//!  5  u8   count            how many entries the directory holds
//!  6  u16  reserved         0
//!  8  u32  total_len        the whole pack, header and directory and payload
//! 12  u32  reserved         0
//! 16  [u8; 32] digest       SHA-256 of bytes 0..16 followed by bytes 48..total_len
//! 48  directory             count entries of 24 bytes, ascending by name:
//!                             0  [u8; 16] name, NUL-padded ASCII
//!                            16  u32      offset from the start of the pack
//!                            20  u32      length in bytes
//!     payload               the blobs, each starting on a four-byte boundary
//! ```
//!
//! The digest covers the header's own fields as well as the directory and the payload; it cannot
//! cover the bytes it is written into, which is the reason for the two ranges rather than one.
//! Entries ascend by name so a pack built from the same inputs is byte-for-byte the same pack, and
//! so a duplicate name cannot hide behind an earlier one.

#![no_std]

#[cfg(feature = "alloc")]
extern crate alloc;

use sha2::{Digest, Sha256};

pub const MAGIC: [u8; 4] = *b"LAP1";
/// The schema version in the header (byte 4) -- the shared blob-header convention (`magic` then a
/// `u8` version) the framework's LGF fonts, LTH themes and LUI UIs also carry.
pub const VERSION: u8 = 1;
pub const HEADER_LEN: usize = 48;
/// A SHA-256, the digest the header carries and the firmware image is stamped with.
pub const DIGEST_LEN: usize = 32;
/// One directory entry: a padded name, an offset and a length.
const ENTRY_LEN: usize = 24;
/// The longest entry name. Names are short identifiers -- `font`, `theme`, `ui`.
pub const NAME_LEN: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackError {
        /// Nothing that begins like a pack is there -- most often blank storage, or a region that
        /// was never written.
        BadMagic,
        /// A pack, but of a schema this build does not read.
        UnsupportedVersion(u8),
        /// The header, the directory or an entry's extent runs past the end of what was handed in.
        Truncated,
        /// The directory does not ascend by name, or an entry overlaps the directory.
        BadDirectory,
        /// The pack is intact but is not the pack this firmware was built against.
        DigestMismatch,
        /// No entry of that name.
        NotFound,
        /// Offered to the writer: a name that is empty, too long, not ASCII, or already used.
        BadName,
}

/// A pack opened over a region of memory, and the lookups it answers.
///
/// Zero-copy throughout: a looked-up blob is a view into the region, so a font stays where it is
/// rather than being copied into the memory the application wanted for something else.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pack<'a> {
        blob: &'a [u8],
        count: usize,
}

impl<'a> Pack<'a> {
        /// Open the pack at the start of `region`, checking it against `expected`.
        ///
        /// `region` may be larger than the pack -- it usually is, being a whole partition or
        /// window -- and only the pack's own extent is read.
        pub fn open(region: &'a [u8], expected: &[u8; DIGEST_LEN]) -> Result<Self, PackError> {
                let pack = Self::open_unchecked(region)?;
                if pack.digest() != expected {
                        return Err(PackError::DigestMismatch);
                }
                //   the header says what the digest is; whether the content agrees is the
                // question tampering turns on
                let mut h = Sha256::new();
                h.update(&pack.blob[..16]);
                h.update(&pack.blob[HEADER_LEN..]);
                if h.finalize().as_slice() != expected {
                        return Err(PackError::DigestMismatch);
                }
                Ok(pack)
        }

        /// Open without checking the digest: structure only.
        ///
        /// For a tool that inspects a pack it has no image to compare against. Firmware uses
        /// [`open`](Self::open) -- a pack whose contents were never checked is a pack someone else
        /// may have written.
        pub fn open_unchecked(region: &'a [u8]) -> Result<Self, PackError> {
                if region.len() < HEADER_LEN {
                        return Err(PackError::Truncated);
                }
                if region[..4] != MAGIC {
                        return Err(PackError::BadMagic);
                }
                if region[4] != VERSION {
                        return Err(PackError::UnsupportedVersion(region[4]));
                }
                let count = region[5] as usize;
                let total = u32::from_le_bytes([region[8], region[9], region[10], region[11]]) as usize;
                let dir_end = HEADER_LEN
                        .checked_add(count.checked_mul(ENTRY_LEN).ok_or(PackError::Truncated)?)
                        .ok_or(PackError::Truncated)?;
                if total < dir_end || total > region.len() {
                        return Err(PackError::Truncated);
                }
                let pack = Pack { blob: &region[..total], count };
                //   every entry checked once here, so a lookup afterwards is arithmetic on values
                // already known to be in range
                let mut previous: Option<&[u8]> = None;
                for i in 0..count {
                        let e = &pack.blob[HEADER_LEN + i * ENTRY_LEN..][..ENTRY_LEN];
                        let name = trim_name(&e[..NAME_LEN]);
                        if name.is_empty() {
                                return Err(PackError::BadDirectory);
                        }
                        if let Some(p) = previous {
                                if name <= p {
                                        return Err(PackError::BadDirectory);
                                }
                        }
                        previous = Some(name);
                        let off = u32::from_le_bytes([e[16], e[17], e[18], e[19]]) as usize;
                        let len = u32::from_le_bytes([e[20], e[21], e[22], e[23]]) as usize;
                        let end = off.checked_add(len).ok_or(PackError::Truncated)?;
                        if off < dir_end || end > total {
                                return Err(PackError::BadDirectory);
                        }
                }
                Ok(pack)
        }

        /// The digest the header carries.
        pub fn digest(&self) -> &'a [u8; DIGEST_LEN] {
                self.blob[16..HEADER_LEN].try_into().expect("header length is fixed")
        }

        /// The whole pack, exactly as long as its header says -- what a tool writes back out.
        pub fn as_bytes(&self) -> &'a [u8] {
                self.blob
        }

        /// How many entries the directory holds.
        pub fn len(&self) -> usize {
                self.count
        }

        pub fn is_empty(&self) -> bool {
                self.count == 0
        }

        /// The name of entry `index`, in the directory's ascending order.
        pub fn name(&self, index: usize) -> Option<&'a str> {
                let e = self.entry(index)?;
                core::str::from_utf8(trim_name(&e[..NAME_LEN])).ok()
        }

        /// The blob of entry `index`.
        pub fn blob(&self, index: usize) -> Option<&'a [u8]> {
                let e = self.entry(index)?;
                let off = u32::from_le_bytes([e[16], e[17], e[18], e[19]]) as usize;
                let len = u32::from_le_bytes([e[20], e[21], e[22], e[23]]) as usize;
                Some(&self.blob[off..off + len])
        }

        /// The blob named `name`.
        ///
        /// The error a caller acts on: a pack that opened but does not carry what this build asks
        /// for is as unusable as no pack at all, and for the same reason.
        pub fn get(&self, name: &str) -> Result<&'a [u8], PackError> {
                //   ascending names, so the search stops at the first name past the one wanted
                for i in 0..self.count {
                        match self.name(i) {
                                Some(n) if n == name => return self.blob(i).ok_or(PackError::NotFound),
                                Some(n) if n > name => break,
                                _ => {}
                        }
                }
                Err(PackError::NotFound)
        }

        fn entry(&self, index: usize) -> Option<&'a [u8]> {
                if index >= self.count {
                        return None;
                }
                Some(&self.blob[HEADER_LEN + index * ENTRY_LEN..][..ENTRY_LEN])
        }
}

fn trim_name(padded: &[u8]) -> &[u8] {
        match padded.iter().position(|&b| b == 0) {
                Some(n) => &padded[..n],
                None => padded,
        }
}

/// Building a pack: the writer side, used by the host tool that compiles the assets.
#[cfg(feature = "alloc")]
pub mod build {
        use super::*;
        use alloc::vec::Vec;

        /// Collects named blobs and emits a pack.
        #[derive(Default)]
        pub struct Builder {
                entries: Vec<([u8; NAME_LEN], Vec<u8>)>,
        }

        impl Builder {
                pub fn new() -> Self {
                        Self::default()
                }

                /// Add a blob under `name`: short, ASCII, and not already present.
                pub fn add(&mut self, name: &str, blob: &[u8]) -> Result<(), PackError> {
                        if name.is_empty() || name.len() > NAME_LEN || !name.is_ascii() {
                                return Err(PackError::BadName);
                        }
                        if name.bytes().any(|b| b == 0) {
                                return Err(PackError::BadName);
                        }
                        let mut padded = [0u8; NAME_LEN];
                        padded[..name.len()].copy_from_slice(name.as_bytes());
                        if self.entries.iter().any(|(n, _)| *n == padded) {
                                return Err(PackError::BadName);
                        }
                        self.entries.push((padded, blob.to_vec()));
                        Ok(())
                }

                /// Emit the pack.
                ///
                /// Entries are sorted here rather than demanded of the caller, so a build that
                /// lists its assets in a different order still produces the same bytes.
                pub fn build(&self) -> Vec<u8> {
                        let mut sorted: Vec<&([u8; NAME_LEN], Vec<u8>)> = self.entries.iter().collect();
                        sorted.sort_by(|a, b| a.0.cmp(&b.0));

                        let dir_end = HEADER_LEN + sorted.len() * ENTRY_LEN;
                        let mut payload = Vec::new();
                        let mut directory = Vec::with_capacity(sorted.len() * ENTRY_LEN);
                        for (name, blob) in &sorted {
                                while (dir_end + payload.len()) % 4 != 0 {
                                        payload.push(0);
                                }
                                let off = (dir_end + payload.len()) as u32;
                                directory.extend_from_slice(name);
                                directory.extend_from_slice(&off.to_le_bytes());
                                directory.extend_from_slice(&(blob.len() as u32).to_le_bytes());
                                payload.extend_from_slice(blob);
                        }

                        let total = (dir_end + payload.len()) as u32;
                        let mut out = Vec::with_capacity(total as usize);
                        out.extend_from_slice(&MAGIC);
                        out.push(VERSION);
                        out.push(sorted.len() as u8);
                        out.extend_from_slice(&[0, 0]);
                        out.extend_from_slice(&total.to_le_bytes());
                        out.extend_from_slice(&[0, 0, 0, 0]);
                        //   the digest goes in last, because it is taken over what is written here
                        out.extend_from_slice(&[0u8; DIGEST_LEN]);
                        out.extend_from_slice(&directory);
                        out.extend_from_slice(&payload);

                        let mut h = Sha256::new();
                        h.update(&out[..16]);
                        h.update(&out[HEADER_LEN..]);
                        out[16..HEADER_LEN].copy_from_slice(&h.finalize());
                        out
                }
        }
}

#[cfg(all(test, feature = "alloc"))]
mod tests {
        use super::build::Builder;
        use super::*;
        use alloc::vec;

        fn pack_of(pairs: &[(&str, &[u8])]) -> alloc::vec::Vec<u8> {
                let mut b = Builder::new();
                for (n, v) in pairs {
                        b.add(n, v).expect("name is usable");
                }
                b.build()
        }

        fn digest_of(bytes: &[u8]) -> [u8; DIGEST_LEN] {
                let p = Pack::open_unchecked(bytes).expect("built packs open");
                *p.digest()
        }

        #[test]
        fn a_built_pack_reads_back_every_blob_it_was_given() {
                let bytes = pack_of(&[("font", &[1, 2, 3]), ("theme", &[9; 70]), ("ui", &[4, 5])]);
                let d = digest_of(&bytes);
                let p = Pack::open(&bytes, &d).expect("its own digest matches");
                assert_eq!(p.len(), 3);
                assert_eq!(p.get("font"), Ok(&[1u8, 2, 3][..]));
                assert_eq!(p.get("theme"), Ok(&[9u8; 70][..]));
                assert_eq!(p.get("ui"), Ok(&[4u8, 5][..]));
                assert_eq!(p.get("nothing"), Err(PackError::NotFound));
        }

        #[test]
        fn entries_are_ordered_and_aligned_whatever_order_they_were_added_in() {
                let one = pack_of(&[("ui", &[4, 5]), ("font", &[1, 2, 3]), ("theme", &[6])]);
                let two = pack_of(&[("font", &[1, 2, 3]), ("theme", &[6]), ("ui", &[4, 5])]);
                assert_eq!(one, two, "the same assets make the same pack");
                let p = Pack::open_unchecked(&one).expect("opens");
                assert_eq!((p.name(0), p.name(1), p.name(2)), (Some("font"), Some("theme"), Some("ui")));
                for i in 0..p.len() {
                        let blob = p.blob(i).expect("in range");
                        let off = blob.as_ptr() as usize - one.as_ptr() as usize;
                        assert_eq!(off % 4, 0, "entry {i} starts on a four-byte boundary");
                }
        }

        #[test]
        fn a_pack_opened_against_another_packs_digest_is_refused() {
                let mine = pack_of(&[("font", &[1, 2, 3])]);
                let theirs = pack_of(&[("font", &[3, 2, 1])]);
                assert_eq!(Pack::open(&theirs, &digest_of(&mine)), Err(PackError::DigestMismatch));
        }

        #[test]
        fn an_edited_blob_is_caught_even_though_the_header_still_says_the_old_digest() {
                let mut bytes = pack_of(&[("ui", &[1, 2, 3, 4])]);
                let d = digest_of(&bytes);
                let last = bytes.len() - 1;
                bytes[last] ^= 0xff;
                assert_eq!(Pack::open(&bytes, &d), Err(PackError::DigestMismatch));
        }

        #[test]
        fn blank_storage_and_a_future_schema_are_told_apart() {
                assert_eq!(Pack::open_unchecked(&[0xff; 64]), Err(PackError::BadMagic));
                assert_eq!(Pack::open_unchecked(&[0u8; 8]), Err(PackError::Truncated));
                let mut bytes = pack_of(&[("font", &[1])]);
                bytes[4] = VERSION + 1;
                assert_eq!(Pack::open_unchecked(&bytes), Err(PackError::UnsupportedVersion(VERSION + 1)));
        }

        #[test]
        fn a_pack_opens_inside_a_region_much_larger_than_itself() {
                let bytes = pack_of(&[("font", &[7; 11])]);
                let d = digest_of(&bytes);
                let mut region = vec![0xffu8; 4096];
                region[..bytes.len()].copy_from_slice(&bytes);
                let p = Pack::open(&region, &d).expect("the trailing blank storage is not part of it");
                assert_eq!(p.as_bytes().len(), bytes.len());
                assert_eq!(p.get("font"), Ok(&[7u8; 11][..]));
        }

        #[test]
        fn a_truncated_or_disordered_directory_is_refused() {
                let bytes = pack_of(&[("font", &[1, 2, 3]), ("ui", &[4])]);
                //   a length field reaching past the pack
                let mut cut = bytes.clone();
                cut[HEADER_LEN + 20..HEADER_LEN + 24].copy_from_slice(&u32::MAX.to_le_bytes());
                assert!(matches!(Pack::open_unchecked(&cut), Err(PackError::Truncated | PackError::BadDirectory)));
                //   the two names swapped, so the directory no longer ascends
                let mut swapped = bytes.clone();
                let (a, b) = (HEADER_LEN, HEADER_LEN + ENTRY_LEN);
                for i in 0..NAME_LEN {
                        swapped.swap(a + i, b + i);
                }
                assert_eq!(Pack::open_unchecked(&swapped), Err(PackError::BadDirectory));
        }

        #[test]
        fn the_writer_refuses_a_name_it_could_not_store_or_find_again() {
                let mut b = Builder::new();
                assert_eq!(b.add("", &[1]), Err(PackError::BadName));
                assert_eq!(b.add("a_name_far_too_long_for_the_directory", &[1]), Err(PackError::BadName));
                assert_eq!(b.add("ünïcode", &[1]), Err(PackError::BadName));
                b.add("font", &[1]).expect("a plain name is fine");
                assert_eq!(b.add("font", &[2]), Err(PackError::BadName), "twice over is not");
        }

        #[test]
        fn the_digest_is_a_sha_256_of_the_two_documented_ranges() {
                let bytes = pack_of(&[("font", &[1, 2, 3])]);
                let mut h = Sha256::new();
                h.update(&bytes[..16]);
                h.update(&bytes[HEADER_LEN..]);
                assert_eq!(&bytes[16..HEADER_LEN], h.finalize().as_slice());
        }
}
