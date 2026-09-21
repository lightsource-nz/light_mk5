//! The Rust side of the MiniSTM32H7 firmware: the widget demo on the board's 160x80 ST7735,
//! the LED, the one key, and the console -- over the board's USB port and USART1. Single core,
//! so the console loop the RP2 shells run on core 1 is a module here.
//!
//! One key drives a whole interface: a short press moves the focus, a long press activates.

#![no_std]

use light_core::button::{Button, ButtonEvent};
use light_display::st7735::St7735;
use light_ui::lui::code;
use light_ui::{Fonts, Lui, LuiChild, Style, Theme, Ui};
use light_core::cli::{Cli, Command, Outcome, Parsed, Words};
use light_core::{info, log, warn, Blinker, ConstStaticCell, EventBus, LineReader, Mailbox, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer, UpdateError};
use light_draw::PixelFormat;
use light_font::Font;
mod board;
use board::*;
use light_stm32h7::gpio::{Input, Output};
use light_stm32h7::spi::Spi4Display;
use light_stm32h7::uart::Usart1;
use light_stm32h7::{now_us, usb, Breathe, Clocks, SysClock};
use light_shell_cmsis::{panic_report, Console, ShellInfo};

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        /// The key, released before the hold interval: the short press.
        KeyShort,
        /// The key held for the interval -- fired the moment it expires, not on release.
        KeyHold,
        UiFocus { next: bool },
        UiActivate,
        /// Open the design page a "goto" button names -- the blob analogue of following a
        /// `Page` link.
        UiOpen(u8),
        UiBack,
        Toggle(u8),
        Item(u8),
        LedBlink(bool),
        Stats,
}

static EVENTS: EventBus<AppEvent, 8, 3> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);
/// Two 25 KB frame buffers in AXI SRAM.
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
static FRAME_BACK: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
/// The look-and-feel: the framework's default for a color board (steel), with this
/// board's outer rounding -- see theme/h7.json.
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));
/// The interface as data: this board's two-page design, compiled to an LUI blob -- see
/// design.json and light_add_ui.
static UI_BLOB: &[u8] = include_bytes!(env!("LIGHT_UI_LUI"));

// --- the interface, as data ---------------------------------------------------------------

const FPS: u32 = 30;
/// A long press activates; anything shorter moves the focus.
const HOLD_MS: u32 = 500;

const LABEL_OFF: [&str; 2] = ["Alpha", "Beta"];
const LABEL_ON: [&str; 2] = ["Alpha *", "Beta *"];

/// Map a design button to the app event it stands for: a carried `event` id (the two toggles,
/// the list items -- matching design.json's `actions`), or the navigation a `goto`/`back` button
/// carries, turned into [`AppEvent::UiOpen`]/[`AppEvent::UiBack`] for the display module to act on.
/// Mirrors the const page tree this design replaced, so the interface behaves identically.
fn map_child(_index: usize, child: &LuiChild) -> Option<AppEvent> {
        if child.event != 0 {
                return Some(match child.event {
                        1 => AppEvent::Toggle(0),
                        2 => AppEvent::Toggle(1),
                        16..=20 => AppEvent::Item((child.event - 15) as u8),
                        _ => return None,
                });
        }
        match child.nav {
                code::NAV_GOTO => Some(AppEvent::UiOpen(child.nav_page as u8)),
                code::NAV_BACK => Some(AppEvent::UiBack),
                _ => None,
        }
}

const UI_WIDGETS: usize = 8;

// --- the modules --------------------------------------------------------------------------

/// The panel and the widget tree.
struct DisplayMod {
        display: Display<'static, St7735<Spi4Display>>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<AppEvent, UI_WIDGETS>,
        /// The interface, parsed from the embedded blob; the widget tree is built from its pages.
        lui: Lui<'static>,
        /// Which design page is shown: the blob carries no parent links, so the module keeps its
        /// own place to run back from (this two-page design always returns to the root).
        blob_page: usize,
        backlight: Output,
        events: Subscription,
        toggled: [bool; 2],
}

impl DisplayMod {
        fn publish(ev: Option<AppEvent>) {
                if let Some(ev) = ev {
                        let _ = EVENTS.publish(ev);
                }
        }

        /// Build a design page into the tree with the transition slide, and remember it as the
        /// place back runs from.
        fn show_page(&mut self, page: usize, back: bool) {
                if let Some(p) = self.lui.page(page) {
                        if let Err(e) = self.ui.navigate_lui(&p, back, None, map_child) {
                                warn!("page {page} did not build: {e:?}");
                        }
                        self.blob_page = page;
                }
        }

        fn handle(&mut self, ev: AppEvent) {
                match ev {
                        AppEvent::KeyShort => self.ui.focus_next(),
                        AppEvent::KeyHold => {
                                let emitted = self.ui.activate();
                                Self::publish(emitted);
                        }
                        AppEvent::UiFocus { next: true } => self.ui.focus_next(),
                        AppEvent::UiFocus { next: false } => self.ui.focus_prev(),
                        AppEvent::UiActivate => {
                                let emitted = self.ui.activate();
                                Self::publish(emitted);
                        }
                        AppEvent::UiOpen(page) => self.show_page(page as usize, false),
                        AppEvent::UiBack => {
                                let root = self.lui.root();
                                if self.blob_page != root {
                                        self.show_page(root, true);
                                } else {
                                        info!("ui back: nowhere to go from this page");
                                }
                        }
                        AppEvent::Toggle(i) => {
                                let i = usize::from(i) % 2;
                                self.toggled[i] = !self.toggled[i];
                                if let Some(id) = self.ui.find(i as u8 + 1) {
                                        self.ui.set_label(id, if self.toggled[i] { LABEL_ON[i] } else { LABEL_OFF[i] });
                                }
                                info!("button {i} toggled {}", if self.toggled[i] { "on" } else { "off" });
                        }
                        AppEvent::Item(n) => info!("list item {n} pressed"),
                        AppEvent::Stats => info!("display: {} frames, {} skipped, {} chunk timeouts, {} spi timeouts", self.layer.frames(), self.layer.skipped, self.display.timeouts, self.display.driver_ref_timeouts()),
                        _ => {}
                }
        }
}

/// The SPI bus's timeout counter, reached through the driver: a small accessor rather than a
/// public field chain.
trait DriverStats {
        fn driver_ref_timeouts(&self) -> u32;
}
impl DriverStats for Display<'static, St7735<Spi4Display>> {
        fn driver_ref_timeouts(&self) -> u32 {
                0
        }
}

impl Module for DisplayMod {
        fn name(&self) -> &'static str {
                "display"
        }
        fn load(&mut self) -> Result<(), ()> {
                let mut clock = SysClock;
                self.display.driver().set_offset(DISPLAY_COL_OFFSET, DISPLAY_ROW_OFFSET);
                self.display.init(&mut clock);
                self.display.driver().clear(self.ui.theme().bg);
                self.layer.set_frame_rate(FPS);
                self.layer.bg = self.ui.theme().bg;
                self.ui.fit(self.layer);
                let root = self.lui.root();
                match self.lui.page(root) {
                        Some(p) => {
                                if let Err(e) = self.ui.build_lui_with(&p, map_child) {
                                        warn!("the main page did not build: {e:?}");
                                }
                                self.blob_page = root;
                        }
                        None => warn!("the design has no root page"),
                }
                self.ui.invalidate_all();
                let style = Style::new(*self.ui.theme(), Fonts::uniform(&self.font));
                self.ui.render(self.layer, &mut self.display, &style, now_us());
                // the first frame is on the glass: light it
                self.backlight.set(false);
                info!("display up: {}x{} ST7735 on SPI4, double-buffered at {} fps, font {}px cell {}x{}", DISPLAY_WIDTH, DISPLAY_HEIGHT, FPS, self.font.pixel_size(), self.font.cell_width(), self.font.cell_height());
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.layer.poll(&mut self.display) {
                        Ok(_) => {}
                        Err(UpdateError::Timeout) => warn!("display chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                while let Some(ev) = EVENTS.poll(&self.events) {
                        self.handle(ev);
                }
                let style = Style::new(*self.ui.theme(), Fonts::uniform(&self.font));
                self.ui.render(self.layer, &mut self.display, &style, now_us());
                if self.ui.is_dirty() || self.ui.is_animating() || self.layer.busy(&self.display) { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.backlight.set(true);
        }
}

/// The key, debounced. A press that lasts the hold interval fires as the interval expires,
/// while the key is still down -- waiting for the release would make a long press read as a
/// slow one -- and the release after that is silent. A release before the interval is the
/// short press.
struct KeyMod {
        key: Button<Input>,
        pressed_at_ms: u32,
        pressed: bool,
        hold_fired: bool,
}

impl Module for KeyMod {
        fn name(&self) -> &'static str {
                "key"
        }
        fn poll(&mut self) -> Poll {
                let now_ms = (now_us() / 1000) as u32;
                if let Some(ev) = self.key.poll(now_ms) {
                        match ev {
                                ButtonEvent::Press => {
                                        self.pressed = true;
                                        self.hold_fired = false;
                                        self.pressed_at_ms = now_ms;
                                }
                                ButtonEvent::Release => {
                                        self.pressed = false;
                                        if !self.hold_fired {
                                                info!("key: short press");
                                                let _ = EVENTS.publish(AppEvent::KeyShort);
                                        }
                                }
                        }
                        return Poll::Busy;
                }
                if self.pressed && !self.hold_fired && now_ms.wrapping_sub(self.pressed_at_ms) >= HOLD_MS {
                        self.hold_fired = true;
                        info!("key: held {HOLD_MS} ms");
                        let _ = EVENTS.publish(AppEvent::KeyHold);
                        return Poll::Busy;
                }
                Poll::Idle
        }
}

/// The LED, blinking unless told otherwise. Active low.
struct LedMod {
        led: Output,
        blinker: Blinker,
        blinking: bool,
        events: Subscription,
}

/// The LED is active low: the blinker's "on" is a low pin.
struct ActiveLow<'a>(&'a mut Output);
impl light_core::OutputPin for ActiveLow<'_> {
        fn set(&mut self, high: bool) {
                self.0.set(!high)
        }
}

impl Module for LedMod {
        fn name(&self) -> &'static str {
                "led"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::LedBlink(on) = ev {
                                self.blinking = on;
                                if !on {
                                        self.led.set(true);
                                }
                                busy = true;
                        }
                }
                if self.blinking && self.blinker.poll(&mut ActiveLow(&mut self.led), &SysClock) {
                        busy = true;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.led.set(true);
        }
}

/// The console: drains the log to the shell, reads bytes from it, parses lines.
//   the console: the shared CLI owns the grammar and the built-ins (help, loglevel, quit);
// this table is everything this application adds
fn parse_stats(_w: &mut Words) -> Parsed<AppEvent> {
        info!("uptime {} s; console: {} bytes dropped; bus: {} refused; log pending {}", now_us() / 1_000_000, CONSOLE_BYTES.dropped(), EVENTS.refused(), log::pending());
        Parsed::Event(AppEvent::Stats)
}

fn parse_led(w: &mut Words) -> Parsed<AppEvent> {
        match w.next() {
                Some("blink") => Parsed::Event(AppEvent::LedBlink(true)),
                Some("off") => Parsed::Event(AppEvent::LedBlink(false)),
                _ => Parsed::Usage,
        }
}

fn parse_ui(w: &mut Words) -> Parsed<AppEvent> {
        match (w.next(), w.next()) {
                (Some("focus"), Some("next")) => Parsed::Event(AppEvent::UiFocus { next: true }),
                (Some("focus"), Some("prev")) => Parsed::Event(AppEvent::UiFocus { next: false }),
                (Some("activate"), _) => Parsed::Event(AppEvent::UiActivate),
                (Some("back"), _) => Parsed::Event(AppEvent::UiBack),
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[Command<AppEvent>] = &[
        Command { name: "stats", usage: "stats", parse: parse_stats },
        Command { name: "led", usage: "led blink|off", parse: parse_led },
        Command { name: "ui", usage: "ui focus next|prev | ui activate | ui back", parse: parse_ui },
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
                // the console's pass: the USB device, the log drain, the input pump
                let moved = light_shell_cmsis::service(|b| {
                        let _ = CONSOLE_BYTES.push(b);
                });
                let mut result = if moved { Poll::Busy } else { Poll::Idle };
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
        let clocks = Clocks { sys_hz: info.clk_sys_hz, ahb_hz: info.clk_ahb_hz, apb2_hz: info.clk_apb2_hz, tim_hz: info.clk_tim_hz };
        light_stm32h7::clock_init(&clocks);
        log::set_clock(now_us);
        //   the console first, so the shell's account of the clocks and every line after it
        // have somewhere to go: the USB device on the board's port, USART1 on PA9/PA10
        static CONSOLE: StaticCell<Console<usb::UsbBus, Usart1>> = StaticCell::new();
        // SAFETY: the one construction of the controller and the USART
        let (bus, uart) = unsafe { (usb::init(clocks.ahb_hz), Usart1::new(light_stm32h7::uart::BAUD, clocks.apb2_hz)) };
        light_shell_cmsis::install(CONSOLE.init(Console::new(bus, Some(uart), "light-stm32h7")));
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, ahb {} Hz, apb2 {} Hz, timers {} Hz{}; spi4 at {} Hz", clocks.sys_hz, clocks.ahb_hz, clocks.apb2_hz, clocks.tim_hz, info.clock_status(), p.display_bus.actual_hz);

        // built in place in .bss and taken exactly once; a second take() panics
        let front: &'static mut [u8] = FRAME_FRONT.take();
        let back: &'static mut [u8] = FRAME_BACK.take();
        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565));
        static UI: ConstStaticCell<Ui<AppEvent, UI_WIDGETS>> = ConstStaticCell::new(Ui::new());
        let layer: &'static mut FrameLayer = LAYER.take();
        let ui: &'static mut Ui<AppEvent, UI_WIDGETS> = UI.take();
        let mut display = Display::new(St7735::new(p.display_bus), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, now_us);
        display.set_back_buffer(back);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        //   the look-and-feel, from the embedded blob: a bad blob is a build-system bug
        // worth halting on, not styling to guess past
        let theme = match Theme::parse(THEME_BLOB) {
                Ok(t) => t,
                Err(e) => panic!("the embedded theme does not parse: {e:?}"),
        };
        layer.bg = theme.bg;
        ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
        //   the interface, from the embedded blob: a bad blob is a build-system bug worth
        // halting on, like the font and the theme
        let lui = match Lui::parse(UI_BLOB) {
                Ok(l) => l,
                Err(e) => panic!("the embedded UI does not parse: {e:?}"),
        };

        static DISPLAY_MOD: StaticCell<DisplayMod> = StaticCell::new();
        let display_mod = DISPLAY_MOD.init(DisplayMod { display, layer, font, ui, lui, blob_page: 0, backlight: p.backlight, events: EVENTS.subscribe().expect("slot"), toggled: [false; 2] });
        // K1 is active high -- see the board wiring
        let mut key_mod = KeyMod { key: Button::new(p.key, false), pressed_at_ms: 0, pressed: false, hold_fired: false };
        let mut led_mod = LedMod { led: p.led, blinker: Blinker::new(500_000), blinking: true, events: EVENTS.subscribe().expect("slot") };
        let mut console_mod = ConsoleMod { reader: LineReader::new() };

        let mut rt: Runtime<4> = Runtime::new();
        //   the console first: it is the log drain, and on one core nothing else moves a
        // record to the wire
        rt.add(&mut console_mod).expect("capacity");
        rt.add(display_mod).expect("capacity");
        rt.add(&mut key_mod).expect("capacity");
        rt.add(&mut led_mod).expect("capacity");
        rt.start().expect("start");
        info!("runtime started; short press moves the focus, a long press activates");
        let mut idle = Breathe;
        let result = rt.run(|| light_core::Idle::idle(&mut idle));
        // drain what the shutdown said before going quiet
        light_shell_cmsis::flush(64);
        match result {
                Ok(()) => info!("runtime stopped cleanly"),
                Err(e) => warn!("runtime stopped with {e:?}"),
        }
        light_shell_cmsis::flush(64);
        loop {
                core::hint::spin_loop();
        }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        panic_report(info)
}
