//! The `fs` console command, shared by every board with a card: parse it into an [`FsOp`] and two
//! [`FsPath`] arguments that ride an event bus by value, then [`run`] it against a [`Store`] --
//! ready the medium, mount the volume over a borrowed device, act, unmount by drop. The events it
//! travels in live in each application's own vocabulary, so the parse takes a constructor for them.
//!
//! Every operation reports on the log and returns nothing: this is a bring-up and inspection
//! instrument, not an API. [`exec`] is the part that acts on an already-mounted volume.

use light_core::cli::{Parsed, Words};
use light_core::hal::{BlockDevice, Store};
use light_core::info;

use crate::fat::{Fat, FsError};

/// The usage line for a command table built on [`parse_command`].
pub const USAGE: &str = "fs info|ls [P]|cat P|write P TEXT|rm P|mv A B|mkdir P|trunc P N|hex P OFF|peak P";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FsOp {
        /// The volume's type and geometry.
        Info,
        /// A directory's entries (the root with no path).
        Ls,
        /// The first bytes of a file, as text.
        Cat,
        /// Append one line of text, creating the file if it is not there.
        Write,
        /// Remove a file or an empty directory.
        Rm,
        /// Rename or move.
        Mv,
        Mkdir,
        /// Cut a file to a byte length.
        Trunc,
        /// Sixteen little-endian 16-bit samples from a byte offset, as signed decimals.
        Hex,
        /// The peak and RMS of a 16-bit PCM WAV's samples, past its 44-byte header.
        Peak,
}

/// A path argument small enough to ride the event bus by value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FsPath {
        buf: [u8; FsPath::MAX],
        len: u8,
}

impl FsPath {
        pub const MAX: usize = 48;
        pub const EMPTY: FsPath = FsPath { buf: [0; FsPath::MAX], len: 0 };

        /// `None` when the path is longer than [`FsPath::MAX`] bytes.
        pub fn new(s: &str) -> Option<Self> {
                if s.len() > Self::MAX {
                        return None;
                }
                let mut buf = [0u8; Self::MAX];
                buf[..s.len()].copy_from_slice(s.as_bytes());
                Some(Self { buf, len: s.len() as u8 })
        }

        pub fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..usize::from(self.len)]).unwrap_or("")
        }

        pub fn is_empty(&self) -> bool {
                self.len == 0
        }
}

/// Parse the words after `fs` into an operation and its two arguments (either may be empty), and
/// hand them to `make` for the caller's event. A missing operation is `info`; an operation missing
/// an argument it needs, or an argument over [`FsPath::MAX`], is the usage line.
pub fn parse_command<E>(w: &mut Words, make: impl FnOnce(FsOp, FsPath, FsPath) -> E) -> Parsed<E> {
        let op = match w.next() {
                Some("info") | None => FsOp::Info,
                Some("ls") => FsOp::Ls,
                Some("cat") => FsOp::Cat,
                Some("write") => FsOp::Write,
                Some("rm") => FsOp::Rm,
                Some("mv") => FsOp::Mv,
                Some("mkdir") => FsOp::Mkdir,
                Some("trunc") => FsOp::Trunc,
                Some("hex") => FsOp::Hex,
                Some("peak") => FsOp::Peak,
                _ => return Parsed::Usage,
        };
        let path = w.next().unwrap_or("");
        let arg = w.next().unwrap_or("");
        if !matches!(op, FsOp::Info | FsOp::Ls) && path.is_empty() {
                return Parsed::Usage;
        }
        if matches!(op, FsOp::Write | FsOp::Mv | FsOp::Trunc | FsOp::Hex) && arg.is_empty() {
                return Parsed::Usage;
        }
        match (FsPath::new(path), FsPath::new(arg)) {
                (Some(path), Some(arg)) => Parsed::Event(make(op, path, arg)),
                _ => Parsed::Usage,
        }
}

/// One `fs` command against the store's medium: ready it, mount the volume, act, unmount by drop.
/// An I/O failure while mounting may be a pulled or swapped card, so the store is reset and the
/// next command starts from power-up.
pub fn run<S: Store>(store: &mut S, op: FsOp, path: &str, arg: &str) {
        let Some(dev) = store.open() else {
                info!("fs: no card");
                return;
        };
        let mut fs = match Fat::mount(dev) {
                Ok(fs) => fs,
                Err(e) => {
                        info!("fs: mount failed: {e:?}");
                        if matches!(e, FsError::Io(_)) {
                                store.reset();
                        }
                        return;
                }
        };
        exec(&mut fs, op, path, arg);
}

/// One `fs` command against an already-mounted volume.
pub fn exec<D: BlockDevice>(fs: &mut Fat<D>, op: FsOp, path: &str, arg: &str) {
        match op {
                FsOp::Info => {
                        let v = fs.volume_info();
                        info!("fs: {}, {} clusters of {} bytes", if v.fat32 { "FAT32" } else { "FAT16" }, v.cluster_count, v.bytes_per_cluster);
                }
                FsOp::Ls => {
                        let mut count = 0u32;
                        let r = fs.list_dir(path, |e| {
                                count += 1;
                                match (e.is_dir, e.long_name()) {
                                        (true, Some(l)) => info!("  {}/  <{}>", e.name(), l),
                                        (true, None) => info!("  {}/", e.name()),
                                        (false, Some(l)) => info!("  {}  {} B  <{}>", e.name(), e.size, l),
                                        (false, None) => info!("  {}  {} B", e.name(), e.size),
                                }
                        });
                        match r {
                                Ok(()) => info!("fs: {count} entries"),
                                Err(e) => info!("fs: ls failed: {e:?}"),
                        }
                }
                FsOp::Write => {
                        //   create, or append when it already exists: `fs write LOG.TXT hello`
                        // twice is a two-line file
                        let mut f = match fs.create(path) {
                                Ok(f) => f,
                                Err(FsError::Exists) => match fs.append(path) {
                                        Ok(f) => f,
                                        Err(e) => {
                                                info!("fs: append failed: {e:?}");
                                                return;
                                        }
                                },
                                Err(e) => {
                                        info!("fs: create failed: {e:?}");
                                        return;
                                }
                        };
                        let mut data = [0u8; FsPath::MAX + 1];
                        let n = arg.len().min(FsPath::MAX);
                        data[..n].copy_from_slice(&arg.as_bytes()[..n]);
                        data[n] = b'\n';
                        match f.write(fs, &data[..n + 1]) {
                                Ok(w) => info!("fs: wrote {} B; {} is now {} B", w, path, f.size()),
                                Err(e) => info!("fs: write failed: {e:?}"),
                        }
                }
                FsOp::Rm => {
                        //   files and (empty) directories both; the entry decides
                        let r = match fs.stat(path) {
                                Ok(e) if e.is_dir => fs.rmdir(path),
                                Ok(_) => fs.remove(path),
                                Err(e) => Err(e),
                        };
                        match r {
                                Ok(()) => info!("fs: removed {}", path),
                                Err(e) => info!("fs: rm failed: {e:?}"),
                        }
                }
                FsOp::Mv => match fs.rename(path, arg) {
                        Ok(()) => info!("fs: {} -> {}", path, arg),
                        Err(e) => info!("fs: mv failed: {e:?}"),
                },
                FsOp::Mkdir => match fs.mkdir(path) {
                        Ok(()) => info!("fs: created {}/", path),
                        Err(e) => info!("fs: mkdir failed: {e:?}"),
                },
                FsOp::Hex => match (arg.parse::<u32>(), fs.open(path)) {
                        (Ok(off), Ok(mut f)) => {
                                //   16 samples as signed decimals: the shape of captured audio at a
                                // glance -- small around zero, rail-to-rail, or stuck
                                let mut b = [0u8; 32];
                                let r = f.seek(fs, off).and_then(|()| f.read(fs, &mut b));
                                match r {
                                        Ok(n) => {
                                                let mut line = [0i16; 16];
                                                for (i, s) in line.iter_mut().enumerate().take(n / 2) {
                                                        *s = i16::from_le_bytes([b[i * 2], b[i * 2 + 1]]);
                                                }
                                                info!("fs: {}@{}: {:?}", path, off, &line[..n / 2]);
                                        }
                                        Err(e) => info!("fs: hex failed: {e:?}"),
                                }
                        }
                        _ => info!("fs: hex needs PATH and a byte offset"),
                },
                FsOp::Trunc => match arg.parse::<u32>() {
                        Ok(len) => match fs.open(path) {
                                Ok(mut f) => match f.truncate(fs, len) {
                                        Ok(()) => info!("fs: {} is now {} B", path, f.size()),
                                        Err(e) => info!("fs: trunc failed: {e:?}"),
                                },
                                Err(e) => info!("fs: open failed: {e:?}"),
                        },
                        Err(_) => info!("fs: trunc needs a byte count"),
                },
                FsOp::Peak => match fs.open(path) {
                        //   the whole take's level in one line: what gain tuning needs, where
                        // 16-sample hex windows sample a sliver of a take and miss every real peak
                        Ok(mut f) => match pcm_level(fs, &mut f) {
                                Ok((peak, rms, n)) => info!("fs: {} peak {} rms {} over {} samples", path, peak, rms, n),
                                Err(e) => info!("fs: peak failed: {e:?}"),
                        },
                        Err(e) => info!("fs: open failed: {e:?}"),
                },
                FsOp::Cat => match fs.open(path) {
                        Ok(mut f) => {
                                //   a peek, not a pager: the first 120 bytes, as text
                                let mut buf = [0u8; 120];
                                match f.read(fs, &mut buf) {
                                        Ok(n) => {
                                                let text = core::str::from_utf8(&buf[..n]).unwrap_or("<binary>");
                                                info!("fs: {} ({} B): {}", path, f.size(), text);
                                        }
                                        Err(e) => info!("fs: read failed: {e:?}"),
                                }
                        }
                        Err(e) => info!("fs: open failed: {e:?}"),
                },
        }
}

/// The peak magnitude, the RMS and the sample count of a 16-bit PCM WAV's samples.
fn pcm_level<D: BlockDevice>(fs: &mut Fat<D>, f: &mut crate::fat::File) -> Result<(i32, u64, u64), FsError> {
        f.seek(fs, 44)?;
        let mut buf = [0u8; 512];
        let (mut peak, mut sum, mut n) = (0i32, 0u64, 0u64);
        loop {
                let got = f.read(fs, &mut buf)?;
                if got == 0 {
                        break;
                }
                for i in 0..got / 2 {
                        let s = i32::from(i16::from_le_bytes([buf[i * 2], buf[i * 2 + 1]]));
                        peak = peak.max(s.abs());
                        sum += (s * s) as u64;
                        n += 1;
                }
        }
        let mean = if n > 0 { sum / n } else { 0 };
        let mut rms = 0u64;
        while (rms + 1) * (rms + 1) <= mean {
                rms += 1;
        }
        Ok((peak, rms, n))
}

#[cfg(test)]
mod tests {
        use super::*;
        use crate::fat::tests::{fat16_superfloppy, MemDev};
        use light_core::hal::BlockError;

        extern crate std;

        #[derive(Debug, PartialEq)]
        struct Ev(FsOp, FsPath, FsPath);

        fn parse(line: &str) -> Parsed<Ev> {
                parse_command(&mut Words::new(line), Ev)
        }

        #[test]
        fn a_bare_fs_is_info_and_ops_carry_their_arguments() {
                assert!(matches!(parse(""), Parsed::Event(Ev(FsOp::Info, p, a)) if p.is_empty() && a.is_empty()));
                let Parsed::Event(Ev(op, p, a)) = parse("mv A.TXT B.TXT") else { panic!("expected an event") };
                assert_eq!((op, p.as_str(), a.as_str()), (FsOp::Mv, "A.TXT", "B.TXT"));
                assert!(matches!(parse("peak TAKE.WAV"), Parsed::Event(Ev(FsOp::Peak, _, _))));
        }

        #[test]
        fn a_missing_argument_or_an_overlong_path_is_the_usage_line() {
                let long = std::format!("cat {}", "x".repeat(FsPath::MAX + 1));
                for line in ["cat", "write LOG.TXT", "mv A.TXT", "trunc F", "hex F", "nope", long.as_str()] {
                        assert!(matches!(parse(line), Parsed::Usage), "{line}");
                }
        }

        #[test]
        fn write_creates_then_appends_a_line() {
                let mut fs = Fat::mount(fat16_superfloppy()).unwrap();
                exec(&mut fs, FsOp::Write, "LOG.TXT", "hello");
                exec(&mut fs, FsOp::Write, "LOG.TXT", "again");
                let mut f = fs.open("LOG.TXT").unwrap();
                let mut buf = [0u8; 32];
                let n = f.read(&mut fs, &mut buf).unwrap();
                assert_eq!(&buf[..n], b"hello\nagain\n");
        }

        #[test]
        fn rm_mv_and_trunc_act_on_the_volume() {
                let mut fs = Fat::mount(fat16_superfloppy()).unwrap();
                exec(&mut fs, FsOp::Trunc, "HELLO.TXT", "10");
                assert_eq!(fs.open("HELLO.TXT").unwrap().size(), 10);
                exec(&mut fs, FsOp::Mv, "HELLO.TXT", "BYE.TXT");
                assert!(fs.open("HELLO.TXT").is_err());
                exec(&mut fs, FsOp::Rm, "BYE.TXT", "");
                assert!(fs.open("BYE.TXT").is_err());
        }

        #[test]
        fn the_pcm_level_skips_the_header_and_measures_the_samples() {
                let mut fs = Fat::mount(fat16_superfloppy()).unwrap();
                let mut f = fs.create("TAKE.WAV").unwrap();
                let mut data = std::vec![0xAAu8; 44];
                for s in [100i16, -300, 200, -100] {
                        data.extend_from_slice(&s.to_le_bytes());
                }
                f.write(&mut fs, &data).unwrap();
                let mut f = fs.open("TAKE.WAV").unwrap();
                //   mean square = (10000 + 90000 + 40000 + 10000) / 4 = 37500; floor sqrt = 193
                assert_eq!(pcm_level(&mut fs, &mut f).unwrap(), (300, 193, 4));
        }

        /// A store over a medium that fails every block: opening succeeds, mounting does not.
        struct FailingStore {
                resets: u32,
        }
        struct DeadDev;
        impl BlockDevice for DeadDev {
                fn block_count(&self) -> u32 {
                        4200
                }
                fn read_block(&mut self, _: u32, _: &mut [u8; 512]) -> Result<(), BlockError> {
                        Err(BlockError::Io)
                }
                fn write_block(&mut self, _: u32, _: &[u8; 512]) -> Result<(), BlockError> {
                        Err(BlockError::Io)
                }
        }
        impl Store for FailingStore {
                type Dev = DeadDev;
                fn open(&mut self) -> Option<DeadDev> {
                        Some(DeadDev)
                }
                fn reset(&mut self) {
                        self.resets += 1;
                }
        }

        #[test]
        fn an_io_failure_while_mounting_resets_the_store() {
                let mut store = FailingStore { resets: 0 };
                run(&mut store, FsOp::Info, "", "");
                assert_eq!(store.resets, 1);
        }

        #[test]
        fn a_store_over_a_good_volume_runs_the_command() {
                struct Mem(Option<MemDev>);
                impl Store for Mem {
                        type Dev = MemDev;
                        fn open(&mut self) -> Option<MemDev> {
                                self.0.take()
                        }
                        fn reset(&mut self) {
                                panic!("a good volume is never reset");
                        }
                }
                run(&mut Mem(Some(fat16_superfloppy())), FsOp::Ls, "", "");
        }
}
