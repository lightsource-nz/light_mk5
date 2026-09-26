//! The portable part of networking: what a network gave, what a transfer can do instead of
//! succeeding, and the wire formats.
//!
//! WHAT IS DELIBERATELY ABSENT. There is no socket here, no protocol stack and no asynchrony: a
//! stack is built on the packet interface of whatever hardware carries the packets and cannot be
//! separated from it, so the reading and writing belong beside that hardware. What belongs here is
//! everything above the bytes -- and it is where the defects are. Finding the end of a response head
//! that arrived in three pieces, reading a length whose header name is spelled differently by
//! different servers, keeping the body bytes that came early: none of that needs a network to
//! exercise, and inside a driver it could only be exercised by pointing a board at a server and
//! hoping the server was awkward in the right way.

#![no_std]
#![forbid(unsafe_code)]

/// What a network handed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Config {
        pub address: [u8; 4],
        /// How many leading bits of the address are the network's.
        pub prefix: u8,
        /// The way out, if one was offered.
        ///
        ///   WORTH CARRYING AS AN OPTION rather than assuming there is one: an address without a way
        /// out reaches the hosts of its own network and nothing beyond it. That is a transfer which
        /// fails much later for a reason nobody would look for here, so it is reported at the moment
        /// it is learned instead.
        pub gateway: Option<[u8; 4]>,
}

/// Why there is no address.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
        /// None was handed out in the time allowed. The network either did not answer or refused.
        NoAddress,
        /// A stack was already built on this hardware.
        AlreadyBuilt,
}

/// What a transfer hands out, in the order it becomes known.
#[derive(Debug, PartialEq, Eq)]
pub enum Incoming<'a> {
        /// How long the body is, told ONCE and before any of it.
        ///
        /// Not a convenience: what is being fetched has to be given its size before its first byte
        /// -- storage is erased to fit -- and that is the reason a length is demanded of the server
        /// rather than merely read from it.
        Length(u32),
        /// A piece of the body, in the order it arrived. Not kept here afterwards.
        Body(&'a [u8]),
}

/// Why a transfer did not deliver.
///
/// Each of these sends a reader somewhere different, which is the whole reason they are told apart.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchError {
        /// There is no network to transfer over.
        NoNetwork,
        /// The other end refused the connection or never answered it.
        NoConnection,
        /// The connection broke part-way.
        Broken,
        /// The answer was not one this understands: not a reply at all, or a reply whose head is
        /// longer than there is room to read it in.
        NotAnAnswer,
        /// The server answered, and its answer was a refusal. The code is carried so that a missing
        /// file and a broken server are told apart -- the difference between a typo and an outage.
        Refused(u16),
        /// The answer did not say how long it was. See [`Incoming::Length`] for why that is fatal
        /// rather than merely unhelpful.
        NoLength,
        /// The body stopped before the length promised.
        Short,
        /// The request would not fit the room given for it. Ours rather than the server's -- an
        /// absurd path, or a buffer sized too small -- and named separately so it is not mistaken
        /// for something the far end did.
        RequestTooLong,
        /// Whatever was being written to could not take it.
        Rejected,
        /// It did not finish in the time allowed.
        TooSlow,
}

/// How much of a response head has been understood.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Head {
        /// The head is not complete. Read more and offer everything again.
        NeedMore,
        /// The head is complete.
        Complete {
                /// The code the server answered with.
                status: u16,
                /// The length it declared, if it declared one.
                length: Option<u32>,
                /// Where the body begins in what was offered.
                ///
                ///   REPORTED BECAUSE THE BODY ARRIVES WITH THE HEAD. A response small enough, or a
                /// link fast enough, delivers both in one read -- so a reader that starts the body
                /// from the next read loses however much came early. It is not an edge case: it
                /// happens on almost every transfer.
                body_at: usize,
        },
        /// Not a response at all. Nothing further will make it one.
        Malformed,
}

/// Format a request for `path` at `host` into `buf`, returning its length.
///
/// `None` if the buffer is too small -- which is the caller's to size, and a truncated request is
/// not something to send and find out about later.
///
///   THE CONNECTION IS ASKED TO CLOSE when the body is done, which is what makes a body readable
/// without trusting anything but its end; the length is still required, to know the body was all of
/// it. The host is given as the address because an address is what was dialled -- see the note on
/// names in the subsystem document.
pub fn get_request(buf: &mut [u8], path: &str, host: [u8; 4]) -> Option<usize> {
        let mut w = Cursor { buf, len: 0 };
        use core::fmt::Write as _;
        write!(w, "GET {path} HTTP/1.1\r\nHost: {}.{}.{}.{}\r\nConnection: close\r\n\r\n", host[0], host[1], host[2], host[3]).ok()?;
        Some(w.len)
}

/// Read what has arrived so far and say whether the response head is complete.
///
/// Offered the whole of what has been received each time, not just the newest piece: a head may
/// arrive in any number of reads and the boundary that ends it may fall between two of them.
///
/// `room_exhausted` is the caller saying it has no space for more -- a head that will not fit is an
/// answer that cannot be used, rather than a reason to keep reading forever.
pub fn read_head(received: &[u8], room_exhausted: bool) -> Head {
        let Some(at) = find(received, b"\r\n\r\n") else {
                return if room_exhausted { Head::Malformed } else { Head::NeedMore };
        };
        let body_at = at + 4;
        let head = &received[..body_at];
        match status_of(head) {
                Some(status) => Head::Complete { status, length: content_length(head), body_at },
                None => Head::Malformed,
        }
}

/// Somewhere to write into, so a request can be built with the ordinary formatting machinery rather
/// than by hand. Anything past the end is a failure rather than a truncation.
struct Cursor<'a> {
        buf: &'a mut [u8],
        len: usize,
}

impl core::fmt::Write for Cursor<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let end = self.len.checked_add(s.len()).ok_or(core::fmt::Error)?;
                if end > self.buf.len() {
                        return Err(core::fmt::Error);
                }
                self.buf[self.len..end].copy_from_slice(s.as_bytes());
                self.len = end;
                Ok(())
        }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        if needle.len() > haystack.len() {
                return None;
        }
        haystack.windows(needle.len()).position(|w| w == needle)
}

/// The number in `HTTP/1.1 200 OK`.
fn status_of(head: &[u8]) -> Option<u16> {
        let line = &head[..find(head, b"\r\n")?];
        let mut parts = line.split(|b| *b == b' ');
        let version = parts.next()?;
        //   checked, because a server that answers something other than this protocol is not
        // answering a request of ours, and reading the second word of whatever it did send would
        // turn a wrong answer into a plausible one
        if !version.starts_with(b"HTTP/") {
                return None;
        }
        let code = parts.next()?;
        core::str::from_utf8(code).ok()?.parse().ok()
}

/// How long the body says it is. The name is matched without regard to case, because the standard
/// allows any and servers use several.
fn content_length(head: &[u8]) -> Option<u32> {
        const NAME: &[u8] = b"content-length:";
        let mut at = 0;
        while at < head.len() {
                let end = at + find(&head[at..], b"\r\n")?;
                let line = &head[at..end];
                if line.len() > NAME.len() && line[..NAME.len()].eq_ignore_ascii_case(NAME) {
                        let value = core::str::from_utf8(&line[NAME.len()..]).ok()?;
                        return value.trim().parse().ok();
                }
                at = end + 2;
        }
        None
}

#[cfg(test)]
mod tests {
        use super::*;

        const OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 12\r\nConnection: close\r\n\r\n";

        #[test]
        fn a_complete_head_gives_the_status_the_length_and_where_the_body_starts() {
                match read_head(OK, false) {
                        Head::Complete { status, length, body_at } => {
                                assert_eq!(status, 200);
                                assert_eq!(length, Some(12));
                                assert_eq!(body_at, OK.len());
                        }
                        other => panic!("expected a complete head, got {other:?}"),
                }
        }

        #[test]
        fn the_body_that_arrived_with_the_head_is_not_lost() {
                //   this is the ordinary case rather than an edge one: a small response, or a fast
                // link, delivers head and body in one read
                let mut both = [0u8; 128];
                both[..OK.len()].copy_from_slice(OK);
                both[OK.len()..OK.len() + 5].copy_from_slice(b"hello");
                let received = &both[..OK.len() + 5];
                match read_head(received, false) {
                        Head::Complete { body_at, .. } => assert_eq!(&received[body_at..], b"hello"),
                        other => panic!("expected a complete head, got {other:?}"),
                }
        }

        #[test]
        fn a_head_split_across_reads_is_only_complete_once_it_is() {
                //   offered a prefix at every length, it must say NeedMore right up until the final
                // byte of the terminator arrives -- including where the split falls INSIDE it
                for n in 0..OK.len() {
                        assert_eq!(read_head(&OK[..n], false), Head::NeedMore, "a {n}-byte prefix was called complete");
                }
                assert!(matches!(read_head(OK, false), Head::Complete { .. }));
        }

        #[test]
        fn a_head_with_no_room_left_is_an_answer_that_cannot_be_used() {
                //   the caller has nowhere to put more, so waiting for the rest would be waiting
                // forever -- and an unbounded read is how a server that never terminates its head
                // would hold a device open indefinitely
                assert_eq!(read_head(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n", true), Head::Malformed);
                assert_eq!(read_head(b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\n", false), Head::NeedMore);
        }

        #[test]
        fn a_refusal_carries_the_servers_own_code() {
                //   the difference between a path typed wrongly and a server that has fallen over
                let missing = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n";
                match read_head(missing, false) {
                        Head::Complete { status, .. } => assert_eq!(status, 404),
                        other => panic!("expected a complete head, got {other:?}"),
                }
        }

        #[test]
        fn a_missing_length_is_reported_as_missing_rather_than_guessed() {
                let no_length = b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n";
                match read_head(no_length, false) {
                        Head::Complete { length, .. } => assert_eq!(length, None),
                        other => panic!("expected a complete head, got {other:?}"),
                }
        }

        #[test]
        fn the_length_header_is_found_whatever_its_spelling() {
                for head in [
                        b"HTTP/1.1 200 OK\r\ncontent-length: 7\r\n\r\n".as_slice(),
                        b"HTTP/1.1 200 OK\r\nCONTENT-LENGTH:7\r\n\r\n".as_slice(),
                        b"HTTP/1.1 200 OK\r\nContent-Length:   7   \r\n\r\n".as_slice(),
                        b"HTTP/1.1 200 OK\r\nServer: x\r\nContent-Length: 7\r\nEtag: y\r\n\r\n".as_slice(),
                ] {
                        match read_head(head, false) {
                                Head::Complete { length, .. } => assert_eq!(length, Some(7), "in {:?}", core::str::from_utf8(head)),
                                other => panic!("expected a complete head, got {other:?}"),
                        }
                }
        }

        #[test]
        fn a_header_merely_ending_in_the_length_name_is_not_it() {
                //   a prefix match on the line would have accepted this one
                let head = b"HTTP/1.1 200 OK\r\nX-Content-Length: 99\r\n\r\n";
                match read_head(head, false) {
                        Head::Complete { length, .. } => assert_eq!(length, None),
                        other => panic!("expected a complete head, got {other:?}"),
                }
        }

        #[test]
        fn something_that_is_not_a_response_is_refused_rather_than_read() {
                //   the second word of a non-response would parse as a status and turn a wrong
                // answer into a plausible one
                assert_eq!(read_head(b"220 smtp ready\r\n\r\n", false), Head::Malformed);
                assert_eq!(read_head(b"\r\n\r\n", false), Head::Malformed);
                assert_eq!(read_head(b"GARBAGE 200 OK\r\n\r\n", false), Head::Malformed);
        }

        #[test]
        fn a_request_names_the_path_the_host_and_asks_the_connection_to_close() {
                let mut buf = [0u8; 128];
                let n = get_request(&mut buf, "/firmware.bin", [192, 168, 88, 60]).expect("room");
                let text = core::str::from_utf8(&buf[..n]).expect("text");
                assert_eq!(text, "GET /firmware.bin HTTP/1.1\r\nHost: 192.168.88.60\r\nConnection: close\r\n\r\n");
        }

        #[test]
        fn a_request_too_long_for_its_buffer_fails_rather_than_being_cut_short() {
                //   a truncated request is not something to send and find out about from the far end
                let mut buf = [0u8; 16];
                assert_eq!(get_request(&mut buf, "/a-path-far-longer-than-the-room", [10, 0, 0, 1]), None);
        }
}
