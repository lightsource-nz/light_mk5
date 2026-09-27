//! The radio contract: what credentials are, what a scan heard, why a join failed, and the seams a
//! radio driver implements.
//!
//! There is no hardware here and no vendor. What is here is the part of a radio's behaviour that is
//! the same whatever the part -- and that had been written twice, once in an application and once in
//! a port, each copy locally complete and the pair of them jointly wrong.
//!
//! WHAT THIS CRATE IS FOR, stated plainly: a radio's driver is entitled to treat a command the part
//! refuses as fatal, because a part that refuses a command is a part in a state the driver cannot
//! reason about. That makes "never send the part something it will refuse" a safety property, not a
//! matter of politeness -- and the way to hold a safety property is to make its violation
//! unrepresentable rather than to check for it. So [`Credentials`] validates on construction and is
//! the only way to express a network to join: a driver that takes one cannot be handed a name or a
//! passphrase of a length the protocol forbids, and [`JoinError`] accordingly has no variant for it.

#![no_std]
#![forbid(unsafe_code)]

/// The longest name a network can have -- the width of the field it travels in.
pub const SSID_MAX: usize = 32;
/// The shortest passphrase a protected network accepts.
pub const PASSPHRASE_MIN: usize = 8;
/// The longest passphrase a protected network accepts.
pub const PASSPHRASE_MAX: usize = 63;

/// Why a set of credentials could not be built.
///
/// These are the protocol's limits, not a local policy: a name or passphrase outside them is not a
/// weak one, it is one the radio will not take, and it says so by refusing the command rather than
/// by failing to associate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialError {
        /// Empty, or longer than [`SSID_MAX`].
        Name,
        /// Not empty, and not between [`PASSPHRASE_MIN`] and [`PASSPHRASE_MAX`].
        ///
        /// Empty is not an error: it means a network that asks for none, which is
        /// [`Credentials::open`] and a different request rather than the same one with nothing in it.
        Passphrase,
}

/// A network to join: its name, and the passphrase if it is protected.
///
/// Valid by construction -- see the crate documentation for why that is the whole point.
///
/// `Copy`, because the ordinary way to get a set of credentials from a console to a radio is to put
/// it on an event bus, and an event is copied rather than moved.
#[derive(Clone, Copy)]
pub struct Credentials {
        ssid: [u8; SSID_MAX],
        ssid_len: u8,
        pass: [u8; PASSPHRASE_MAX],
        pass_len: u8,
}

impl Credentials {
        /// A protected network.
        pub fn new(ssid: &str, passphrase: &str) -> Result<Self, CredentialError> {
                if passphrase.len() < PASSPHRASE_MIN || passphrase.len() > PASSPHRASE_MAX {
                        return Err(CredentialError::Passphrase);
                }
                let mut c = Self::open(ssid)?;
                c.pass[..passphrase.len()].copy_from_slice(passphrase.as_bytes());
                c.pass_len = passphrase.len() as u8;
                Ok(c)
        }

        /// A network that asks for no passphrase.
        pub fn open(ssid: &str) -> Result<Self, CredentialError> {
                if ssid.is_empty() || ssid.len() > SSID_MAX {
                        return Err(CredentialError::Name);
                }
                let mut c = Self { ssid: [0; SSID_MAX], ssid_len: ssid.len() as u8, pass: [0; PASSPHRASE_MAX], pass_len: 0 };
                c.ssid[..ssid.len()].copy_from_slice(ssid.as_bytes());
                Ok(c)
        }

        pub fn ssid(&self) -> &str {
                //   the bytes came from a &str and their length is unchanged since, so this cannot
                // fail; the fallback keeps the accessor infallible rather than asserting
                core::str::from_utf8(&self.ssid[..usize::from(self.ssid_len)]).unwrap_or("")
        }

        /// The passphrase, or empty for an open network.
        pub fn passphrase(&self) -> &str {
                core::str::from_utf8(&self.pass[..usize::from(self.pass_len)]).unwrap_or("")
        }

        pub fn is_open(&self) -> bool {
                self.pass_len == 0
        }
}

//   THE PASSPHRASE IS NOT IN IT, and this is written out by hand for exactly that reason. A derived
// Debug would put it in every log line that ever reports a set of credentials, and a console
// transcript pasted into a bug report would carry it. The length is given because it is the useful
// diagnostic -- it tells a wrong passphrase apart from one that quoting swallowed -- and it does not
// disclose the secret.
impl core::fmt::Debug for Credentials {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                f.debug_struct("Credentials").field("ssid", &self.ssid()).field("passphrase_len", &self.pass_len).finish()
        }
}

/// What protection a network offers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Security {
        /// None at all.
        Open,
        /// The older key exchange only.
        Wpa2,
        /// The newer key exchange only.
        Wpa3,
        /// Both, offered together, so a station chooses. Worth its own name because a part that
        /// offers the newer one and is refused has no way to withdraw the offer -- see
        /// [`JoinError::Refused`].
        Wpa2Wpa3,
}

/// One radio heard during a scan.
///
/// A radio, not a network: several radios serving one name is the ordinary shape of a network that
/// covers a building, and it is [`ScanEntry::bssid`] that tells them apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanEntry {
        ssid: [u8; SSID_MAX],
        ssid_len: u8,
        /// The address of the radio that sent the beacon.
        pub bssid: [u8; 6],
        /// The signal of THIS SIGHTING, in dBm -- a sample, not a measurement. Successive beacons
        /// from one place differ by several decibels, so it answers "is this in range", not "which
        /// of these is stronger".
        pub rssi: i16,
        /// The channel it was heard on.
        pub channel: u8,
}

impl ScanEntry {
        pub fn new(ssid: &str, bssid: [u8; 6], rssi: i16, channel: u8) -> Self {
                let len = ssid.len().min(SSID_MAX);
                let mut e = ScanEntry { ssid: [0; SSID_MAX], ssid_len: len as u8, bssid, rssi, channel };
                e.ssid[..len].copy_from_slice(&ssid.as_bytes()[..len]);
                e
        }

        /// The name, or empty where the network withholds it.
        pub fn ssid(&self) -> &str {
                core::str::from_utf8(&self.ssid[..usize::from(self.ssid_len)]).unwrap_or("")
        }

        /// Whether the network withheld its name. Not the same as absent: such a network is there
        /// and joinable by anyone who knows what it is called.
        pub fn hidden(&self) -> bool {
                self.ssid_len == 0
        }
}

/// Tells a radio heard for the first time from one heard again.
///
///   WHY A SCAN NEEDS THIS AT ALL. What a sweep reports is every beacon it receives, and a sweep
/// revisits each channel -- so a radio in the room answers several times, with a different signal
/// each time. Reported as they arrive, those are indistinguishable from several networks that cannot
/// be told apart.
///
///   Kept apart by the sending address rather than by the name, because two radios serving one name
/// are genuinely two things worth seeing and one radio answering twice is not.
pub struct Sightings<const N: usize> {
        seen: [[u8; 6]; N],
        len: usize,
        offered: usize,
}

impl<const N: usize> Default for Sightings<N> {
        fn default() -> Self {
                Self::new()
        }
}

impl<const N: usize> Sightings<N> {
        pub const fn new() -> Self {
                Self { seen: [[0; 6]; N], len: 0, offered: 0 }
        }

        /// Offer a sighting. `true` the first time this radio is heard, `false` for a repeat.
        ///
        ///   PAST CAPACITY IT RETURNS TRUE, so the list grows repeats rather than losing entries.
        /// That is the better failure of the two: a long list with repeats in it still says what is
        /// in range, where dropping the overflow would quietly claim a network is absent.
        pub fn offer(&mut self, bssid: &[u8; 6]) -> bool {
                self.offered += 1;
                if self.seen[..self.len].contains(bssid) {
                        return false;
                }
                if self.len < N {
                        self.seen[self.len] = *bssid;
                        self.len += 1;
                }
                true
        }

        /// How many distinct radios have been heard.
        pub fn len(&self) -> usize {
                self.len
        }

        pub fn is_empty(&self) -> bool {
                self.len == 0
        }

        /// How many sightings were offered, repeats included. Larger than [`Self::len`] on any real
        /// sweep, and the gap is the reason this type exists.
        pub fn offered(&self) -> usize {
                self.offered
        }

        /// Whether capacity is used up, and so whether repeats may reappear.
        pub fn saturated(&self) -> bool {
                self.len == N
        }

        pub fn reset(&mut self) {
                self.len = 0;
                self.offered = 0;
        }
}

/// Why a network was not joined.
///
///   NO VARIANT FOR MALFORMED CREDENTIALS, and that absence is deliberate: [`Credentials`] cannot be
/// built wrong, so a driver cannot be handed something the radio would refuse. What is left is the
/// set of answers that come from the air.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinError {
        /// No network of that name was within range.
        NotFound,
        /// Nothing was heard back at all in the time allowed. Worth telling apart from a refusal,
        /// because the thing to check is different.
        NoAnswer,
        /// It was found and would not have us.
        ///
        ///   TWO CAUSES, AND THE SECOND IS THE ONE PEOPLE MISS. The passphrase, usually -- but also
        /// a network whose protection this part cannot complete. Where a network offers both the
        /// older and the newer key exchange, a part may offer the newer one, be refused, and have no
        /// way to withdraw the offer. A report that blames only the passphrase sends people to check
        /// the one thing that is right.
        Refused,
        /// The radio reported a refusal this contract has no name for. The code is the driver's own.
        Other(u32),
        /// The radio cannot attempt another join until it is powered again.
        ///
        ///   FOR A PART THAT CANNOT RE-ARM ITSELF, and refusing is then the point: the alternative
        /// is not a failed join but a stopped board, because joining issues commands such a part
        /// accepts only while its interface is down, and letting go of a network does not bring it
        /// down. It is the ATTEMPT that counts and not the outcome -- a join that failed has
        /// configured the interface just the same.
        ///
        ///   THE REFERENCE DRIVER NO LONGER RETURNS THIS, because it takes the interface down and
        /// back up instead and simply tries again -- which is what anyone who mistypes a passphrase
        /// expects. The variant stays because the limitation belongs to the part rather than to the
        /// contract, and a driver that cannot re-arm needs a way to say so that is not a panic.
        CannotRetry,
}

/// Why a radio could not be brought up.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WirelessError {
        /// The part did not answer on its bus. Nothing above this can proceed.
        NoPart,
        /// The part answered, but not as the part this driver is for.
        WrongPart,
        /// The firmware the part was given was refused, or never took effect.
        Firmware,
}

/// A radio that joins wireless networks.
///
/// Bringing one up is slow and blocking on purpose: a quarter of a megabyte crosses the bus before
/// there is a radio at all, and there is nothing useful to do in the meantime.
pub trait WifiRadio {
        /// Power the part, upload its firmware and settings, and read back its address.
        ///
        /// The address is READ OUT OF THE RUNNING PART, not out of the image: getting one is the
        /// proof that the bus carried the whole upload correctly and that what is on the other end
        /// of it is now running.
        fn bring_up(&mut self) -> Result<[u8; 6], WirelessError>;

        /// The part's address, once it is up.
        fn address(&mut self) -> [u8; 6];

        /// Join a network. Blocking, and it can take seconds.
        fn join(&mut self, credentials: &Credentials) -> Result<(), JoinError>;

        /// Sweep for radios, reporting each distinct one once. Returns how many were reported.
        ///
        /// Needs no credentials, which is the point of having it: it separates "that network is not
        /// there" from "this radio is not receiving", and those are the two explanations for a join
        /// that comes back saying it found nothing.
        fn scan(&mut self, found: &mut dyn FnMut(&ScanEntry)) -> usize;

        /// Give the driver its pass of the runtime loop. Radio stacks are written for an executor
        /// and this framework has none, so every future in one is polled from here.
        fn poll(&mut self);
}

/// A general-purpose pin that belongs to the radio rather than to the board.
///
/// Worth a seam of its own because on a board where a radio is fitted, the pin that carries an
/// indicator on the plain variant is the radio's chip select -- so such a board's indicator hangs off
/// the radio's own pins, and driving it means asking the radio.
pub trait RadioPins {
        fn set_gpio(&mut self, pin: u8, on: bool);
}

/// The short-range radio a combined part carries beside its wireless side.
///
/// ADDITIVE, NEVER AN ALTERNATIVE: it shares the bus, the power line and the upload, and asking for
/// it changes nothing about the wireless side. A part that answers on one and not the other is
/// reported and carried on with.
pub trait BluetoothController {
        /// Reset the controller and read back its address, or `None` if it did not answer.
        ///
        /// It may arrive byte-reversed against the wireless address: the protocol reports it
        /// little-endian, and one part's two radios commonly share an address block.
        fn bluetooth_address(&mut self) -> Option<[u8; 6]>;
}

//   THE FRAMEWORK DECLARES NO PACKET INTERFACE OF ITS OWN FOR THE SHORT-RANGE SIDE, and the absence
// is deliberate. One was written here -- a byte seam for a host stack to sit on -- before it was
// checked against what the stack and the driver already agree on, and they agree on the ecosystem's
// own HCI crate: the driver implements ITS transport trait, and every host stack consumes that
// trait. A framework trait in between would have had an implementation on one side, an adapter on
// the other, and no consumer at all.
//
//   Which is the general rule this stands for: a seam earns its place by being the thing two sides
// would otherwise disagree about. Where they already agree, declaring one adds a boundary the
// framework does not own and cannot change.

#[cfg(test)]
mod tests {
        use super::*;
        use core::fmt::Write;

        //   ONE LITERAL, SLICED, so the lengths these tests turn on cannot be miscounted: a test
        // that meant to offer 64 characters and offered 63 would pass while proving the opposite.
        // The assertion below is what makes that a failure rather than a silent wrong test.
        const X: &str = "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";
        const N: &str = "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn";

        #[test]
        fn the_fixtures_are_the_lengths_the_other_tests_assume() {
                assert_eq!(X.len(), 64);
                assert_eq!(N.len(), 40);
        }

        /// Somewhere to render a `Debug` without an allocator -- this crate has none, and gaining one
        /// for a test would be a poor trade.
        struct Buf {
                bytes: [u8; 256],
                len: usize,
        }

        impl Buf {
                fn new() -> Self {
                        Self { bytes: [0; 256], len: 0 }
                }

                fn as_str(&self) -> &str {
                        core::str::from_utf8(&self.bytes[..self.len]).expect("what was written was text")
                }
        }

        impl Write for Buf {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                        let end = self.len + s.len();
                        if end > self.bytes.len() {
                                return Err(core::fmt::Error);
                        }
                        self.bytes[self.len..end].copy_from_slice(s.as_bytes());
                        self.len = end;
                        Ok(())
                }
        }

        #[test]
        fn a_passphrase_the_radio_would_refuse_cannot_be_built() {
                //   the length that stopped a board: seven is short, and the part answers a short
                // passphrase by refusing the command rather than by failing to associate
                assert_eq!(Credentials::new("net", &X[..7]).unwrap_err(), CredentialError::Passphrase);
                assert_eq!(Credentials::new("net", X).unwrap_err(), CredentialError::Passphrase);
                assert!(Credentials::new("net", &X[..8]).is_ok());
                assert!(Credentials::new("net", &X[..63]).is_ok());
        }

        #[test]
        fn an_empty_passphrase_is_a_different_request_not_a_short_one() {
                //   and so it is reached through its own constructor rather than by passing nothing
                assert_eq!(Credentials::new("net", "").unwrap_err(), CredentialError::Passphrase);
                let open = Credentials::open("net").expect("a named open network");
                assert!(open.is_open());
                assert_eq!(open.passphrase(), "");
        }

        #[test]
        fn a_name_must_fit_the_field_it_travels_in() {
                assert_eq!(Credentials::open("").unwrap_err(), CredentialError::Name);
                assert_eq!(Credentials::open(&N[..33]).unwrap_err(), CredentialError::Name);
                let c = Credentials::open(&N[..32]).expect("the longest name there is");
                assert_eq!(c.ssid().len(), 32);
        }

        #[test]
        fn credentials_read_back_exactly_what_went_in() {
                let c = Credentials::new("Two Words", "correct horse").expect("valid");
                assert_eq!(c.ssid(), "Two Words");
                assert_eq!(c.passphrase(), "correct horse");
                assert!(!c.is_open());
        }

        #[test]
        fn the_debug_report_never_carries_the_passphrase() {
                //   a console transcript in a bug report must not disclose it, and the length is the
                // diagnostic that matters anyway
                let c = Credentials::new("net", "hunter2hunter2").expect("valid");
                let mut buf = Buf::new();
                write!(&mut buf, "{c:?}").expect("room");
                let shown = buf.as_str();
                assert!(!shown.contains("hunter2"), "the passphrase appeared in {shown}");
                assert!(shown.contains("passphrase_len"));
                assert!(shown.contains("net"));
        }

        const A: [u8; 6] = [0x3c, 0x6a, 0x48, 0xb8, 0x65, 0xc9];
        const B: [u8; 6] = [0x3c, 0x6a, 0x48, 0xb8, 0x66, 0x7d];

        #[test]
        fn a_radio_heard_again_is_not_a_second_radio() {
                let mut s: Sightings<8> = Sightings::new();
                assert!(s.offer(&A));
                assert!(!s.offer(&A));
                assert!(!s.offer(&A));
                assert_eq!(s.len(), 1);
                assert_eq!(s.offered(), 3);
        }

        #[test]
        fn two_radios_serving_one_name_stay_two() {
                //   the shape of a network covering a building: same name, same channel, different
                // radio -- and the whole reason the address is the key rather than the name
                let mut s: Sightings<8> = Sightings::new();
                assert!(s.offer(&A));
                assert!(s.offer(&B));
                assert_eq!(s.len(), 2);
        }

        #[test]
        fn past_capacity_repeats_come_back_rather_than_entries_going_missing() {
                let mut s: Sightings<2> = Sightings::new();
                assert!(s.offer(&A));
                assert!(s.offer(&B));
                assert!(s.saturated());
                let c = [1, 2, 3, 4, 5, 6];
                //   reported, because a list with repeats still says what is in range
                assert!(s.offer(&c));
                assert!(s.offer(&c));
                //   and the ones already known are still suppressed
                assert!(!s.offer(&A));
                assert_eq!(s.len(), 2);
        }

        #[test]
        fn a_sweep_can_be_run_again() {
                let mut s: Sightings<4> = Sightings::new();
                s.offer(&A);
                s.reset();
                assert!(s.is_empty());
                assert_eq!(s.offered(), 0);
                assert!(s.offer(&A), "a new sweep hears it for the first time again");
        }

        #[test]
        fn a_withheld_name_is_reported_as_withheld_not_guessed_at() {
                let hidden = ScanEntry::new("", A, -85, 3);
                assert!(hidden.hidden());
                assert_eq!(hidden.ssid(), "");
                let named = ScanEntry::new("Caliban", A, -82, 10);
                assert!(!named.hidden());
                assert_eq!(named.ssid(), "Caliban");
                assert_eq!(named.channel, 10);
                assert_eq!(named.rssi, -82);
        }

        #[test]
        fn a_name_too_long_for_an_entry_is_cut_rather_than_refused() {
                //   a scan reports what is in the air, and what is in the air is not ours to reject:
                // an over-long name is truncated so the radio is still listed
                let e = ScanEntry::new(N, A, -70, 6);
                assert_eq!(e.ssid().len(), SSID_MAX);
        }
}
