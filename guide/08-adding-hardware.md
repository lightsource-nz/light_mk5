# 08 — Adding hardware

The framework is extended by implementing a **seam** — a trait that names what a kind of hardware
does — not by special-casing above it. This chapter says, for each kind of thing you might add,
which seam it plugs into, where the code lives, and what the shipped reference implementations
are. The specification carries the full contracts.

## A panel

A panel is a `light_display::DisplayDriver` over one of the display-bus traits
(`SpiDisplayBus`, `QspiDisplayBus`), living in `light-display` beside the others. It answers three
questions and the display core owns everything else — the in-flight state, the chunk index, the
per-chunk deadline, the spin-or-yield budget:

```rust
pub trait DisplayDriver {
        fn init(&mut self, clock: &mut dyn Clock, width: u16, height: u16);
        fn chunk_count(&self, region: &Region) -> u16;           // how many pushes this update takes
        fn chunks_per_poll(&self, region: &Region) -> u16;       // 0 = yield while a chunk is in flight
        fn kick(&mut self, frame: &Frame<'_>, region: &Region, index: u16);   // start chunk N; must not wait
        fn chunk_complete(&mut self) -> bool;                    // has the one in flight landed?
        fn chunk_timeout_ms(&self) -> u32;
}
```

A push panel
with its own frame memory chunks by row or column and reports a DMA transfer's completion; a
scanned panel reports zero chunks. Part-specific facts — a GDDRAM offset, an init table, a quirk
such as ignoring its own window — belong inside the driver, with a setter where a board must
state them (`set_offset`, a `MADCTL` byte). The driver sees only its bus; it never names a chip.

Reference drivers: ST7789, ST7796, ST7735 and SH1107 (SPI), AXS15231B (QSPI), `Scanout` (RGB/DPI
through a hardware engine). Specification: [02-graphics-and-ui.md](../documents/02-graphics-and-ui.md).

## A touch controller

A touch controller is a `light_input::touch::TouchController` producing the one canonical
`TouchSample`, so gesture tracking and the `BoardEvent` contract are controller-independent:
`probe` (is the part there), `poll(now_ms)` (the next sample, if any), `diagnostics`, and an
optional `reset`. It is generic over `I2cBus` and the pins it needs, lives in `light-input`'s
`drivers`, and carries the shared poll-and-recovery discipline — a controller that naps on a
shared bus must not stall the loop. The trait requires `HardwareGestures` as well, which is how a
controller declares whether it classifies gestures itself or leaves that to the software tracker.

Reference drivers: CST816T, CST328, GT911, AXS15231B. Specification:
[03-input.md](../documents/03-input.md).

## An I2C part

A codec, a clock, a sensor, a power-management part: each is a plain struct generic over
`light_core::hal::I2cBus`, in whichever crate owns its subject (`light-audio`, `light-rtc`,
`light-power`, `light-input`, `light-camera`). Several drivers share one bus through a
`&RefCell<Bus>`, which implements `I2cBus` too; every transaction is one borrow.

```rust
pub struct MyPart<B: I2cBus> { bus: B }
impl<B: I2cBus> MyPart<B> {
        pub fn probe(&mut self) -> Option<u8> {
                let mut id = [0u8; 1];
                self.bus.read_register(ADDR, REG_ID, &mut id).ok()?;
                Some(id[0])
        }
}
```

Two rules the shipped drivers learned the hard way: write a register with
`write_register_byte` (one transaction, no repeated START — some parts store nothing when the
pair is split), and check the part's identity register before anything else, because a register
map taken from someone else's driver is a hypothesis until the part answers.

## A board

A board is a **board-support crate**, `crates/light-board-<board>`, holding only what is genuinely
board-specific: the pin and constant set, the `Peripherals` struct and its taken-once
`take(&Clocks) -> Option<Peripherals>`, the touch coordinate map and IMU axis map, backlight
inversion, DMA channel assignments, and the board's `PowerMechanism`. Nothing board-agnostic goes
in it: the shell glue is the port's `shell` module, the generic input modules are
`light-input`'s, the power policy is `light-power-manager`'s.

```rust
pub fn take(clocks: &Clocks) -> Option<Peripherals> {
        if TAKEN.swap(true, Ordering::AcqRel) {
                return None;
        }
        // SAFETY: the flag above makes this the one construction of each peripheral
        unsafe {
                Some(Peripherals {
                        led: Output::new(PIN_LED, false),
                        key0: Input::new_pull_up(PIN_KEY0),
                        oled_bus: Spi1Display::new(clocks.peri_hz, PIN_SCK, PIN_MOSI, PIN_CS, PIN_DC, Some(PIN_RESET), SPI_HZ, DMA_CH),
                })
        }
}
```

A board the platform SDK does not know also needs a board header (`boards/<name>.h`) carrying only
what the SDK consumes — chip variant, flash size, default UART and LED — and a line in the
project's `CMakeLists.txt` naming it ([Chapter 05](05-your-own-project.md)). The pin map stays in
the crate.

DMA channels are taken from the top of the chip's range, where the SDK's own allocator (counting
up from zero) will not reach; the top is a chip fact (eleven on the RP2040, fifteen on the
RP2350). Specification: [09-application-model.md](../documents/09-application-model.md).

## A chip

A chip is a **port**: a crate implementing the `light_core::hal` traits its boards need —
`Clock`, `Idle`, `OutputPin`, `InputPin`, `I2cBus`, `SpiBus`, `SpiDisplayBus`, and whichever of
the others apply — plus one `critical-section` implementation, in terms of that chip's
registers. It stops at the chip: constructors take pin numbers and clock rates and assume no
pinout. It is a member of the firmware workspace and is never host-tested.

Beside it goes the **C shell** for the platform (or a configuration of an existing one): what
brings the chip up and hands each core to Rust through `light_app_main` and, on a two-core part,
`light_app_core1_main`; and the Rust **shell glue** — the console loop over the port's USB device
and UART, the panic relay, `usb_mounted` — which is shared by every board on that chip and lives
in the port's `shell` module (RP2) or a shared crate (`light-shell-cmsis` for the STM32 ports).

A port is a per-firmware choice: `critical-section` permits exactly one implementation in a
binary, so two ports cannot be linked together, and that is what the two-workspace layout is
built around. The three reference ports — `light-rp2` (RP2040 and RP2350, both ISAs),
`light-stm32h7`, `light-stm32f4` — are the models. Specification:
[07-ports-and-shell.md](../documents/07-ports-and-shell.md).

## A radio

A radio part with no firmware of its own is driven over `light_core::hal::RadioBus`, a
half-duplex single-wire word bus the port supplies from programmable I/O. Its driver is a crate of
its own — target-only but chip-independent, because it belongs to the part — and its firmware
image is an asset in the data partition, not part of the firmware image. The contract above it
(credentials, networks heard, join outcomes) is `light-wireless`. Specification:
[12-wireless.md](../documents/12-wireless.md).

## A storage medium

Anything that can read and write 512-byte blocks is a `light_core::hal::BlockDevice`, and
`light-fs` mounts a FAT16/FAT32 volume over it. Removable media that must be brought up per
insertion implements `Store` (`open` hands a device per mounted operation, `reset` forgets a
medium that failed mid-way). `light-sd` is the reference: an SD card in SPI mode over `SpiBus`.
Specification: [05-storage.md](../documents/05-storage.md).

## Where to record what you learn

Every reference driver in the tree carries the facts it was born from — a timing that had to be
measured, a register that behaves otherwise than documented — as comments beside the code that
depends on them. When a part teaches you something, write it there, in terms a stranger with the
same part can use.
