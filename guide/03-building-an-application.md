# 03 — Building an application

This chapter builds a complete firmware from nothing: a board LED that blinks, and a console that
can turn it on and off. Then it shows how that single crate grows into the shape every real
application in the tree has — a portable application crate, a board-support crate, and a thin
per-board instantiation crate — and how a module is tested on the host.

The example targets a Raspberry Pi Pico 2 inside the framework repository, because that needs
the least ceremony. [Chapter 05](05-your-own-project.md) shows the same files living in a project
of your own.

Specification: [09-application-model.md](../documents/09-application-model.md).

## A minimal firmware

A firmware target is one directory under `module/` holding a `CMakeLists.txt` for the executable
and a `rust/` crate for everything else. Call it `blinky`:

```
module/blinky/
    CMakeLists.txt
    rust/
        Cargo.toml
        src/lib.rs
```

### The crate manifest

`module/blinky/rust/Cargo.toml`:

```toml
[package]
workspace = "../../../firmware"      # the FIRMWARE workspace, because this crate selects a port
name = "light_app_blinky"            # always light_app_<target>: see "Naming" below
version.workspace = true
edition.workspace = true
license.workspace = true

[lib]
crate-type = ["staticlib"]           # what Corrosion hands to CMake for the C shell to link

[dependencies]
light-core = { path = "../../../crates/light-core" }
light-rp2 = { path = "../../../crates/light-rp2", features = ["rp2350", "usb-console"] }
```

The port is chosen here and nowhere else: `rp2350` picks the chip, `usb-console` puts the USB
port in its device role with the console on it. Add the crate to the firmware workspace's member
list in `firmware/Cargo.toml` (`"../module/blinky/rust"`).

### The code

`module/blinky/rust/src/lib.rs`, complete:

```rust
#![no_std]

use light_core::cli::{Cli, Command, Outcome, Parsed, Words};
use light_core::{info, log, warn, Blinker, EventBus, LineReader, Mailbox, Module, Poll, Runtime, Subscription};
use light_rp2::gpio::Output;
use light_rp2::shell::{panic_report, ShellInfo, UART_BAUD, UART_RX, UART_TX};
use light_rp2::uart::Uart;
use light_rp2::{now_us, Breathe, SysClock};

const PIN_LED: usize = 25;

// --- the vocabulary -----------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        LedOn,
        LedOff,
        LedBlink,
        Stats,
}

static EVENTS: EventBus<AppEvent> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

// --- the LED ------------------------------------------------------------------------------

struct LedMod {
        led: Output,
        blinker: Blinker,
        blinking: bool,
        toggles: u32,
        events: Subscription,
}

impl Module for LedMod {
        fn name(&self) -> &'static str {
                "led"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        busy = true;
                        match ev {
                                AppEvent::LedOn => {
                                        self.blinking = false;
                                        self.led.set(true);
                                }
                                AppEvent::LedOff => {
                                        self.blinking = false;
                                        self.led.set(false);
                                }
                                AppEvent::LedBlink => self.blinking = true,
                                AppEvent::Stats => info!("led: {} toggles, blinking={}", self.toggles, self.blinking),
                        }
                }
                if self.blinking && self.blinker.poll(&mut self.led, &SysClock) {
                        self.toggles += 1;
                        busy = true;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.led.set(false);
        }
}

// --- the console --------------------------------------------------------------------------

fn parse_led(w: &mut Words) -> Parsed<AppEvent> {
        match w.next() {
                Some("on") => Parsed::Event(AppEvent::LedOn),
                Some("off") => Parsed::Event(AppEvent::LedOff),
                Some("blink") => Parsed::Event(AppEvent::LedBlink),
                _ => Parsed::Usage,
        }
}

fn parse_stats(_w: &mut Words) -> Parsed<AppEvent> {
        info!("uptime {} s; console: {} bytes dropped; bus: {} refused", now_us() / 1_000_000, CONSOLE_BYTES.dropped(), EVENTS.refused());
        Parsed::Event(AppEvent::Stats)
}

static COMMANDS: &[Command<AppEvent>] = &[
        Command { name: "led", usage: "led on|off|blink", parse: parse_led },
        Command { name: "stats", usage: "stats", parse: parse_stats },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

struct ConsoleMod {
        reader: LineReader<96>,
}

impl Module for ConsoleMod {
        fn name(&self) -> &'static str {
                "console"
        }
        fn poll(&mut self) -> Poll {
                let mut result = Poll::Idle;
                while let Some(b) = CONSOLE_BYTES.pop() {
                        if let Some(line) = self.reader.push(b) {
                                match CLI.dispatch(line.as_str()) {
                                        Outcome::Quiet => {}
                                        Outcome::Shutdown => return Poll::Shutdown,
                                        Outcome::Event(e) => {
                                                let _ = EVENTS.publish(e);
                                                result = Poll::Busy;
                                        }
                                        Outcome::Handled => result = Poll::Busy,
                                }
                        }
                }
                result
        }
}

// --- the shell entry points ---------------------------------------------------------------

/// Core 1: the port's console loop owns the whole core. We give it the sink for console
/// bytes and the UART (or `None` on a board whose UART pins are spoken for).
#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_main(info: &ShellInfo) -> ! {
        // SAFETY: core 1's one construction of the console UART
        let uart = unsafe { Uart::new(UART_TX, UART_RX, UART_BAUD, info.clk_peri_hz) };
        light_rp2::shell::core1_main(|b| { let _ = CONSOLE_BYTES.push(b); }, Some(uart))
}

/// Core 0: build the modules and run the loop forever.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        log::set_clock(now_us);
        // SAFETY: the one construction of this pin's driver
        let led = unsafe { Output::new(PIN_LED, false) };

        let mut led_mod = LedMod { led, blinker: Blinker::new(500_000), blinking: true, toggles: 0, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime = Runtime::new();
        rt.add(&mut led_mod).expect("capacity");
        rt.add(&mut console_mod).expect("capacity");
        rt.start().expect("start");
        info!("blinky running at {} Hz; type 'help' on the console", info.clk_sys_hz);

        let mut idle = Breathe;
        match rt.run(|| light_core::Idle::idle(&mut idle)) {
                Ok(()) => info!("runtime stopped cleanly"),
                Err(e) => warn!("runtime stopped with {e:?}"),
        }
        loop {
                core::hint::spin_loop();
        }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        panic_report(info)
}
```

Read it top to bottom and it is the whole of [Chapter 02](02-how-an-application-runs.md): a
vocabulary, a bus, two modules, a command table, and the three entry points. The `unsafe` on the
peripheral constructors is the port's construct-once contract — nothing else may touch that pin
while the `Output` lives — and the comment says why this call honours it.

### The executable

`module/blinky/CMakeLists.txt`:

```cmake
add_executable(blinky
        ../light_shell/src/main.c            # the C shell, shared by every RP2 firmware
)

target_link_libraries(blinky PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_blinky                     # the staticlib, imported by the root CMakeLists
)

light_shell_configure(blinky)                # the shell's per-executable settings, UF2 output
```

Register it in the root `CMakeLists.txt`: add `light_app_blinky` to the `CRATES` list of the
`corrosion_import_crate` call that imports `firmware/Cargo.toml`, and `add_subdirectory(module/blinky)`
in the branch for the board it targets. Then give the script layer a target in
`scripts/project.config.ps1`:

```powershell
Targets = @{
        'blinky' = @{ Preset = 'conf-light_mk5-pico2-debug'; Flash = 'swd' }
        # …
}
```

Now:

```
scripts/build.ps1 -Target blinky
scripts/flash.ps1 -Target blinky
scripts/console.ps1 -Send "help","led off","stats"
```

### Naming

The CMake executable and the Corrosion-imported crate share one CMake target namespace, so they
cannot share a name. The convention is uniform: the executable is `<name>`, its crate is
`light_app_<name>`, and the executable's directory is `module/<name>/` — which is also where
`flash.ps1` looks for `<name>.uf2`.

## Growing it: the three-crate shape

The firmware above is one crate because it has one board. The moment an application runs on a
second board — or you want to test its logic on the host — it splits into three layers with one
direction of dependency:

1. **The portable application crate** (`crates/light_app_<app>`): the event enum, the modules'
   behaviour, the console commands, the rendering machinery. `no_std`, depends only on framework
   crates, **never on a port**, so it has no pin in it and runs under `cargo test`. It is a member
   of the portable workspace (the root `Cargo.toml`).
2. **The board-support crate** (`crates/light-board-<board>`): what is specific to a tangible
   board but independent of which application runs on it — the pin map, the taken-once
   `Peripherals` with its `board::take(&Clocks)`, the touch coordinate map, the IMU axis map, the
   board's power mechanism. It must not depend on an application crate; that independence is what
   lets several applications share one board.
3. **The per-board instantiation crate** (`crates/light_app_<app>_<board>` or
   `module/<target>/rust`): the staticlib the firmware links. It constructs the concrete
   peripherals, embeds the compiled assets, wires the portable modules to the board's drivers,
   and provides the three entry points. Tens of lines. It and the board crate are members of the
   firmware workspace.

The widget demo is the in-tree example: `crates/light_app_ui_demo` (portable),
`crates/light-board-touch169` (one board), and `crates/light_app_ui_demo_touch169` (the two joined,
linked by `module/ui_demo_touch169`). The same portable crate is instantiated on five boards.

### Making a module portable

A module in the portable crate cannot name the concrete bus, pin or clock. It is generic over the
`hal` traits and holds the bus as the capacity-erased trait object:

```rust
use light_core::hal::{Clock, OutputPin};
use light_core::{Bus, Module, Poll, Subscription};

pub struct LedMod<P: OutputPin, C: Clock> {
        led: P,
        clock: C,
        bus: &'static dyn Bus<AppEvent>,
        events: Subscription,
}

impl<P: OutputPin, C: Clock> LedMod<P, C> {
        pub fn new(led: P, clock: C, bus: &'static dyn Bus<AppEvent>) -> Self {
                Self { led, clock, bus, events: bus.subscribe().expect("subscriber slot") }
        }
}
```

The instantiation crate hands it `Output`, `SysClock` and `&EVENTS`; a host test hands it a
recording pin, a hand-advanced clock and a bus it created.

### The board extension

A portable event enum leaves room for the board:

```rust
#[derive(Clone, Copy, Debug)]
pub enum DemoEvent<X: Copy> {
        Toggle(u8),
        Command(Command),
        Ext(X),              // the board's own affairs, which this crate never interprets
}
```

The touch board's instantiation crate defines `enum Ext { Tone { hz: u16, ms: u16 }, Beep(u16), … }`
and `type AppEvent = DemoEvent<Ext>`; its audio module matches on `AppEvent::Ext(Ext::Tone { .. })`
while the portable display module ignores the variant. Both ride the same bus.

### Reusing framework modules

A runtime module that is the same for every application that has the hardware is a framework
crate, not a copy in yours: `TouchMod` and `ImuMod` in `light-input`, `PowerMod` in
`light-power-manager`, `RtcMod` in `light-rtc`, `Recorder` in `light-recorder`. Each is generic
over its drivers, a clock and your event type. Where it needs to read one of your events, you pass
a **recognizer function** rather than implementing a trait on your type:

```rust
fn power_backlight(e: &AppEvent) -> Option<u16> {
        if let DemoEvent::Command(Command::Backlight(level)) = e { Some(*level) } else { None }
}
fn power_is_stats(e: &AppEvent) -> bool {
        matches!(e, DemoEvent::Command(Command::Stats))
}

let mut power_mod = PowerMod::new(Touch169Power { backlight: p.backlight }, SysClock, &EVENTS, power_backlight, power_is_stats, |_| None);
let mut imu_mod = ImuMod::new(imu, &EVENTS, SysClock, IMU_AXIS_MAP);
let touch_mod = TouchMod::new(touch, Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), &EVENTS, SysClock, demo::touch_reads_held);
```

So an application wires drivers, mechanisms and recognizer functions; what stays yours is logic
that is genuinely yours.

### The console in a portable crate

The console module and the byte pump move into the portable crate too, so every board's
instantiation is the same two lines:

```rust
// portable crate
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();
pub fn push_console_byte(b: u8) { let _ = CONSOLE_BYTES.push(b); }
pub struct ConsoleMod<X: Copy + 'static> { /* a LineReader, &'static Cli<DemoEvent<X>>, &'static dyn Bus<…> */ }

// instantiation crate
pub extern "C" fn light_app_core1_main(info: &ShellInfo) -> ! {
        let uart = unsafe { Uart::new(UART_TX, UART_RX, UART_BAUD, info.clk_peri_hz) };
        light_rp2::shell::core1_main(demo::push_console_byte, Some(uart))
}
let mut console_mod = demo::ConsoleMod::new(&CLI, &EVENTS);
```

The portable crate can export its command table as a macro that a board extends with its own
rows (`demo_commands![Ext; CliCommand { name: "tone", … }]`), so `help` lists both.

## Testing a module on the host

Because a portable module reaches hardware only through traits, a test supplies the traits:

```rust
#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use core::cell::Cell;
        use light_core::hal::{Clock, OutputPin};
        use std::boxed::Box;

        struct MockClock(Cell<u64>);
        impl Clock for MockClock {
                fn now_us(&self) -> u64 { self.0.get() }
        }

        #[derive(Default)]
        struct RecordingPin(Cell<bool>);
        impl OutputPin for &RecordingPin {
                fn set(&mut self, high: bool) { self.0.set(high); }
        }

        #[test]
        fn led_off_turns_the_pin_off_and_stops_blinking() {
                // a bus per test: `Box::leak` gives the 'static the module wants
                let bus: &'static EventBus<AppEvent> = Box::leak(Box::new(EventBus::new()));
                let pin = RecordingPin::default();
                let clock = MockClock(Cell::new(0));
                let mut led = LedMod::new(&pin, clock, bus);

                bus.publish(AppEvent::LedOff).unwrap();
                assert_eq!(led.poll(), Poll::Busy);
                assert!(!pin.0.get());
        }
}
```

Two conventions make these tests cheap to write:

- **Drive the module through the bus, not its fields.** Publish the event a console line or a
  tap would have published, poll once, and assert on what the mocks recorded. The same test then
  covers every input path, because they all end as the same event.
- **Test a command table without a bus.** `CLI.dispatch("led off")` returns
  `Outcome::Event(AppEvent::LedOff)`; bad arguments return `Outcome::Handled` after printing the
  usage line. A table's grammar is a pure function of a string.

A static `EventBus` in a test binary is shared by every test in that binary, and a subscription
slot is only freed by `unsubscribe`. A bus per test (leaked, as above) avoids both the sharing and
the slot leak.

`scripts/test.ps1` runs every portable crate's tests; `cargo test -p light_app_demo` runs one
crate's from the repository root. [Chapter 06](06-running-without-hardware.md) covers the host
side in more depth, including the simulated board that runs the whole application at once.
