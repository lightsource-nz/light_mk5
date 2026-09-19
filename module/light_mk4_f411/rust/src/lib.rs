//! The Rust side of the Blackpill firmware: the LED, the key and the console -- this board's
//! demo on the mk4 runtime, on the CMSIS shell's second chip. The key toggles the blink,
//! the console reports and steers.

#![no_std]

use light_core::button::{Button, ButtonEvent};
use light_core::cli::{Cli, Command, Outcome, Parsed, Words};
use light_core::{info, log, warn, Blinker, EventBus, LineReader, Mailbox, Module, Poll, Runtime, Subscription};
mod board;
use board::*;
use light_shell_cmsis::{drain_log, panic_report, read_console, ShellInfo};
use light_stm32f4::gpio::{Input, Output};
use light_stm32f4::{now_us, Breathe, Clocks, SysClock};

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        Key(bool),
        LedBlink(bool),
        LedRate(u32),
        Stats,
}

static EVENTS: EventBus<AppEvent, 8, 2> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

/// The LED is active low: the blinker's "on" is a low pin.
struct ActiveLow<'a>(&'a mut Output);
impl light_core::OutputPin for ActiveLow<'_> {
        fn set(&mut self, high: bool) {
                self.0.set(!high)
        }
}

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
                                // a press toggles the blink; the release is nothing
                                AppEvent::Key(true) => {
                                        self.blinking = !self.blinking;
                                        if !self.blinking {
                                                self.led.set(true);
                                        }
                                        info!("blink {}", if self.blinking { "on" } else { "off" });
                                }
                                AppEvent::LedBlink(on) => {
                                        self.blinking = on;
                                        if !on {
                                                self.led.set(true);
                                        }
                                }
                                AppEvent::LedRate(ms) => self.blinker = Blinker::new(u64::from(ms) * 1000),
                                AppEvent::Stats => info!("led: {} toggles, blinking={}", self.toggles, self.blinking),
                                _ => {}
                        }
                }
                if self.blinking && self.blinker.poll(&mut ActiveLow(&mut self.led), &SysClock) {
                        self.toggles += 1;
                        busy = true;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.led.set(true);
        }
}

struct KeyMod {
        key: Button<Input>,
}

impl Module for KeyMod {
        fn name(&self) -> &'static str {
                "key"
        }
        fn poll(&mut self) -> Poll {
                let now_ms = (now_us() / 1000) as u32;
                let Some(ev) = self.key.poll(now_ms) else { return Poll::Idle };
                let pressed = ev == ButtonEvent::Press;
                info!("key {}", if pressed { "pressed" } else { "released" });
                let _ = EVENTS.publish(AppEvent::Key(pressed));
                Poll::Busy
        }
}

//   the console: the shared CLI owns the grammar and the built-ins (help, loglevel, quit);
// this table is everything this application adds
fn parse_stats(_w: &mut Words) -> Parsed<AppEvent> {
        info!("uptime {} s; console: {} bytes dropped; bus: {} refused; log pending {}", now_us() / 1_000_000, CONSOLE_BYTES.dropped(), EVENTS.refused(), log::pending());
        Parsed::Event(AppEvent::Stats)
}

fn parse_led(w: &mut Words) -> Parsed<AppEvent> {
        match (w.next(), w.next()) {
                (Some("blink"), _) => Parsed::Event(AppEvent::LedBlink(true)),
                (Some("off"), _) => Parsed::Event(AppEvent::LedBlink(false)),
                (Some("rate"), Some(ms)) => match ms.parse::<u32>() {
                        Ok(ms) if ms >= 10 => Parsed::Event(AppEvent::LedRate(ms)),
                        _ => Parsed::Usage,
                },
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[Command<AppEvent>] = &[
        Command { name: "stats", usage: "stats", parse: parse_stats },
        Command { name: "led", usage: "led blink|off | led rate MS (>= 10)", parse: parse_led },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

struct ConsoleMod {
        reader: LineReader<96>,
}

impl ConsoleMod {
        fn dispatch(&mut self, line: &str) -> Poll {
                match CLI.dispatch(line) {
                        Outcome::Quiet => Poll::Idle,
                        Outcome::Shutdown => Poll::Shutdown,
                        Outcome::Event(e) => {
                                let _ = EVENTS.publish(e);
                                Poll::Busy
                        }
                        Outcome::Handled => Poll::Busy,
                }
        }
}

impl Module for ConsoleMod {
        fn name(&self) -> &'static str {
                "console"
        }
        fn poll(&mut self) -> Poll {
                let drained = drain_log(4);
                read_console(|b| {
                        let _ = CONSOLE_BYTES.push(b);
                });
                let mut result = if drained > 0 { Poll::Busy } else { Poll::Idle };
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

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        let clocks = Clocks { sys_hz: info.clk_sys_hz, apb2_hz: info.clk_apb2_hz, tim_hz: info.clk_tim_hz };
        light_stm32f4::clock_init(&clocks);
        log::set_clock(now_us);
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("blackpill: sys {} Hz, timers {} Hz", clocks.sys_hz, clocks.tim_hz);

        let mut led_mod = LedMod { led: p.led, blinker: Blinker::new(500_000), blinking: true, toggles: 0, events: EVENTS.subscribe().expect("slot") };
        let mut key_mod = KeyMod { key: Button::new(p.key, true) };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<3> = Runtime::new();
        rt.add(&mut console_mod).expect("capacity");
        rt.add(&mut led_mod).expect("capacity");
        rt.add(&mut key_mod).expect("capacity");
        rt.start().expect("start");
        info!("runtime started; the key toggles the blink, type 'help' on the console");
        let mut idle = Breathe;
        let result = rt.run(|| light_core::Idle::idle(&mut idle));
        drain_log(64);
        match result {
                Ok(()) => info!("runtime stopped cleanly"),
                Err(e) => warn!("runtime stopped with {e:?}"),
        }
        drain_log(64);
        loop {
                core::hint::spin_loop();
        }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        panic_report(info)
}
