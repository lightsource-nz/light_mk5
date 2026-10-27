# 02 — How an application runs

This chapter is the mental model: what the C shell does, what Rust does, what a module is, how
modules talk to each other, and how text gets in and out. Everything here is in `light-core`, the
crate every other crate depends on.

Specification: [00-overview.md](../documents/00-overview.md) (the execution model) and
[01-core-runtime.md](../documents/01-core-runtime.md).

## Rust above a C shell

A firmware image is two things linked together: a small **C shell** that the platform SDK's build
owns, and a Rust **staticlib** that is everything else. On the RP2 chips the shell is pico-sdk's
`crt0`, `boot2`, linker script and multicore launch plus one `main.c`; on the STM32 boards a bare
CMSIS startup file and a clock tree stand in for it. The shell has no stdio and no USB stack. It
brings the platform up and hands each core to Rust through a handful of functions:

- **`light_app_main(&ShellInfo) -> !`** — called once on the application core with the clock
  rates the shell configured. Your code constructs the peripherals, builds the modules, and runs
  the runtime loop forever. It never returns.
- **`light_app_core1_main(&ShellInfo) -> !`** — on a two-core chip, called once on the second
  core. The port's shell module provides the whole body (`light_rp2::shell::core1_main`): it brings
  up the USB console and the UART, drains the log queue to them, and pumps console input into the
  application. Your crate's version of this function is three lines.
- A `#[panic_handler]` that calls the port's `panic_report`, which prints the message through the
  console core and leaves a device-role board in BOOTSEL so it can still be reflashed.

Rust calls back into the shell only for what the SDK alone can do: entering the bootloader,
reading the BOOTSEL button, and the boot-ROM routines behind a firmware update.

**The application always runs on one core**, cooperatively. Where the chip has a second core the
framework uses it for the console and the USB device stack, so a busy render loop can never stall
the console and the console can never stall the loop. Where it does not, the same housekeeping
runs inline in the loop. Your modules and your event bus are the same either way.

## Modules and the runtime

An application is a set of **modules** driven by a **runtime**. A module owns some state and
usually some hardware, and implements one trait:

```rust
pub trait Module {
        fn name(&self) -> &'static str;                 // unique in the application
        fn deps(&self) -> &'static [&'static str] { &[] }  // names that must load first
        fn load(&mut self) -> Result<(), ()> { Ok(()) } // once, at start
        fn unload(&mut self) {}                         // once, at shutdown, reverse order
        fn poll(&mut self) -> Poll { Poll::Idle }       // every pass
}

pub enum Poll { Idle, Busy, Shutdown, Error }
```

`poll` is where the work happens, and what it returns is what the runtime needs to know:
**`Busy`** means "I did something, come back promptly", **`Idle`** means "nothing to do".
**`Shutdown`** asks the runtime to stop; it still unloads every module, in reverse load order, so
a `quit` on the console leaves the hardware tidy. **`Error`** is for something the module cannot
recover from; the runtime stops with the module's name.

The runtime holds the modules, orders them by their declared dependencies, and polls them round
robin:

```rust
let mut rt: Runtime = Runtime::new();          // capacity 8 by default; Runtime<16> for more
rt.add(&mut led_mod)?;                         // Err(Error::Capacity) when full, never a silent drop
rt.add(&mut console_mod)?;
rt.start()?;                                   // resolves deps, loads each; a missing dep is named
let result = rt.run(|| idle.idle());           // returns only on Shutdown or Error
```

The closure passed to `run` is the **idle hook**, called between passes in which no module was
busy: on a real chip it sleeps the core until the next interrupt (`light_rp2::Breathe`), on the
host it yields the thread. That is the whole concurrency model: no preemption, no async
executor, no hidden allocation. The rules that follow from it:

- **Never block in `poll`.** The loop period *is* the latency: nothing is answered until the pass
  that answers it comes round, and the worst case is the longest pass. A blocking delay belongs
  in `load`, where init sequences are allowed to wait. A long job is chunked across passes, or
  handed to hardware (DMA, an interrupt-fed ring) and polled for completion.
- **Report `Busy` honestly.** A module that returns `Busy` when it did nothing keeps the core
  awake; one that returns `Idle` with work pending waits for someone else to wake the loop.
- **Registration is explicit.** You construct each module as a value and add it. There is no
  registry, no linker-section walk, and a dependency nobody registered is an error at start-up
  with its name in it.

The `loop` console built-in prints the pass rate; `loop detail` also attributes time to each
module, which is how you find the one adding half a millisecond to everything else's response.

## The event bus

Modules never call each other and never share state. They communicate through one typed
**broadcast bus**, parameterised by the application's own event type:

```rust
#[derive(Clone, Copy, Debug)]
enum AppEvent {
        LedOn,
        LedOff,
        Key(u8, bool),
        Stats,
}

static EVENTS: EventBus<AppEvent> = EventBus::new();   // a board static, sized by defaults
```

The event type is a single `Copy` enum carrying every kind of thing that happens: a key press, a
console command, a status change, an elapsed second. Each module **subscribes once at start-up**
(a `Subscription` is one cursor) and **polls its subscription every pass**:

```rust
struct LedMod { led: Output, events: Subscription }

impl Module for LedMod {
        fn name(&self) -> &'static str { "led" }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        busy = true;
                        match ev {
                                AppEvent::LedOn => self.led.set(true),
                                AppEvent::LedOff => self.led.set(false),
                                AppEvent::Stats => info!("led: fine"),
                                _ => {}
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}
```

and anyone publishes:

```rust
let _ = EVENTS.publish(AppEvent::Key(0, true));
```

Every subscriber sees every event, in order. The bus is bounded: it holds `N` events (16 by
default) for the slowest subscriber, and when it is full `publish` **refuses** the event — returns
it in the `Err` and counts the refusal — rather than blocking or overwriting something unseen.
With no subscribers at all an event is dropped silently. `EVENTS.refused()` and
`EVENTS.backlog()` are the two numbers worth printing from `stats`.

Capacities have defaults so that a board names them only when it must: `EventBus<E>` is
`EventBus<E, 16, 8>` — sixteen events deep, eight subscriber slots, one per module a default
`Runtime` can hold. A board with more modules, or a module that takes two subscriptions, writes
`EventBus<E, 16, 12>` and `Runtime<12>` in the one place its statics live. Over-subscription
shows up at start-up (`subscribe` returns `None`), never as a mid-run panic.

Polling an empty bus is deliberately cheap — a comparison of two counters with no lock — because
every module does it tens of thousands of times a second.

**The `Ext` pattern.** A portable application crate defines its event enum with one variant it
does not interpret, `Ext(X)`, generic over a type the board supplies. The board's own concerns —
a buzzer, a battery, a real-time clock — ride the same bus as `Ext(…)` events without the
portable crate naming them. Framework modules that are generic over the app's event type hold
`&'static dyn Bus<E>` (the capacity-erased trait) rather than the concrete `EventBus`.

## Logging

```rust
use light_core::{error, warn, info, debug, trace};
info!("oled up: {}x{} glass", width, height);
```

The macros push a **record** onto a bounded queue (32 deep, 96 characters of text each) and
return at once. Whoever runs the framework's housekeeping — the second core, or the loop itself
on a single-core chip — drains the queue to the console. When the queue is full the record is
**dropped and counted**, and the next drain says `dropped N log records`. A full log costs a lost
line, never a stalled loop. Records above the current level are discarded before anything is
formatted, so a `debug!` in a hot path costs a load and a compare.

`light_core::log::now_us()` is the time source for portable code: microseconds since boot, from
the clock the board installed with `log::set_clock` at the top of `light_app_main`. It is the same
clock the log lines are stamped with.

## The console

Console bytes arrive on whichever core owns the transport and are handed to the application
through a **mailbox**, a bounded single-producer/single-consumer queue that never blocks:

```rust
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

pub fn push_console_byte(b: u8) {      // what the console core calls, once per byte
        let _ = CONSOLE_BYTES.push(b);
}
```

A console module pops the bytes each pass, assembles lines with a `LineReader`, and hands each
line to a **`Cli`**: a static table of the application's commands over a shared grammar that
already knows `help`, `loglevel`, `loop` and `quit`:

```rust
use light_core::cli::{Cli, Command, Outcome, Parsed, Words};

fn parse_led(w: &mut Words) -> Parsed<AppEvent> {
        match w.next() {
                Some("on") => Parsed::Event(AppEvent::LedOn),
                Some("off") => Parsed::Event(AppEvent::LedOff),
                _ => Parsed::Usage,                      // the Cli prints the usage line
        }
}

static COMMANDS: &[Command<AppEvent>] = &[
        Command { name: "led", usage: "led on|off", parse: parse_led },
        Command { name: "stats", usage: "stats", parse: |_| Parsed::Event(AppEvent::Stats) },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);
```

`CLI.dispatch(line)` echoes the line, answers the built-ins itself, and otherwise looks the first
word up in the table and calls its parse function with the remaining words. The result is an
`Outcome`: `Quiet` (an empty line), `Handled` (answered already), `Event(e)` (publish this), or
`Shutdown` (`quit`). The caller publishes the event, which is the point of the design: **a console
line, a tapped button and a test's direct injection are the same record on the same bus**, and a
module cannot tell them apart.

Words are separated by spaces, except that a run inside double quotes is one word (`join "My
Network" secret`). There is no escape character.

## Memory: statics, not stacks

The application core's stack is small — a few kilobytes — and an overflow is silent. Anything
large (a frame buffer, the widget arena, the frame layer, a module with big buffers) lives in
`.bss` as a static and is taken once as `&'static mut`:

```rust
use light_core::{ConstStaticCell, StaticCell};

static FRAME: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
let frame: &'static mut [u8] = FRAME.take();                 // built in place; a second take panics

static OLED_MOD: StaticCell<OledMod> = StaticCell::new();
let oled_mod = OLED_MOD.init(OledMod { /* … */ });          // for a value built at runtime
```

`ConstStaticCell` is for values with a `const` constructor (built in place, nothing ever on the
stack); `StaticCell` is for values you construct at runtime and move in. There is no `static mut`
anywhere in the framework, and the discipline is worth keeping in your own code: a large value on
the stack can overflow into another core's stack region and kill the console without a word.

## The port interface

Portable code reaches hardware only through the traits in `light_core::hal` — `Clock`, `Idle`,
`OutputPin`, `InputPin`, `I2cBus`, `SpiBus`, `SpiDisplayBus`, `QspiDisplayBus`, `BlockDevice`,
`AudioStream`, and the rest — which a **port** crate implements for one chip (`light-rp2`,
`light-stm32h7`, `light-stm32f4`). A port stops at the chip: its constructors take pin numbers and
clock rates, and know nothing about what is soldered to a pin. That is what lets a framework
crate run under `cargo test` on the host against mocks, and it is the discipline that keeps your
own application portable: name a pin only in the one crate that wires the board.
