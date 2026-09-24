//! Lifting a wireless part's firmware out of the vendor's source drop.
//!
//! A radio of this kind holds no firmware of its own: the host uploads an image into its RAM at
//! every power-up, and a second, much smaller blob of regulatory limits after it. The vendor ships
//! both as one C array inside a header, for a C driver to link into its image -- which is exactly
//! what this framework does not want, because the blob is a quarter of a megabyte and belongs in
//! storage beside the fonts rather than inside the firmware.
//!
//! So this reads the header and writes the two blobs out as files. The header is the authority: it
//! carries the array and, beside it, the two lengths as `#define`s, and those lengths are read
//! rather than assumed. The one piece of arithmetic is where the second blob begins -- the vendor's
//! own driver places it at the first 512-byte boundary at or after the end of the first, and a
//! reader that simply concatenated them would load a shifted image and get no radio and no reason
//! why.

/// Where the regulatory blob starts, relative to the array: the vendor driver's own rounding.
const CLM_ALIGN: usize = 512;

/// What went wrong reading a vendor header.
#[derive(Debug, PartialEq, Eq)]
pub enum RadioError {
        /// A `#define` the split depends on is not in this file, so it is not the header meant.
        MissingLength(&'static str),
        /// A length is there but is not a number.
        BadLength(&'static str),
        /// The array is shorter than the lengths say it is, so one blob would be truncated.
        Truncated { need: usize, found: usize },
}

impl core::fmt::Display for RadioError {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                match self {
                        RadioError::MissingLength(name) => write!(f, "this file has no {name}, so it is not a combined firmware header"),
                        RadioError::BadLength(name) => write!(f, "{name} is not a number"),
                        RadioError::Truncated { need, found } => {
                                write!(f, "the header's lengths need {need} bytes but its array holds {found}")
                        }
                }
        }
}

/// A radio's two blobs, lifted out of one header.
pub struct Firmware {
        /// The image uploaded into the radio's RAM at power-up.
        pub firmware: Vec<u8>,
        /// The regulatory limits, loaded once the image is running.
        pub clm: Vec<u8>,
}

/// Read the lengths and the array out of a vendor header, and cut the two blobs from it.
pub fn split(header: &str) -> Result<Firmware, RadioError> {
        let fw_len = define(header, "CYW43_WIFI_FW_LEN")?;
        let clm_len = define(header, "CYW43_CLM_LEN")?;
        let bytes = array_bytes(header);

        //   the vendor's driver reads the regulatory blob from the first 512-byte boundary at or
        // after the image, not from the byte after it: the gap between is padding
        let clm_at = fw_len.next_multiple_of(CLM_ALIGN);
        let need = clm_at + clm_len;
        if bytes.len() < need {
                return Err(RadioError::Truncated { need, found: bytes.len() });
        }

        Ok(Firmware {
                firmware: bytes[..fw_len].to_vec(),
                clm: bytes[clm_at..need].to_vec(),
        })
}

/// The value of a `#define NAME (123)` or `#define NAME 123`.
fn define(header: &str, name: &'static str) -> Result<usize, RadioError> {
        let line = header
                .lines()
                .find(|l| l.trim_start().starts_with("#define") && l.contains(name))
                .ok_or(RadioError::MissingLength(name))?;
        let after = line.split(name).nth(1).ok_or(RadioError::MissingLength(name))?;
        //   the value is whatever digits come first after the name; the vendor writes it
        // parenthesised and follows it with a comment naming the file it came from
        let digits: String = after.chars().skip_while(|c| !c.is_ascii_digit()).take_while(|c| c.is_ascii_digit()).collect();
        if digits.is_empty() {
                return Err(RadioError::BadLength(name));
        }
        digits.parse().map_err(|_| RadioError::BadLength(name))
}

/// Every `0x..` in the file, in order, as bytes.
///
/// Deliberately not a C parser: the array is the only thing in these headers written as hex byte
/// literals, and the `#define`s that follow it are decimal. Anything that made that untrue would
/// fail the length check rather than pass silently.
fn array_bytes(header: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let b = header.as_bytes();
        let mut i = 0;
        while i + 1 < b.len() {
                if b[i] == b'0' && (b[i + 1] == b'x' || b[i + 1] == b'X') {
                        let mut v: u32 = 0;
                        let mut n = 0;
                        let mut j = i + 2;
                        while j < b.len() && (b[j] as char).is_ascii_hexdigit() && n < 2 {
                                v = v * 16 + (b[j] as char).to_digit(16).unwrap();
                                n += 1;
                                j += 1;
                        }
                        if n > 0 {
                                out.push(v as u8);
                        }
                        i = j;
                } else {
                        i += 1;
                }
        }
        out
}

#[cfg(test)]
mod tests {
        use super::*;

        fn header(fw: usize, clm: usize, bytes: usize) -> String {
                let mut s = String::from("static const unsigned char combined[] = {\n");
                for i in 0..bytes {
                        s.push_str(&format!("0x{:02x}, ", (i & 0xff) as u8));
                }
                s.push_str("};\n");
                s.push_str(&format!("#define CYW43_WIFI_FW_LEN ({fw}) // launch_firmware/43439A0.bin\n"));
                s.push_str(&format!("#define CYW43_CLM_LEN ({clm}) // a_clm_blob\n"));
                s
        }

        #[test]
        fn the_two_blobs_come_out_at_the_lengths_the_header_states() {
                let h = header(600, 20, 1200);
                let f = split(&h).expect("splits");
                assert_eq!(f.firmware.len(), 600);
                assert_eq!(f.clm.len(), 20);
        }

        #[test]
        fn the_regulatory_blob_is_read_from_the_padded_boundary_not_the_byte_after() {
                //   600 rounds up to 1024, so the blob starts there -- the whole point of the
                // rounding, and the bug that would otherwise upload a shifted image
                let h = header(600, 4, 1200);
                let f = split(&h).expect("splits");
                assert_eq!(f.clm, vec![0x00, 0x01, 0x02, 0x03]);
        }

        #[test]
        fn a_blob_that_starts_on_a_boundary_is_not_pushed_to_the_next_one() {
                let h = header(512, 4, 600);
                let f = split(&h).expect("splits");
                assert_eq!(f.clm, vec![0x00, 0x01, 0x02, 0x03]);
        }

        #[test]
        fn an_array_too_short_for_the_stated_lengths_is_refused_rather_than_cut() {
                let h = header(600, 20, 1030);
                match split(&h) {
                        Err(RadioError::Truncated { need, found }) => assert_eq!((need, found), (1044, 1030)),
                        other => panic!("a short array should be refused, not cut: {:?}", other.err()),
                }
        }

        #[test]
        fn a_file_without_the_lengths_is_not_mistaken_for_a_firmware_header() {
                assert!(matches!(split("int x = 0x41;"), Err(RadioError::MissingLength(_))));
        }
}
