# Storage — `light-sd`, `light-fs`

This document extracts the **mark 4 status quo**: the storage stack as `light_mk4` builds it today.

The stack is two portable crates joined by one HAL trait. `light-sd` brings up an SD/TF card in
SPI mode and presents it as blocks; `light-fs` reads and writes a FAT16/FAT32 filesystem over any
source of blocks. Neither knows the other exists: the only thing between them is
`light_core::hal::BlockDevice`, a 512-byte block interface that a card driver, a raw flash region or
a host test vector can all satisfy. A filesystem therefore mounts anything block-shaped, and the
portable code runs unchanged under `cargo test` against a byte vector.

---

## `light_core::hal::BlockDevice` — the seam

`BlockDevice` lives in the port interface (`light-core`'s `hal`), not in either storage crate,
because it is the contract *between* them and belongs to neither.

### Responsibility

Name block-addressed storage in 512-byte blocks — the boundary between a medium (an SD card, a raw
flash region, a test vector) and a filesystem.

### Public surface

```rust
pub enum BlockError { Io, Timeout, OutOfRange }

pub trait BlockDevice {
    fn block_count(&self) -> u32;
    fn read_block(&mut self, lba: u32, out: &mut [u8; 512]) -> Result<(), BlockError>;
    fn write_block(&mut self, lba: u32, data: &[u8; 512]) -> Result<(), BlockError>;
}

impl<T: BlockDevice + ?Sized> BlockDevice for &mut T { /* forwards */ }
```

### Behaviour and invariants

- The block size is fixed at 512 bytes, in the type: reads and writes take a `&[u8; 512]`, so a
  short or oversized transfer is not representable.
- Addressing is always by **logical block (LBA)**. A medium with different native addressing
  translates internally — the way the SD driver does for byte-addressed cards — so the filesystem
  never sees anything but a block index.
- `block_count` is the medium's size in blocks; a medium that is not yet ready answers `0`.
- The three errors are the whole failure vocabulary a filesystem must handle: the medium answered
  wrongly or not at all (`Io`), a deadline passed (`Timeout`), or the index is off the end
  (`OutOfRange`).

### Notable design decisions and constraints

- **The blanket impl on `&mut T` is load-bearing, not a convenience.** Because a mutable borrow of
  a block device is itself a block device, a filesystem can be mounted *over* a device something
  else owns, for the duration of one operation, without surrendering the device. This is what lets
  one block device back both a mounted volume and a concurrent writer (see *Sharing one block
  device*, below).

---

## `light-sd` — the SPI-mode block layer

`light-sd` is a single module, `spi_card`, re-exporting `SpiSd`, `CardInfo` and `SdError`. It
depends only on `light-core`.

### Responsibility

Identify an SD/TF card over an `SpiBus` and move 512-byte blocks to and from it. The crate stops at
"the card answers and blocks move" — it contains no filesystem knowledge whatsoever.

### Public surface

```rust
pub const INIT_HZ: u32 = 300_000;      // below the 400 kHz init ceiling
pub const DATA_HZ: u32 = 12_000_000;   // conservative; every card class does 12.5 MHz

pub enum SdError { NoCard, Unusable, Timeout, Response(u8) }

pub struct CardInfo { pub high_capacity: bool, pub blocks: u32 }

pub struct SpiSd<B: SpiBus, O: OutputPin> { /* bus, cs */ pub card: Option<CardInfo> }

impl<B: SpiBus, O: OutputPin> SpiSd<B, O> {
    pub fn new(bus: B, cs: O) -> Self;
    pub fn init(&mut self, clock: &mut dyn Clock) -> Result<CardInfo, SdError>;
    pub fn read_block(&mut self, lba: u32, out: &mut [u8; 512]) -> Result<(), SdError>;
    pub fn write_block(&mut self, lba: u32, data: &[u8; 512]) -> Result<(), SdError>;
}

impl<B: SpiBus, O: OutputPin> BlockDevice for SpiSd<B, O> { /* ... */ }
```

`SpiSd` is generic over the `SpiBus` and `OutputPin` traits of `light_core::hal`, so it holds its
own bus and chip-select and reaches hardware only through the port. `card` is public and readable:
`None` until a successful `init`, `Some(CardInfo)` after.

### Behaviour and invariants

- **Card init is the SD Simplified Physical Layer bring-up dance**, run by `init`:
  - The bus is dropped to `INIT_HZ` first — cards ignore faster clocks until initialised — and at
    least 80 clocks are sent with CS deasserted to wake the card into SPI mode.
  - **CMD0** (`GO_IDLE`) with its constant CRC `0x95`, retried a few times; no response at all is
    read as an empty slot and returned as `SdError::NoCard`, not a bus fault.
  - **CMD8** (`SEND_IF_COND`) distinguishes a v2 card (which echoes the `0x1AA` check pattern, CRC
    `0x87`) from a v1 card (which answers illegal-command).
  - **ACMD41** (`CMD55` + `SD_SEND_OP_COND`) is polled until the card leaves the idle state, with
    the HCS bit offered to v2 cards. The spec's full one-second budget is enforced against the
    injected `Clock`; overrun is `SdError::Timeout`.
  - **CMD58** reads the OCR; its CCS bit sets `high_capacity`. **SDHC/SDXC** cards are
    block-addressed; **SDSC** (v1, or standard-capacity v2) cards are byte-addressed, and for those
    **CMD16** fixes the block length at 512.
  - **CMD9** reads the CSD, from which `blocks` (the capacity in 512-byte blocks) is decoded — the
    CSD-v2 `C_SIZE` form for high-capacity cards, the CSD-v1 multiplier form for the rest.
  - On success the bus is raised to `DATA_HZ` and `card` is filled in.
- **Addressing is hidden from the caller.** `read_block`/`write_block` always take an LBA; the
  driver multiplies by 512 for a byte-addressed card and passes the LBA straight through for a
  high-capacity one.
- **A write blocks until the card commits.** `write_block` sends CMD24, the data token and payload,
  checks the card's data-response token (accepting the `xxx0_0101` "data accepted" form), then polls
  the busy line until the card releases it — the internal program can run hundreds of milliseconds
  on a worn card, and the call does not return until it finishes or the generous byte-time bound
  trips as `Timeout`.
- CRC is left off — SPI mode's default — except for the two init commands whose CRCs are the fixed
  constants above.
- **The `BlockDevice` impl is a thin adapter over an initialised card.** It maps `SdError` down to
  the three `BlockError` values (`Timeout` stays `Timeout`, everything else is `Io`). Before `init`
  succeeds the device answers `Io` to every transfer and reports a `block_count` of `0`.

### Notable design decisions and constraints

- **Init runs below 400 kHz by construction**; the two clock rates are public constants, not magic
  numbers, and the driver owns the switch to data rate itself.
- `SpiSd::read_block`/`write_block` are inherent methods returning the rich `SdError`, and the
  `BlockDevice` methods of the same name are the narrowed view; a board that wants the detailed
  error can use the card directly, a filesystem gets the trait.
- `new` documents that its `cs` pin must arrive deasserted (high); the driver frames every command
  transaction with CS and an extra trailing clock byte, which some cards need before they release
  MISO.

---

## `light-fs` — FAT16/FAT32 over any `BlockDevice`

`light-fs` is the framework's portable filesystem layer. The media dependency is exactly one trait,
so the same code mounts an SD card on a board and a byte vector in a host test. The one filesystem
implemented so far is FAT (module `fat`), because that is what memory cards carry and what every
desktop reads back.

### Responsibility

Mount, list, look up, read and write a FAT16 or FAT32 volume over a `BlockDevice`, in `no_std` with
no allocation.

### Public surface

```rust
pub enum FsError {
    Io(BlockError), NotFat, ExFat, Fat12, NotFound, NotADirectory, IsADirectory,
    BadChain, Exists, NoSpace, DirFull, BadName, NotEmpty,
}

pub struct VolumeInfo { pub fat32: bool, pub cluster_count: u32, pub bytes_per_cluster: u32 }

pub struct DirEntry { pub is_dir: bool, pub size: u32, /* names */ }
impl DirEntry { pub fn name(&self) -> &str; pub fn long_name(&self) -> Option<&str>; }

pub struct Fat<D: BlockDevice> { /* ... */ }
impl<D: BlockDevice> Fat<D> {
    pub fn mount(dev: D) -> Result<Self, FsError>;
    pub fn volume_info(&self) -> VolumeInfo;
    pub fn device(&mut self) -> &mut D;
    pub fn list_dir(&mut self, path: &str, f: impl FnMut(&DirEntry)) -> Result<(), FsError>;
    pub fn stat(&mut self, path: &str) -> Result<DirEntry, FsError>;
    pub fn open(&mut self, path: &str) -> Result<File, FsError>;
    pub fn create(&mut self, path: &str) -> Result<File, FsError>;
    pub fn append(&mut self, path: &str) -> Result<File, FsError>;
    pub fn remove(&mut self, path: &str) -> Result<(), FsError>;
    pub fn mkdir(&mut self, path: &str) -> Result<(), FsError>;
    pub fn rmdir(&mut self, path: &str) -> Result<(), FsError>;
    pub fn rename(&mut self, from: &str, to: &str) -> Result<(), FsError>;
}

pub struct File { /* a cursor; borrows nothing */ }
impl File {
    pub fn size(&self) -> u32;
    pub fn pos(&self) -> u32;
    pub fn read<D: BlockDevice>(&mut self, fs: &mut Fat<D>, out: &mut [u8]) -> Result<usize, FsError>;
    pub fn write<D: BlockDevice>(&mut self, fs: &mut Fat<D>, data: &[u8]) -> Result<usize, FsError>;
    pub fn truncate<D: BlockDevice>(&mut self, fs: &mut Fat<D>, len: u32) -> Result<(), FsError>;
    pub fn seek<D: BlockDevice>(&mut self, fs: &mut Fat<D>, pos: u32) -> Result<(), FsError>;
}
```

### Behaviour and invariants

- **One buffer, no allocation.** A `Fat<D>` owns exactly one 512-byte sector buffer and allocates
  nothing. Directory entries are decoded into value types (`DirEntry` is `Copy`), and the single
  buffer is a one-sector cache keyed by LBA.
- **A `File` is a cursor that borrows nothing.** It is a handful of words — position, current
  cluster, first cluster, and the location of its directory entry — and every operation (`read`,
  `write`, `seek`, `truncate`) takes the filesystem by `&mut`. So any number of open files
  interleave over the one buffer; the filesystem, not the file, holds the borrow.
- **Mounting** reads sector 0 and takes what it finds: a bare FAT volume (a "superfloppy"), or an
  MBR whose first FAT-typed partition is followed to its boot sector. The FAT **type is decided by
  cluster count** per the spec — `< 4085` is FAT12, `< 65525` is FAT16, otherwise FAT32 — never by
  the FAT string in the BPB, which lies on real cards. `mount` takes the device by value and owns
  it; `device()` lends it back for block-level work.
- **Directory reading** walks entries in on-disk order. Names are **8.3 with a bounded long-name
  read**: LFN chains up to 64 ASCII characters are decoded, checksum-verified against their 8.3
  entry, and usable in both listings and path lookup; a longer or non-ASCII name falls back to its
  short alias. Matching is case-insensitive, FAT's own rule. Orphaned LFN slots — an old editor's
  crash, a half-done rename — fail the checksum gate and are ignored.
- **File reading** fills the caller's buffer from the cursor, following the cluster chain; the count
  is short only at end of file, and a chain that ends before the recorded size does reads short
  rather than failing.
- **The write path is complete and write-through.** `create` places an 8.3 entry (claiming the
  first cluster lazily, on first write); `write` overwrites and extends, allocating clusters as
  needed; `append`, `truncate`, `remove`, `mkdir`, `rmdir` and `rename` (across directories, with a
  guard against moving a directory into its own subtree) round it out. Every mutated sector reaches
  the medium before the call returns, **every FAT copy is kept in step**, and the directory entry's
  size and first cluster are rewritten at the end of each `write`. The consequence is the crate's
  durability contract: **a pulled card loses at most the call in flight.**
- **On-disk cluster numbers are validated before they become addresses.** A cluster that enters from
  on-disk directory data passes a range check (`check_cluster`) before any address arithmetic, and
  what the FAT itself hands back is checked in `next_cluster`; a corrupt volume yields `BadChain`,
  not a wild read.

### Notable design decisions and constraints

- **Unsupported variants are named, not misparsed.** **exFAT** — the factory format of every SDXC
  card — is detected (by the `EXFAT` signature and the `0x07` partition type) and returned as
  `FsError::ExFat`; **FAT12** is returned as `FsError::Fat12`. Each says what the volume is and,
  implicitly, that reformatting as FAT32 is the fix, rather than parsing garbage.
- **Creation is 8.3-only.** Long names are read but never written: `create`, `mkdir` and `rename`
  require a legal uppercase 8.3 name (`BadName` otherwise). The read side carries long names; the
  write side does not author them.
- **Failure is designed to leak, never to corrupt.** A deletion writes the directory entry (the
  commit point) before freeing the chain; a broken link ends a free-walk quietly. The worst outcome
  of an interrupted mutation is a leaked cluster or an orphaned LFN slot — a checker's lint, the
  same lint every FAT implementation accumulates — not an unreadable volume.

---

## Sharing one block device between a filesystem and a concurrent writer

The two crates plus the `&mut T` blanket impl let an application keep **one** block device and lend
it, per operation, to whatever needs blocks — a mounted `Fat` volume for console `fs` commands and a
concurrent writer streaming to a file at the same time — without either owning the device.

An application that shares one device between a mounted volume and a concurrent writer is the worked
example. It holds the single block device — here an `SpiSd` — in a `&'static RefCell`, and wraps that
borrow in a small `BlockDevice` type whose every method takes the device by `borrow_mut()` for the
length of one block transfer and releases it:

```rust
struct SdRef(&'static RefCell<SpiSd<Spi1Bus, Output>>);

impl BlockDevice for SdRef {
    fn block_count(&self) -> u32 { self.0.borrow().card.map(|c| c.blocks).unwrap_or(0) }
    fn read_block(&mut self, lba, out)  -> Result<(), BlockError> { self.0.borrow_mut().read_block(lba, out).map_err(..) }
    fn write_block(&mut self, lba, data) -> Result<(), BlockError> { self.0.borrow_mut().write_block(lba, data).map_err(..) }
}
```

Because the borrow is taken and dropped inside each block operation, and no long-lived `&mut` to the
device is ever held, a mounted `Fat<SdRef>` and a separate writer state machine over the same
`SdRef` coexist on the one physical device. The application's storage abstraction readies the device
on first use, hands out a fresh borrowing device per mounted operation, and forgets it (clearing
`card` to force a re-init) if an operation fails mid-way. This is the block layer's single-owner
model and the filesystem's borrow-friendly mount meeting in an application: the device is owned once,
shared everywhere, one 512-byte operation at a time.
