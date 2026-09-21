//! crossfire: every USB-MIDI instrument on the host port hears every other. This crate is
//! the APPLICATION, with no hardware in it.
//!
//! The forwarding engine is `light_midi`, portable and host-tested; the host stack reaches
//! this crate as a [`light_midi::Host`], the status display as a [`SpiDisplayBus`] under the
//! SH1107 driver, the LED as an [`OutputPin`], time as `light_core::log`'s clock. A tangible
//! crossfire -- a Pico, a Pico 2, whatever comes later -- is a hardware-bound module that
//! constructs those concrete parts, hands them to [`serve`], and owns everything this crate
//! must not: pins, chip features, the USB host stack, the shell ABI, the panic handler.

#![no_std]

use core::fmt::Write;
use light_core::cli::{Cli, Command, Outcome, Parsed, Words};
use light_core::{info, log, warn, Clock, ConstStaticCell, EventBus, LineReader, Mailbox, Module, OutputPin, Poll, Runtime, SpiDisplayBus, Subscription};
use light_display::sh1107::Sh1107;
use light_display::{Display, FrameLayer, UpdateError};
use light_draw::{Flip, Rotation};
use light_font::Font;
use light_midi::{Forwarder, Host, MidiEvent};
use light_ui::{Descent, Fonts, Lui, LuiChild, Style, Theme, Ui};

/// USB device slots the engine tracks: the host stack's MIDI slot count (`light_rp2::usb_host::SLOTS`,
/// four). The engine indexes its table with the mount index directly, so the two must agree.
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
        /// The BOOTSEL button was pressed: move the display to the next page.
        NavToggle,
}

static EVENTS: EventBus<AppEvent, 8, 3> = EventBus::new();
static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();

/// A console byte from the transport the hardware module owns. Never blocks; a full
/// mailbox drops the byte, and the line it belonged to will fail to parse and say so.
pub fn push_console_byte(b: u8) {
        let _ = CONSOLE_BYTES.push(b);
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

/// The counters the stats page shows, published by the USB module and read by the OLED module when
/// that page is up. Uptime is not here -- the display reads the clock directly for that.
#[derive(Clone, Copy, Debug, Default)]
struct StatsSnapshot {
        received: u32,
        forwarded: u32,
}

static STATS: Mailbox<StatsSnapshot, 1> = Mailbox::new();

/// The core 0 heartbeat, for a post-mortem that reads memory without halting anything: whether
/// the core is still executing its loop is the first question, and it should not take a debugger
/// session that disturbs the answer. The console core keeps its own tick in the port.
static CORE0_PASSES: light_core::atomic::AtomicU32 = light_core::atomic::AtomicU32::new(0);

/// Owns the host stack and the forwarding engine. Every pass: run the stack, apply what it
/// reported, forward what arrived, and say what changed.
pub struct UsbMod<H: Host> {
        host: H,
        forwarder: Forwarder<USB_SLOTS>,
        events: Subscription,
        packets: u32,
        status: Status,
}

impl<H: Host> UsbMod<H> {
        pub fn new(host: H) -> Self {
                Self { host, forwarder: Forwarder::new(), events: EVENTS.subscribe().expect("subscriber slot"), packets: 0, status: Status::default() }
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

        fn publish_stats(&self) {
                let s = StatsSnapshot { received: self.forwarder.received, forwarded: self.packets };
                // latest only, like the status mailbox
                let _ = STATS.pop();
                let _ = STATS.push(s);
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
                                AppEvent::Stats => info!("usb: {} mounted, hub addr {}, {} packets forwarded, {} dropped (cable), {} events dropped", self.forwarder.usb_mounted_count(), self.forwarder.hub_addr(), self.packets, self.forwarder.dropped, self.host.dropped_events()),
                                _ => {}
                        }
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
                if busy {
                        // the counters moved (a forward, a drop, a mount): refresh the stats page's data
                        self.publish_stats();
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}

/// Room for the widest page (the stats page: a window and three rows) with headroom for the page
/// transition to build the incoming tree.
const UI_WIDGETS: usize = 8;
/// The pages, in design.json order.
const PAGE_STATUS: usize = 0;
const PAGE_STATS: usize = 1;
/// The status page's tags, matching design.json's `tag`s.
const TAG_RX: u8 = 2;
const TAG_TX: u8 = 3;
/// The stats page's tags.
const TAG_UP: u8 = 10;
const TAG_RECV: u8 = 11;
const TAG_FWD: u8 = 12;
/// The status is event-driven, but the frame layer is polled at a steady rate so a chunked
/// push runs to completion.
const FPS: u32 = 20;
/// How often the stats page redraws while it is up, so its uptime ticks.
const STATS_REFRESH_MS: u32 = 1000;

/// The display: the crossfire interface from the embedded design, on the OLED rotated so it runs
/// along the long side. Two pages the BOOTSEL button cycles between -- a status page (title bar with
/// the live device/hub line, RX/TX labels that show only while that traffic flows) and a stats page
/// (uptime and the forwarding counters). Event-driven, unpaced on the status page; the stats page
/// redraws once a second so its uptime ticks. The toolkit invalidates only the widgets that changed,
/// so a burst of MIDI repaints the indicators alone rather than wiping the glass.
pub struct OledMod<B: SpiDisplayBus, C: Clock> {
        display: Display<'static, Sh1107<B>>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<AppEvent, UI_WIDGETS>,
        /// The interface, parsed from the embedded blob; each page is built from it on demand.
        lui: Lui<'static>,
        clock: C,
        /// The controller's RAM offset the panel sits at -- a board fact, handed in.
        display_offset: u8,
        events: Subscription,
        status: Status,
        stats: StatsSnapshot,
        /// Which design page is shown ([`PAGE_STATUS`]/[`PAGE_STATS`]).
        page: usize,
        /// When the stats page last redrew, for its once-a-second uptime tick.
        stats_ms: u32,
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
                Self { display, layer, font, ui, lui, clock, display_offset, events: EVENTS.subscribe().expect("subscriber slot"), status: Status::default(), stats: StatsSnapshot::default(), page: PAGE_STATUS, stats_ms: 0, dirty: true }
        }

        /// Refresh whichever page is shown from the latest published data.
        fn apply_page(&mut self) {
                match self.page {
                        PAGE_STATS => self.apply_stats(),
                        _ => self.apply_status(),
                }
        }

        /// Push the current status into the status page: the device/hub line onto the title bar's
        /// subtitle row, and the RX/TX labels' visibility from the indicators.
        fn apply_status(&mut self) {
                if let Some(s) = STATUS.pop() {
                        self.status = s;
                }
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

        /// Fill the stats page's rows: uptime read straight from the clock, the forwarding counters
        /// from the latest published snapshot.
        fn apply_stats(&mut self) {
                if let Some(s) = STATS.pop() {
                        self.stats = s;
                }
                let up_s = (log::now_us() / 1_000_000) as u32;
                let mut line = StackString::<16>::new();
                let _ = write!(line, "up {up_s}s");
                if let Some(id) = self.ui.find(TAG_UP) {
                        self.ui.set_text(id, line.as_str());
                }
                let mut line = StackString::<16>::new();
                let _ = write!(line, "recv {}", self.stats.received);
                if let Some(id) = self.ui.find(TAG_RECV) {
                        self.ui.set_text(id, line.as_str());
                }
                let mut line = StackString::<16>::new();
                let _ = write!(line, "fwd {}", self.stats.forwarded);
                if let Some(id) = self.ui.find(TAG_FWD) {
                        self.ui.set_text(id, line.as_str());
                }
        }

        /// Cycle to the next page, sliding it in (forward for status->stats, back the other way), then
        /// fill it from the current data.
        fn show_next_page(&mut self) {
                let target = if self.page == PAGE_STATUS { PAGE_STATS } else { PAGE_STATUS };
                let back = target < self.page;
                //   force one axis for both directions: the two pages have different layouts (row vs
                // stack) whose layout-derived descents run different axes. A fixed descent plus the
                // `back` flag gives a mirrored slide either way. Vertical on-screen -- under the panel's
                // R90 rotation that is a within-row (sub-byte) buffer shift.
                if let Some(p) = self.lui.page(target) {
                        if let Err(e) = self.ui.navigate_lui(&p, back, Some(Descent::FromBottom), |_, _: &LuiChild| None) {
                                warn!("page {target} did not build: {e:?}");
                                return;
                        }
                        self.page = target;
                        self.stats_ms = (log::now_us() / 1000) as u32;
                        self.apply_page();
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
                //   region buffering: the forward page turn scrolls the outgoing page off this one 1 KB
                // buffer to reveal the incoming, the back turn covers it -- a mirrored reveal/cover. The
                // panel is 1 bpp, but under the R90 rotation the on-screen horizontal slide is a
                // whole-row vertical buffer shift, which shift_region handles for Mono1.
                self.display.set_region_buffering(true);
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
                self.apply_page();
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
                                AppEvent::NavToggle => self.show_next_page(),
                                AppEvent::Status | AppEvent::Indicators { .. } => self.dirty = true,
                                AppEvent::Stats => info!("oled: {} frames, {} skipped, {} chunk timeouts", self.layer.frames(), self.layer.skipped, self.display.timeouts),
                                _ => {}
                        }
                }
                //   the stats page carries a live uptime, so tick it once a second; the status page is
                // purely event-driven and never needs a periodic redraw
                if self.page == PAGE_STATS {
                        let now = (log::now_us() / 1000) as u32;
                        if now.wrapping_sub(self.stats_ms) >= STATS_REFRESH_MS {
                                self.stats_ms = now;
                                self.dirty = true;
                        }
                }
                if self.dirty {
                        self.apply_page();
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
        info!("uptime {} s; console: {} bytes dropped; bus: {} refused; passes {}; log dropped {}", log::now_us() / 1_000_000, CONSOLE_BYTES.dropped(), EVENTS.refused(), CORE0_PASSES.load(light_core::atomic::Ordering::Relaxed), log::pending());
        Parsed::Event(AppEvent::Stats)
}

static COMMANDS: &[Command<AppEvent>] = &[
        Command { name: "stats", usage: "stats", parse: parse_stats },
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

/// The BOOTSEL button as the display's one navigation control: each press cycles the OLED to the
/// next page. The board hands in its port's flash-safe reader (`light_rp2::shell::bootsel`) so this
/// crate stays hardware-independent. Reading BOOTSEL briefly stops the world, so it is sampled at a
/// modest rate rather than every pass, and a press is taken on the edge -- one page turn per push.
pub struct NavMod {
        pressed: fn() -> bool,
        was_down: bool,
        last_ms: u32,
}

impl NavMod {
        pub fn new(pressed: fn() -> bool) -> Self {
                Self { pressed, was_down: false, last_ms: 0 }
        }
}

impl Module for NavMod {
        fn name(&self) -> &'static str {
                "nav"
        }
        fn poll(&mut self) -> Poll {
                let now = (log::now_us() / 1000) as u32;
                // ~20 Hz: a button needs no more, and each read is an interrupts-off window that a
                // fast poll loop must not repeat needlessly while the USB host runs beside it
                if now.wrapping_sub(self.last_ms) < 50 {
                        return Poll::Idle;
                }
                self.last_ms = now;
                let down = (self.pressed)();
                if down && !self.was_down {
                        self.was_down = true;
                        let _ = EVENTS.publish(AppEvent::NavToggle);
                        return Poll::Busy;
                }
                if !down {
                        self.was_down = false;
                }
                Poll::Idle
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
        nav: &mut NavMod,
        idle: impl FnMut(),
) -> ! {
        let mut rt: Runtime<5> = Runtime::new();
        rt.add(usb).expect("capacity");
        rt.add(oled).expect("capacity");
        rt.add(led).expect("capacity");
        rt.add(console).expect("capacity");
        rt.add(nav).expect("capacity");
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
