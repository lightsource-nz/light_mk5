//! crossfire: every USB-MIDI instrument on the host port hears every other. This crate is
//! the APPLICATION, with no hardware in it.
//!
//! The forwarding engine is `light_midi`, portable and host-tested; the host stack reaches
//! this crate as a [`light_midi::Host`], the status display as a [`SpiDisplayBus`] under the
//! SH1107 driver, the LED as an [`OutputPin`], time as `light_core::log`'s clock. A tangible
//! crossfire -- a Pico, a Pico 2, whatever comes later -- is a hardware-bound module that
//! constructs those concrete parts, hands them to [`serve`], and owns everything this crate
//! must not: pins, chip features, the TinyUSB configuration, the shell ABI, the panic
//! handler.

#![no_std]

use core::fmt::Write;
use light_core::cli::{Cli, Command, Outcome, Parsed, Words};
use light_core::{info, log, warn, Clock, ConstStaticCell, EventBus, LineReader, Mailbox, Module, OutputPin, Poll, Runtime, SpiDisplayBus, Subscription};
use light_display::sh1107::Sh1107;
use light_display::{Display, FrameLayer, UpdateError};
use light_draw::{Flip, Rotation};
use light_font::Font;
use light_midi::{Forwarder, Host, MidiEvent};
use light_ui::{Fonts, Lui, LuiChild, Style, Theme, Ui};

/// USB device slots the engine tracks: TinyUSB's CFG_TUH_MIDI, which every hardware
/// module's tusb_config.h sets to the same four. The engine indexes its table with the
/// mount index directly, so the two must agree.
pub const USB_SLOTS: usize = 4;

#[derive(Clone, Copy, Debug)]
enum AppEvent {
        /// The mounted set changed: the display's text is stale.
        Status,
        /// The RX/TX indicators changed. The fields ride along for `Debug` -- the display
        /// reads the levels from the published status, not the event.
        #[allow(dead_code)]
        Indicators { rx: bool, tx: bool },
        /// Whether anything is mounted, for the LED.
        Mounted(bool),
        Stats,
        /// Tear the host controller down and bring it back, from the console.
        UsbReset,
        /// Whether the engine's root-port-empty verdict resets the controller by itself.
        AutoReset(bool),
}

static EVENTS: EventBus<AppEvent, 8, 3> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

/// A console byte from the transport the hardware module owns. Never blocks; a full
/// mailbox drops the byte, and the line it belonged to will fail to parse and say so.
pub fn push_console_byte(b: u8) {
        let _ = CONSOLE_BYTES.push(b);
}

/// The core 1 heartbeat, bumped by the hardware module's service hook -- see [`Stats`].
pub fn core1_heartbeat() {
        CORE1_PASSES.fetch_add(1, light_core::atomic::Ordering::Relaxed);
}

/// The status the display shows, published by the USB module and read by the OLED module:
/// the engine itself stays private to the module that drives it.
#[derive(Clone, Copy, Debug, Default)]
struct Status {
        mounted: u8,
        hub_addr: u8,
        /// Which of the hub's first four ports carry an instrument.
        ports: [bool; USB_SLOTS],
        rx: bool,
        tx: bool,
}

static STATUS: Mailbox<Status, 1> = Mailbox::new();

/// Heartbeats, one per core, for a post-mortem that reads memory without halting anything:
/// whether each core is still executing its loop is the first question, and it should not
/// take a debugger session that disturbs the answer.
static CORE0_PASSES: light_core::atomic::AtomicU32 = light_core::atomic::AtomicU32::new(0);
static CORE1_PASSES: light_core::atomic::AtomicU32 = light_core::atomic::AtomicU32::new(0);

/// Owns the host stack and the forwarding engine. Every pass: run the stack, apply what it
/// reported, forward what arrived, and say what changed.
pub struct UsbMod<H: Host> {
        host: H,
        forwarder: Forwarder<USB_SLOTS>,
        events: Subscription,
        reset_pending: bool,
        /// The controller is reset whenever a disconnect empties the root port, working
        /// around a stale buffer-control state (hathach/tinyusb#3533) -- and the RP2350 needs
        /// it too: without the reset the next enumeration panicked inside the USB IRQ. The
        /// reset itself hung in tusb_deinit(), which closed devices after tearing down the
        /// port's critical section; that is fixed in the pico-sdk TinyUSB fork. `usb autoreset
        /// off` keeps the switch for diagnosis
        auto_reset: bool,
        packets: u32,
        status: Status,
}

impl<H: Host> UsbMod<H> {
        pub fn new(host: H) -> Self {
                Self { host, forwarder: Forwarder::new(), events: EVENTS.subscribe().expect("subscriber slot"), reset_pending: false, auto_reset: true, packets: 0, status: Status::default() }
        }

        fn publish_status(&mut self) {
                let mut s = Status { mounted: self.forwarder.usb_mounted_count() as u8, hub_addr: self.forwarder.hub_addr(), ports: [false; USB_SLOTS], rx: self.status.rx, tx: self.status.tx };
                for (i, p) in s.ports.iter_mut().enumerate() {
                        *p = self.forwarder.hub_port_occupied(i as u8 + 1);
                }
                self.status = s;
                // the mailbox holds the latest only: a stale status is worthless
                let _ = STATUS.pop();
                let _ = STATUS.push(s);
        }
}

impl<H: Host> Module for UsbMod<H> {
        fn name(&self) -> &'static str {
                "usb"
        }
        fn poll(&mut self) -> Poll {
                CORE0_PASSES.fetch_add(1, light_core::atomic::Ordering::Relaxed);
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Stats => info!("usb: {} mounted, hub addr {}, {} packets forwarded, {} dropped (cable), {} events dropped, auto-reset {}", self.forwarder.usb_mounted_count(), self.forwarder.hub_addr(), self.packets, self.forwarder.dropped, self.host.dropped_events(), if self.auto_reset { "on" } else { "off" }),
                                AppEvent::UsbReset => self.reset_pending = true,
                                AppEvent::AutoReset(on) => {
                                        self.auto_reset = on;
                                        info!("usb: controller auto-reset on an empty root port {}", if on { "on" } else { "off" });
                                }
                                _ => {}
                        }
                }
                //   the reset is done here, at the top of a pass, never from inside the
                // callback that asked for it
                //   a reset unmounts everything, and those unmounts empty the bus, which would
                // ask for a second reset: the verdicts of the pass that follows a reset are not
                // honoured
                let mut just_reset = false;
                if self.reset_pending {
                        self.reset_pending = false;
                        info!("resetting the USB host controller");
                        self.host.reset();
                        just_reset = true;
                }
                self.host.task();
                let mut busy = false;
                while let Some(ev) = self.host.next_event() {
                        busy = true;
                        let change = match ev {
                                MidiEvent::Mounted { idx, mount, bus } => {
                                        match bus {
                                                Some(b) if b.hub_addr != 0 => info!("USB-MIDI device mounted: idx {idx} daddr {} rx {} tx {}, on hub {} port {}", mount.daddr, mount.rx_cables, mount.tx_cables, b.hub_addr, b.hub_port),
                                                _ => info!("USB-MIDI device mounted: idx {idx} daddr {} rx {} tx {}, on the root port", mount.daddr, mount.rx_cables, mount.tx_cables),
                                        }
                                        self.forwarder.mount(idx, mount, bus)
                                }
                                MidiEvent::Unmounted { idx } => {
                                        info!("USB-MIDI device unmounted: idx {idx}");
                                        self.forwarder.unmount(idx)
                                }
                        };
                        let Some(c) = change else {
                                warn!("the host stack reported a slot the engine does not have");
                                continue;
                        };
                        if c.reset_host && !just_reset {
                                if self.auto_reset {
                                        self.reset_pending = true;
                                } else {
                                        info!("root port empty; the controller is left as it is (`usb autoreset on` to reset it)");
                                }
                        }
                        let _ = EVENTS.publish(AppEvent::Mounted(c.any_usb_mounted));
                        self.publish_status();
                        let _ = EVENTS.publish(AppEvent::Status);
                }
                let now_ms = (log::now_us() / 1000) as u32;
                let activity = self.forwarder.service(&mut self.host, now_ms);
                if activity.forwarded {
                        self.packets += 1;
                        busy = true;
                }
                let (rx, tx, changed) = self.forwarder.indicators(now_ms);
                if changed {
                        self.status.rx = rx;
                        self.status.tx = tx;
                        self.publish_status();
                        let _ = EVENTS.publish(AppEvent::Indicators { rx, tx });
                        busy = true;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}

/// The widgets the status page holds: the window and its two indicator labels.
const UI_WIDGETS: usize = 4;
/// The status page's tags, matching design.json's `tag`s.
const TAG_RX: u8 = 2;
const TAG_TX: u8 = 3;
/// The status is event-driven, but the frame layer is polled at a steady rate so a chunked
/// push runs to completion.
const FPS: u32 = 20;

/// The status display: the crossfire status page from the embedded design, on the OLED rotated so
/// the interface runs along the long side. The title bar carries "Crossfire" and, on its subtitle
/// row, the live device/hub line; the RX and TX labels show only while that traffic flows.
/// Event-driven, unpaced: a mount or a burst of MIDI, not a clock. The toolkit invalidates only the
/// widgets that changed, so a burst repaints the indicators alone rather than wiping the glass.
pub struct OledMod<B: SpiDisplayBus, C: Clock> {
        display: Display<'static, Sh1107<B>>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<AppEvent, UI_WIDGETS>,
        /// The interface, parsed from the embedded blob; the status page is built from its root.
        lui: Lui<'static>,
        clock: C,
        /// The controller's RAM offset the panel sits at -- a board fact, handed in.
        display_offset: u8,
        events: Subscription,
        status: Status,
        dirty: bool,
}

impl<B: SpiDisplayBus, C: Clock> OledMod<B, C> {
        /// Build the status module from the board's display parts and the embedded asset blobs. The
        /// theme and interface blobs are parsed here: a bad blob is a build-system bug worth halting
        /// on, like the font the hardware module parses.
        pub fn new(display: Display<'static, Sh1107<B>>, layer: &'static mut FrameLayer, font: Font<'static>, theme_blob: &'static [u8], ui_blob: &'static [u8], clock: C, display_offset: u8) -> Self {
                // in .bss, built in place: the widget arena is a good part of core 0's small stack
                static UI: ConstStaticCell<Ui<AppEvent, UI_WIDGETS>> = ConstStaticCell::new(Ui::new());
                let ui = UI.take();
                let theme = match Theme::parse(theme_blob) {
                        Ok(t) => t,
                        Err(e) => panic!("the embedded theme does not parse: {e:?}"),
                };
                layer.bg = theme.bg;
                ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
                let lui = match Lui::parse(ui_blob) {
                        Ok(l) => l,
                        Err(e) => panic!("the embedded UI does not parse: {e:?}"),
                };
                Self { display, layer, font, ui, lui, clock, display_offset, events: EVENTS.subscribe().expect("subscriber slot"), status: Status::default(), dirty: true }
        }

        /// Push the current status into the widget tree: the device/hub line onto the title bar's
        /// subtitle row, and the RX/TX labels' visibility from the indicators.
        fn apply_status(&mut self) {
                if let Some(root) = self.ui.root() {
                        let s = self.status;
                        let mut line = StackString::<16>::new();
                        if s.hub_addr != 0 {
                                // which ports are occupied rather than how many devices: with four
                                // sockets in front of you that is the question you have
                                let _ = line.write_str("hub ");
                                for (i, p) in s.ports.iter().enumerate() {
                                        let _ = line.write_char(if *p { (b'1' + i as u8) as char } else { '-' });
                                }
                        } else {
                                // no space after the colon: the framed header fits nine cells of
                                // this font, and "devices: N" is ten -- the last cell (the count)
                                // falls off the rounded corner's inset. Dropping the space keeps
                                // the word, the colon and the count.
                                let _ = write!(line, "devices:{}", s.mounted);
                        }
                        self.ui.set_subtitle(root, line.as_str());
                }
                if let Some(id) = self.ui.find(TAG_RX) {
                        self.ui.set_visible(id, self.status.rx);
                }
                if let Some(id) = self.ui.find(TAG_TX) {
                        self.ui.set_visible(id, self.status.tx);
                }
        }
}

impl<B: SpiDisplayBus, C: Clock> Module for OledMod<B, C> {
        fn name(&self) -> &'static str {
                "oled"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.display.driver().set_display_offset(self.display_offset);
                self.display.init(&mut self.clock);
                self.display.driver().clear(false);
                self.layer.set_orientation(Rotation::R90, Flip::None);
                self.layer.set_frame_rate(FPS);
                self.ui.fit(self.layer);
                let root = self.lui.root();
                match self.lui.page(root) {
                        Some(p) => {
                                // a status readout: the design carries no actions, so no child emits
                                if let Err(e) = self.ui.build_lui_with(&p, |_, _: &LuiChild| None) {
                                        warn!("the status page did not build: {e:?}");
                                }
                        }
                        None => warn!("the crossfire design has no root page"),
                }
                self.apply_status();
                self.ui.invalidate_all();
                let style = Style::new(*self.ui.theme(), Fonts::uniform(&self.font));
                self.ui.render(self.layer, &mut self.display, &style, log::now_us());
                info!("status display up: {}px font", self.font.pixel_size());
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.layer.poll(&mut self.display) {
                        Ok(_) => {}
                        Err(UpdateError::Timeout) => warn!("oled chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Status | AppEvent::Indicators { .. } => self.dirty = true,
                                AppEvent::Stats => info!("oled: {} frames, {} skipped, {} chunk timeouts", self.layer.frames(), self.layer.skipped, self.display.timeouts),
                                _ => {}
                        }
                }
                if self.dirty {
                        if let Some(s) = STATUS.pop() {
                                self.status = s;
                        }
                        self.apply_status();
                        self.dirty = false;
                }
                let style = Style::new(*self.ui.theme(), Fonts::uniform(&self.font));
                self.ui.render(self.layer, &mut self.display, &style, log::now_us());
                if self.ui.is_dirty() || self.layer.busy(&self.display) { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                self.display.driver().clear(false);
        }
}

/// The LED: lit while anything is mounted.
pub struct LedMod<P: OutputPin> {
        led: P,
        events: Subscription,
}

impl<P: OutputPin> LedMod<P> {
        pub fn new(led: P) -> Self {
                Self { led, events: EVENTS.subscribe().expect("subscriber slot") }
        }
}

impl<P: OutputPin> Module for LedMod<P> {
        fn name(&self) -> &'static str {
                "led"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::Mounted(on) = ev {
                                self.led.set(on);
                                busy = true;
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.led.set(false);
        }
}

//   the console: the shared CLI owns the grammar and the built-ins (help, loglevel, quit);
// this table is everything this application adds
fn parse_stats(_w: &mut Words) -> Parsed<AppEvent> {
        info!("uptime {} s; console: {} bytes dropped; bus: {} refused; passes core0 {} core1 {}; log dropped {}", log::now_us() / 1_000_000, CONSOLE_BYTES.dropped(), EVENTS.refused(), CORE0_PASSES.load(light_core::atomic::Ordering::Relaxed), CORE1_PASSES.load(light_core::atomic::Ordering::Relaxed), log::pending());
        Parsed::Event(AppEvent::Stats)
}

fn parse_usb(w: &mut Words) -> Parsed<AppEvent> {
        match (w.next(), w.next()) {
                (Some("reset"), _) => Parsed::Event(AppEvent::UsbReset),
                (Some("autoreset"), Some("on")) => Parsed::Event(AppEvent::AutoReset(true)),
                (Some("autoreset"), Some("off")) => Parsed::Event(AppEvent::AutoReset(false)),
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[Command<AppEvent>] = &[
        Command { name: "stats", usage: "stats", parse: parse_stats },
        Command { name: "usb", usage: "usb reset | usb autoreset on|off", parse: parse_usb },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

pub struct ConsoleMod {
        reader: LineReader<96>,
}

impl ConsoleMod {
        pub fn new() -> Self {
                Self { reader: LineReader::new() }
        }

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

impl Default for ConsoleMod {
        fn default() -> Self {
                Self::new()
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

/// Run crossfire on the parts a hardware module built, forever. The module allocates the
/// big pieces where its memory map wants them (statics, not this core's stack) and hands
/// in mutable borrows; this seals them into the runtime.
pub fn serve<H: Host, B: SpiDisplayBus, C: Clock, P: OutputPin>(
        usb: &mut UsbMod<H>,
        oled: &mut OledMod<B, C>,
        led: &mut LedMod<P>,
        console: &mut ConsoleMod,
        idle: impl FnMut(),
) -> ! {
        let mut rt: Runtime<4> = Runtime::new();
        rt.add(usb).expect("capacity");
        rt.add(oled).expect("capacity");
        rt.add(led).expect("capacity");
        rt.add(console).expect("capacity");
        rt.start().expect("start");
        info!("runtime started; plug an instrument in");
        let result = rt.run(idle);
        match result {
                Ok(()) => info!("runtime stopped cleanly; core 0 idle"),
                Err(e) => warn!("runtime stopped with {e:?}; core 0 idle"),
        }
        loop {
                core::hint::spin_loop();
        }
}

/// A tiny fixed write target for text assembled at runtime: log lines for a sink, the
/// panic message, the display's second line. Public because the hardware modules need the
/// same thing for their shell glue.
pub struct StackString<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> StackString<N> {
        pub const fn new() -> Self {
                Self { buf: [0; N], len: 0 }
        }
        pub fn as_str(&self) -> &str {
                core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
        }
        pub fn as_bytes(&self) -> &[u8] {
                &self.buf[..self.len]
        }
}

impl<const N: usize> Default for StackString<N> {
        fn default() -> Self {
                Self::new()
        }
}

impl<const N: usize> Write for StackString<N> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let take = s.len().min(N - self.len);
                self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
                self.len += take;
                Ok(())
        }
}
