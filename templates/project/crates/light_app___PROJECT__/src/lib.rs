//! __PROJECT__: the application, with no hardware in it.
//!
//! Everything this crate does reaches the world through the `light_core::hal` traits, so it runs
//! unchanged on a board and under `cargo test` on the host. The board is chosen elsewhere: each
//! firmware target under `module/` constructs the real pins and clock and hands them to [`run`].
//!
//! What it does, as a starting point to replace with your own: an LED that blinks, a button that
//! starts and stops the blinking, and a console that can do both. Three modules, one event bus.

#![no_std]

use light_core::button::{Button, ButtonEvent};
use light_core::cli::{Cli, Command, Outcome, Parsed, Words};
use light_core::hal::{Clock, InputPin, OutputPin};
use light_core::{info, log, warn, Blinker, Bus, EventBus, Idle, LineReader, Mailbox, Module, Poll, Runtime, Subscription};

// --- the vocabulary -------------------------------------------------------------------------

/// Everything that happens in the application. A console line, a button press and a test's
/// direct injection all become one of these on the bus, and no module can tell which it was.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AppEvent {
        LedOn,
        LedOff,
        LedBlink,
        /// The button was pressed (debounced).
        Pressed,
        /// Every module reports its counters.
        Stats,
}

/// The application's bus. Its capacities are the framework's defaults: sixteen events deep, a
/// subscriber slot for each module a default runtime can hold.
pub static EVENTS: EventBus<AppEvent> = EventBus::new();

/// Console bytes, handed over from the core that owns the console transports.
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

/// The sink the console core feeds, one byte at a time. Never blocks: a full mailbox drops the
/// byte, and the line it belonged to fails to parse and says so.
pub fn push_console_byte(b: u8) {
        let _ = CONSOLE_BYTES.push(b);
}

// --- the LED --------------------------------------------------------------------------------

/// How long the LED stays in each state while blinking.
pub const BLINK_PERIOD_US: u64 = 500_000;

/// The LED: blinking to start with, held on or off from the console, and the button starts and
/// stops the blinking.
pub struct LedMod<P: OutputPin, C: Clock> {
        led: P,
        clock: C,
        blinker: Blinker,
        blinking: bool,
        toggles: u32,
        bus: &'static dyn Bus<AppEvent>,
        events: Subscription,
}

impl<P: OutputPin, C: Clock> LedMod<P, C> {
        pub fn new(led: P, clock: C, bus: &'static dyn Bus<AppEvent>) -> Self {
                let events = bus.subscribe().expect("a subscriber slot for the led");
                Self { led, clock, blinker: Blinker::new(BLINK_PERIOD_US), blinking: true, toggles: 0, bus, events }
        }

        pub fn blinking(&self) -> bool {
                self.blinking
        }

        pub fn toggles(&self) -> u32 {
                self.toggles
        }

        fn hold(&mut self, on: bool) {
                self.blinking = false;
                self.led.set(on);
        }
}

impl<P: OutputPin, C: Clock> Module for LedMod<P, C> {
        fn name(&self) -> &'static str {
                "led"
        }

        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = self.bus.poll(&self.events) {
                        busy = true;
                        match ev {
                                AppEvent::LedOn => self.hold(true),
                                AppEvent::LedOff => self.hold(false),
                                AppEvent::LedBlink => self.blinking = true,
                                AppEvent::Pressed => {
                                        if self.blinking {
                                                self.hold(false);
                                        } else {
                                                self.blinking = true;
                                        }
                                }
                                AppEvent::Stats => info!("led: {} toggles, blinking={}", self.toggles, self.blinking),
                        }
                }
                if self.blinking && self.blinker.poll(&mut self.led, &self.clock) {
                        self.toggles += 1;
                        busy = true;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }

        fn unload(&mut self) {
                self.led.set(false);
        }
}

// --- the button -----------------------------------------------------------------------------

/// How often the button is sampled. Well inside the debounce window, and modest on purpose: on
/// some boards reading the button is not free (it can mean a short interrupts-off window).
pub const BUTTON_SAMPLE_MS: u32 = 10;

/// A debounced button onto the bus. The pin reads low while the button is pressed.
pub struct ButtonMod<P: InputPin, C: Clock> {
        button: Button<P>,
        clock: C,
        next_ms: u32,
        presses: u32,
        bus: &'static dyn Bus<AppEvent>,
        events: Subscription,
}

impl<P: InputPin, C: Clock> ButtonMod<P, C> {
        pub fn new(pin: P, clock: C, bus: &'static dyn Bus<AppEvent>) -> Self {
                let events = bus.subscribe().expect("a subscriber slot for the button");
                Self { button: Button::new(pin, true), clock, next_ms: 0, presses: 0, bus, events }
        }

        pub fn presses(&self) -> u32 {
                self.presses
        }
}

impl<P: InputPin, C: Clock> Module for ButtonMod<P, C> {
        fn name(&self) -> &'static str {
                "button"
        }

        fn poll(&mut self) -> Poll {
                while let Some(ev) = self.bus.poll(&self.events) {
                        if ev == AppEvent::Stats {
                                info!("button: {} presses", self.presses);
                        }
                }
                let now_ms = (self.clock.now_us() / 1000) as u32;
                //   a wrapping comparison, so the schedule survives the millisecond count wrapping
                if (now_ms.wrapping_sub(self.next_ms) as i32) < 0 {
                        return Poll::Idle;
                }
                self.next_ms = now_ms.wrapping_add(BUTTON_SAMPLE_MS);
                if self.button.poll(now_ms) == Some(ButtonEvent::Press) {
                        self.presses += 1;
                        if let Err(e) = self.bus.publish(AppEvent::Pressed) {
                                warn!("event bus full; dropped {e:?}");
                        }
                        return Poll::Busy;
                }
                Poll::Idle
        }
}

// --- the console ----------------------------------------------------------------------------

fn parse_led(w: &mut Words) -> Parsed<AppEvent> {
        match w.next() {
                Some("on") => Parsed::Event(AppEvent::LedOn),
                Some("off") => Parsed::Event(AppEvent::LedOff),
                Some("blink") => Parsed::Event(AppEvent::LedBlink),
                _ => Parsed::Usage,
        }
}

fn parse_stats(_w: &mut Words) -> Parsed<AppEvent> {
        Parsed::Event(AppEvent::Stats)
}

/// The application's own commands. `help`, `loglevel`, `loop` and `quit` are built in.
pub static COMMANDS: &[Command<AppEvent>] = &[
        Command { name: "led", usage: "led on|off|blink", parse: parse_led },
        Command { name: "stats", usage: "stats", parse: parse_stats },
];
pub static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

/// Console lines in, events out.
pub struct ConsoleMod {
        reader: LineReader<96>,
        bus: &'static dyn Bus<AppEvent>,
}

impl ConsoleMod {
        pub fn new(bus: &'static dyn Bus<AppEvent>) -> Self {
                Self { reader: LineReader::new(), bus }
        }

        fn dispatch(&mut self, line: &str) -> Poll {
                match CLI.dispatch(line) {
                        Outcome::Quiet => Poll::Idle,
                        Outcome::Shutdown => Poll::Shutdown,
                        Outcome::Handled => Poll::Busy,
                        Outcome::Event(ev) => {
                                if ev == AppEvent::Stats {
                                        info!(
                                                "uptime {} s; console: {} bytes dropped, {} lines dropped; bus: {} refused",
                                                log::now_us() / 1_000_000,
                                                CONSOLE_BYTES.dropped(),
                                                self.reader.dropped_lines,
                                                self.bus.refused()
                                        );
                                }
                                if let Err(e) = self.bus.publish(ev) {
                                        warn!("event bus full; dropped {e:?}");
                                }
                                Poll::Busy
                        }
                }
        }
}

impl Module for ConsoleMod {
        fn name(&self) -> &'static str {
                "console"
        }

        fn poll(&mut self) -> Poll {
                let mut result = Poll::Idle;
                while let Some(b) = CONSOLE_BYTES.pop() {
                        if let Some(line) = self.reader.push(b) {
                                match self.dispatch(line.as_str()) {
                                        Poll::Shutdown => return Poll::Shutdown,
                                        p => result = p,
                                }
                        }
                }
                result
        }
}

// --- the whole application ------------------------------------------------------------------

/// Build the modules over the board's parts and run them forever. A board's `light_app_main`
/// calls this with its own LED, button, clock and idle hook; it never returns.
pub fn run(led: impl OutputPin, button: impl InputPin, clock: impl Clock + Copy, mut idle: impl Idle) -> ! {
        let mut led_mod = LedMod::new(led, clock, &EVENTS);
        let mut button_mod = ButtonMod::new(button, clock, &EVENTS);
        let mut console_mod = ConsoleMod::new(&EVENTS);

        let mut rt: Runtime = Runtime::new();
        rt.add(&mut led_mod).expect("runtime capacity");
        rt.add(&mut button_mod).expect("runtime capacity");
        rt.add(&mut console_mod).expect("runtime capacity");
        rt.start().expect("every module loads");
        info!("__PROJECT__ running; type 'help' on the console");

        match rt.run(|| idle.idle()) {
                Ok(()) => info!("runtime stopped cleanly"),
                Err(e) => warn!("runtime stopped with {e:?}"),
        }
        loop {
                core::hint::spin_loop();
        }
}

// --- host tests -----------------------------------------------------------------------------

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use core::cell::Cell;
        use std::boxed::Box;

        /// A clock the test moves by hand. Time is an input: a test that sleeps is slow and
        /// flaky, a test that advances a counter is neither.
        #[derive(Clone, Copy)]
        struct TestClock<'a>(&'a Cell<u64>);
        impl Clock for TestClock<'_> {
                fn now_us(&self) -> u64 {
                        self.0.get()
                }
        }

        /// An output that remembers its level.
        struct TestPin<'a>(&'a Cell<bool>);
        impl OutputPin for TestPin<'_> {
                fn set(&mut self, high: bool) {
                        self.0.set(high);
                }
        }

        /// An input the test sets; low means pressed.
        struct TestButton<'a>(&'a Cell<bool>);
        impl InputPin for TestButton<'_> {
                fn is_low(&self) -> bool {
                        self.0.get()
                }
        }

        /// A bus per test, so tests share no state and leak no subscriber slots into each other.
        fn bus() -> &'static EventBus<AppEvent> {
                Box::leak(Box::new(EventBus::new()))
        }

        #[test]
        fn the_led_blinks_as_time_passes() {
                let (now, level) = (Cell::new(0), Cell::new(false));
                let mut led = LedMod::new(TestPin(&level), TestClock(&now), bus());
                led.poll();
                assert!(level.get(), "the first poll turns it on");
                led.poll();
                assert_eq!(led.toggles(), 1, "nothing more until a period has passed");
                now.set(BLINK_PERIOD_US);
                led.poll();
                assert!(!level.get());
                assert_eq!(led.toggles(), 2);
        }

        #[test]
        fn led_off_holds_it_off_and_stops_the_blinking() {
                let (now, level) = (Cell::new(0), Cell::new(false));
                let bus = bus();
                let mut led = LedMod::new(TestPin(&level), TestClock(&now), bus);
                led.poll();
                bus.publish(AppEvent::LedOff).unwrap();
                assert_eq!(led.poll(), Poll::Busy);
                assert!(!level.get());
                assert!(!led.blinking());
                now.set(10 * BLINK_PERIOD_US);
                led.poll();
                assert!(!level.get(), "still off however long it waits");
        }

        #[test]
        fn a_press_is_debounced_then_published() {
                let (now, pressed) = (Cell::new(0), Cell::new(false));
                let bus = bus();
                let listener = bus.subscribe().unwrap();
                let mut button = ButtonMod::new(TestButton(&pressed), TestClock(&now), bus);
                button.poll();
                pressed.set(true);
                now.set(10_000);
                button.poll();
                assert_eq!(bus.poll(&listener), None, "a fresh contact is not a press yet");
                now.set(40_000);
                assert_eq!(button.poll(), Poll::Busy);
                assert_eq!(bus.poll(&listener), Some(AppEvent::Pressed));
                assert_eq!(button.presses(), 1);
        }

        #[test]
        fn a_press_stops_the_blinking_and_the_next_starts_it() {
                let (now, level) = (Cell::new(0), Cell::new(false));
                let bus = bus();
                let mut led = LedMod::new(TestPin(&level), TestClock(&now), bus);
                bus.publish(AppEvent::Pressed).unwrap();
                led.poll();
                assert!(!led.blinking());
                bus.publish(AppEvent::Pressed).unwrap();
                led.poll();
                assert!(led.blinking());
        }

        #[test]
        fn console_lines_become_events() {
                assert_eq!(CLI.dispatch("led off"), Outcome::Event(AppEvent::LedOff));
                assert_eq!(CLI.dispatch("led blink"), Outcome::Event(AppEvent::LedBlink));
                assert_eq!(CLI.dispatch("stats"), Outcome::Event(AppEvent::Stats));
                //   bad arguments and unknown commands are answered on the console, not returned
                assert_eq!(CLI.dispatch("led sideways"), Outcome::Handled);
                assert_eq!(CLI.dispatch("frobnicate"), Outcome::Handled);
                assert_eq!(CLI.dispatch(""), Outcome::Quiet);
        }
}
