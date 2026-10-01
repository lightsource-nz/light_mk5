//! The dictaphone: a voice recorder application with no hardware and no page layout in it.
//! Recording and playback are the framework's [`light_recorder`]; this crate is what makes that
//! engine a dictaphone -- takes auto-named `REC_NNNN.WAV` (8.3: the filesystem writes short names
//! only), a record/play pair of buttons, a browsable recordings list -- plus the full diagnostic
//! console (`rec`/`play`/`fs`/`tone`/`micdbg`/...), the same commands the buttons drive.
//!
//! A UI crate (`light_app_dictaphone`, `light_app_dictaphone_wide`) supplies the page
//! TREE -- what the interface looks like -- while this crate supplies everything the tree
//! emits into: the event vocabulary, the recordings list ([`FilesMod`]), and the
//! display-driving [`DisplayMod`] (render loop, touch routing, command plumbing -- the
//! machinery every layout shares). A hardware-bound module then constructs the concrete
//! parts and owns everything neither crate may: pins, the display driver, the touch
//! controller, the I2S transport and its buffers, the card, the shell ABI and the panic
//! handler. The seams, in order of appearance:
//! - [`Event<X>`] carries the app's events plus a board EXTENSION type, on a bus that
//!   stays a board static and reaches the app as `&'static dyn `[`Bus`].
//! - [`Store`] is the card as the app sees it: bring it up per insertion, hand a
//!   [`BlockDevice`](light_core::hal::BlockDevice) per mounted operation.
//! - The board constructs the [`Recorder`] with [`recorder_events`], which is how the recorder
//!   reads this app's commands and publishes its [`AudioStatus`] into it.
//! - The UI crate's pages macro expands the tree in the board crate, where the event type
//!   is concrete; [`dictaphone_commands!`] splices the app's console rows ahead of the
//!   board's own.

#![no_std]

use core::fmt::Write;
use light_core::cli::{Cli, Outcome, Parsed, Words};
use light_core::{debug, info, log, warn, Bus, Clock, LineReader, Mailbox, Module, Poll, Subscription};
use light_display::{Display, DisplayDriver, FrameLayer, Region, UpdateError};
use light_draw::Rotation;
use light_font::Font;
use light_fs::Fat;
use light_input::imu::Orientation;
use light_input::touch::Gesture;
use light_ui::{Fonts, IndicatorShape, Lui, LuiChild, Style, SwipeDir, TextSlot, Touch, Ui};

//   what the board crates build against, from one place
pub use light_input::drivers::cst816t::Event as TouchSample;
pub use light_recorder::console as recorder_console;
pub use light_ui::{Axis, Descent};
pub use light_ui_components::{DirEntry, FilePicker, Order};

/// Widget arena size: the deepest page is the recordings list. Its widest form is the wide
/// layout's -- a window, the pinned back button, the two pinned paging buttons, the scrolling
/// strip, and the strip's eight rows: thirteen, plus a little headroom.
pub const UI_WIDGETS: usize = 15;
/// The backlight scale, `0..=MAX` per-mille.
pub const BACKLIGHT_LEVEL_MAX: u16 = 1000;
/// How many recordings the list page shows: the newest N, one fixed row each -- the page
/// tree is static, its TEXT is not.
pub const LIST_ROWS: usize = 8;

/// Widget tags: how a module finds a widget again to rewrite its text at runtime.
/// The status -- elapsed time, playing/ready -- rides the title bar now, and the
/// recording light is the title bar's own flashing indicator dot, so neither needs a tag.
pub const TAG_REC: u8 = 2;
pub const TAG_PLAY: u8 = 3;
/// The recordings list's paging buttons, shown only when a previous/next page exists.
pub const TAG_PREV: u8 = 4;
pub const TAG_NEXT: u8 = 5;
/// The recordings list's rows carry `TAG_ROW_BASE + row`.
pub const TAG_ROW_BASE: u8 = 0x10;

// --- the event bus ------------------------------------------------------------------------

/// Everything that happens in a dictaphone, as one type: the app's own events, plus the
/// board's extension `X` -- an RTC, a raw card probe, whatever the tangible board carries.
#[derive(Clone, Copy, Debug)]
pub enum Event<X: Copy> {
        Touch(TouchSample),
        Gesture(Gesture),
        Orientation(Orientation),
        Command(Command),
        Ui(UiAction),
        /// The audio module's state, published whenever it changes (which includes every
        /// elapsed second); the display renders it into the status line and button labels.
        Status(AudioStatus),
        /// A recordings-list row's text: the scan hands names to the display one row at a
        /// time, so neither module holds the other's data.
        RowText { row: u8, name: FsPath },
        /// Whether the recordings list has a previous/next page to page to, published after each
        /// scan or page turn; the display shows or hides the paging buttons to match.
        ListNav { prev: bool, next: bool },
        /// The board's own affair; the app carries it and looks away.
        Ext(X),
}

//   the contract a board's generic touch/IMU modules publish through (see light_input::BoardEvent):
// this engine's events ARE those a touch panel and IMU raise, plus the stats/drag-consumed signals
// those modules read
impl<X: Copy> light_input::BoardEvent for Event<X> {
        fn touch(sample: TouchSample) -> Self {
                Event::Touch(sample)
        }
        fn gesture(gesture: Gesture) -> Self {
                Event::Gesture(gesture)
        }
        fn orientation(orientation: Orientation) -> Self {
                Event::Orientation(orientation)
        }
        fn is_stats(&self) -> bool {
                matches!(self, Event::Command(Command::Stats))
        }
        fn drag_consumed(&self) -> bool {
                matches!(self, Event::Ui(UiAction::DragConsumed))
        }
}

#[derive(Clone, Copy, Debug)]
pub enum Command {
        Stats,
        Backlight(u16),
        UiFocus { next: bool },
        UiActivate,
        UiPress { x: u16, y: u16 },
        UiBack,
        RenderMode(RenderMode),
        Fs { op: FsOp, path: FsPath, arg: FsPath },
        /// A command for the recorder (light-recorder): record, play, tone, volume, mic.
        Audio(RecorderCommand),
}

/// The `fs` command's operation and path argument are the framework's: see [`light_fs::console`].
pub use light_fs::console::{FsOp, FsPath, USAGE as FS_USAGE};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderMode {
        Normal,
        Paused,
        Repush,
}

#[derive(Clone, Copy, Debug)]
pub enum UiAction {
        /// The big button: start a recording, or stop the one in flight.
        RecToggle,
        /// Play the most recent recording, or stop the playback in flight.
        PlayToggle,
        /// The recordings page was opened: scan the card and fill the rows.
        FilesOpen,
        /// A recordings-list row was tapped.
        PlayRow(u8),
        /// The recordings list's "next"/"previous" page buttons.
        FilesNext,
        FilesPrev,
        /// Return to the main page -- the recordings page's back button, on the data-driven
        /// (blob) interface. The const-page interface navigates back structurally instead, so it
        /// never emits this.
        Back,
        DragConsumed,
}

/// The blob-interface event ids: a design's button carries one in its `event` field, and the
/// display module maps it back to a [`UiAction`] with [`ui_action`]. Ids `1..16` are the fixed
/// actions; a recordings-list row `i` carries `ROW_BASE + i`. A JSON design cannot hold comments,
/// so this module is the readable side of that contract -- the numbers in `design.json` mean these.
pub mod ui_event {
        pub const REC_TOGGLE: u16 = 1;
        pub const PLAY_TOGGLE: u16 = 2;
        pub const FILES_OPEN: u16 = 3;
        pub const FILES_NEXT: u16 = 4;
        pub const FILES_PREV: u16 = 5;
        pub const BACK: u16 = 6;
        /// Row `i` carries `ROW_BASE + i` (and, by the same offset, tag `TAG_ROW_BASE + i`).
        pub const ROW_BASE: u16 = 0x10;
}

/// A blob child's `event` id, as the design authored it, to the [`UiAction`] the app runs.
/// Unknown ids (and the reserved 0) map to `None`, so a button with no event emits nothing.
pub fn ui_action(event: u16) -> Option<UiAction> {
        use ui_event::*;
        match event {
                REC_TOGGLE => Some(UiAction::RecToggle),
                PLAY_TOGGLE => Some(UiAction::PlayToggle),
                FILES_OPEN => Some(UiAction::FilesOpen),
                FILES_NEXT => Some(UiAction::FilesNext),
                FILES_PREV => Some(UiAction::FilesPrev),
                BACK => Some(UiAction::Back),
                e if e >= ROW_BASE && (e - ROW_BASE) < LIST_ROWS as u16 => Some(UiAction::PlayRow((e - ROW_BASE) as u8)),
                _ => None,
        }
}

/// The blob interface's page indices, fixed for the dictaphone's two pages -- the `design.json`
/// must order them this way (the main page first). The const-page interface follows parent links
/// and does not use these.
pub const PAGE_MAIN: usize = 0;
pub const PAGE_FILES: usize = 1;

/// Map a blob child to the app event its button emits: the design's `event` id through
/// [`ui_action`]. The child index is unused -- the id, not the position, carries the meaning.
fn map_ui_event<X: Copy>(_: usize, child: &LuiChild<'static>) -> Option<Event<X>> {
        ui_action(child.event).map(Event::Ui)
}

// --- shared state --------------------------------------------------------------------------

static CONSOLE_BYTES: Mailbox<u8, 128> = Mailbox::new();
/// The push-versus-touch bisect instruments: whether the panel is mid-push, and whether
/// touch reads should hold while it is.
static PUSHING: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
static TOUCH_HOLD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// A console byte from the transport the board module owns. Never blocks; a full mailbox
/// drops the byte, and the line it belonged to will fail to parse and say so.
pub fn push_console_byte(b: u8) {
        let _ = CONSOLE_BYTES.push(b);
}

/// Whether a board's touch module should skip this read: `touch hold` is on and the panel
/// is being pushed.
pub fn touch_reads_held() -> bool {
        TOUCH_HOLD.load(core::sync::atomic::Ordering::Relaxed) && PUSHING.load(core::sync::atomic::Ordering::Relaxed)
}

// --- storage -------------------------------------------------------------------------------

/// The card as the app sees it -- the framework's removable-storage seam.
pub use light_core::hal::Store;

// --- the display module --------------------------------------------------------------------

/// What a tangible board tells [`DisplayMod`] about its panel.
pub struct DisplayConfig {
        pub width: u16,
        pub height: u16,
        pub fps: u32,
        /// The panel, for the boot log.
        pub desc: &'static str,
        /// The rotation the interface starts in, before any orientation event: the UI
        /// crate's choice -- a portrait layout starts at R0, a landscape one sideways.
        pub initial_rotation: Rotation,
        /// The board's measured orientation-to-rotation table.
        pub rotation_map: fn(Orientation) -> Option<Rotation>,
        /// The interface as data: the parsed LUI design blob the widget tree is built from. Its
        /// `orientation` ([`Lui::landscape`]) decides the layout axis, so the design is the single
        /// source and the firmware states no axis of its own.
        pub source: Lui<'static>,
        /// The direction child pages open across this interface. `None` keeps the toolkit's
        /// layout-derived flow (a `Row` page rises, everything else slides left); a landscape
        /// interface points it one way for the whole tree. It is expressed logically, so it
        /// stays correct through the display rotation the orientation map applies.
        pub default_descent: Option<Descent>,
}

/// Owns the panel and the widget tree: renders when something is dirty, routes touches
/// and commands into the toolkit, follows the audio module's status into the interface's
/// labels, and keeps the draw/push timing `stats` reports.
pub struct DisplayMod<D: DisplayDriver, C: Clock, X: Copy + 'static> {
        display: Display<'static, D>,
        layer: &'static mut FrameLayer,
        font: Font<'static>,
        ui: &'static mut Ui<Event<X>, UI_WIDGETS>,
        clock: C,
        cfg: DisplayConfig,
        bus: &'static dyn Bus<Event<X>>,
        sub: Subscription,
        mode: RenderMode,
        /// The interface's current page index (see [`PAGE_MAIN`]/[`PAGE_FILES`]); the display drives
        /// navigation itself, keeping its own place since the blob carries no parent links.
        page: usize,
        /// What the title bar shows, and what gates the recording light.
        status: AudioStatus,
        /// The recording light's current phase, and when it last toggled: the flash runs
        /// off the display poll at 2 Hz, faster than the once-a-second status events.
        blink_on: bool,
        blink_last_us: u64,
        drag_reported: bool,
        draw_us_max: u64,
        push_us_max: u64,
        push_started_us: Option<u64>,
}

impl<D: DisplayDriver, C: Clock, X: Copy + core::fmt::Debug + 'static> DisplayMod<D, C, X> {
        pub fn new(
                display: Display<'static, D>,
                layer: &'static mut FrameLayer,
                font: Font<'static>,
                ui: &'static mut Ui<Event<X>, UI_WIDGETS>,
                clock: C,
                bus: &'static dyn Bus<Event<X>>,
                cfg: DisplayConfig,
        ) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { display, layer, font, ui, clock, cfg, bus, sub, mode: RenderMode::Normal, page: PAGE_MAIN, status: AudioStatus::Idle, blink_on: false, blink_last_us: 0, drag_reported: false, draw_us_max: 0, push_us_max: 0, push_started_us: None }
        }

        fn publish(bus: &dyn Bus<Event<X>>, ev: Option<Event<X>>) {
                if let Some(ev) = ev {
                        if let Err(e) = bus.publish(ev) {
                                warn!("event bus full; dropped {e:?}");
                        }
                }
        }

        fn handle(&mut self, ev: Event<X>) {
                match ev {
                        Event::Touch(t) => {
                                if self.mode == RenderMode::Repush {
                                        if let TouchSample::Down { .. } = t {
                                                //   a banded display has no frame to resend: the whole
                                                // tree repainted is the same traffic on the bus
                                                if self.display.band_rows().is_some() {
                                                        self.ui.invalidate_all();
                                                } else if !self.display.busy() {
                                                        let _ = self.display.update_async(Region::full(self.cfg.width, self.cfg.height));
                                                }
                                        }
                                }
                                let now = log::now_us();
                                let outcome = match t {
                                        TouchSample::Down { x, y } | TouchSample::Move { x, y } => self.ui.touch(x, y, true, now),
                                        TouchSample::Up => self.ui.touch(0, 0, false, now),
                                        TouchSample::Reset => return,
                                };
                                match outcome {
                                        Touch::Drag if !self.drag_reported => {
                                                self.drag_reported = true;
                                                Self::publish(self.bus, Some(Event::Ui(UiAction::DragConsumed)));
                                        }
                                        Touch::Tap { hit, emitted } => {
                                                debug!("tap: {}", if hit { "hit" } else { "no widget there" });
                                                //   run the whole press-flash BLIP synchronously here, before
                                                // the emitted action runs: paint and push the flash, hold it
                                                // for its short window, then paint and push the reverted
                                                // button. A record/play handler blocks core 0 on card I/O far
                                                // longer than the flash should last, so the loop cannot end
                                                // the flash on its own -- it would linger for the whole delay
                                                if emitted.is_some() {
                                                        self.flush_display();
                                                        self.render();
                                                        self.flush_display();
                                                        let until = now + light_ui::ACTIVATE_FLASH_US;
                                                        while log::now_us() < until {}
                                                        // a render past the deadline lapses the flash; push that frame too
                                                        self.render();
                                                        self.flush_display();
                                                }
                                                Self::publish(self.bus, emitted);
                                                //   navigation is the display's own on the blob path:
                                                // the emitted event still rides the bus (the recorder
                                                // scans on FilesOpen), but the page move happens here,
                                                // in step with the tap, not through a bus round-trip.
                                                // A no-op on the const-page path (that navigates
                                                // structurally from the button itself)
                                                if let Some(Event::Ui(action)) = emitted {
                                                        self.nav_for(action);
                                                }
                                        }
                                        Touch::DragEnd | Touch::None => self.drag_reported = false,
                                        _ => {}
                                }
                        }
                        Event::Gesture(g) => {
                                if self.ui.swipe_direction(g.start, g.end) == Some(SwipeDir::Right) && self.back() {
                                        debug!("swipe: returned to the previous page");
                                }
                        }
                        Event::Orientation(o) => {
                                if let Some(r) = (self.cfg.rotation_map)(o) {
                                        self.ui.set_rotation(self.layer, r);
                                        let (w, h) = self.ui.logical_size();
                                        info!("orientation {o:?}: canvas now {w}x{h}");
                                }
                        }
                        Event::Command(Command::UiFocus { next }) => {
                                if next {
                                        self.ui.focus_next()
                                } else {
                                        self.ui.focus_prev()
                                }
                        }
                        Event::Command(Command::UiActivate) => {
                                let emitted = self.ui.activate();
                                Self::publish(self.bus, emitted);
                                if let Some(Event::Ui(action)) = emitted {
                                        self.nav_for(action);
                                }
                        }
                        Event::Command(Command::UiPress { x, y }) => {
                                let (hit, emitted) = self.ui.press_at(x, y);
                                info!("ui press {x} {y}: {}", if hit { "hit" } else { "no widget there" });
                                Self::publish(self.bus, emitted);
                                if let Some(Event::Ui(action)) = emitted {
                                        self.nav_for(action);
                                }
                        }
                        Event::Command(Command::RenderMode(m)) => {
                                //   coming back to Normal, the glass is untrusted: a pause
                                // may have left stale content standing, so resume repaints
                                // everything rather than waiting for the next interaction
                                if m == RenderMode::Normal && self.mode != RenderMode::Normal {
                                        self.ui.invalidate_all();
                                }
                                self.mode = m;
                                info!("render mode {m:?}");
                        }
                        Event::Command(Command::UiBack) => {
                                if !self.back() {
                                        info!("ui back: nowhere to go from this page");
                                }
                        }
                        Event::Status(s) => {
                                self.status = s;
                                self.render_status();
                                self.set_transport_labels();
                        }
                        Event::RowText { row, name } => {
                                //   an unused row (an empty name, e.g. the tail of the last page) is
                                // hidden, not left as a blank cell; the ListNav that follows the row
                                // batch relays the list so the freed space closes up
                                self.ui.set_list_row(TAG_ROW_BASE, row, name.as_str());
                        }
                        Event::ListNav { prev, next } => {
                                //   the paging buttons appear only when there is a page that way;
                                // find() is None when the files page is not built -- the right
                                // no-op. Then relay: this event ends a list update (the row batch
                                // before it), so the hidden rows and paging buttons close up
                                if let Some(id) = self.ui.find(TAG_PREV) {
                                        self.ui.set_visible(id, prev);
                                }
                                if let Some(id) = self.ui.find(TAG_NEXT) {
                                        self.ui.set_visible(id, next);
                                }
                                self.ui.relayout();
                        }
                        Event::Command(Command::Stats) => {
                                info!(
                                        "display: {} frames, {} skipped, {} chunk timeouts; max draw {} us, max push {} us",
                                        self.layer.frames(),
                                        self.layer.skipped,
                                        self.display.timeouts,
                                        self.draw_us_max,
                                        self.push_us_max
                                );
                                self.draw_us_max = 0;
                                self.push_us_max = 0;
                        }
                        _ => {}
                }
        }

        /// The record/play button labels, following the audio module's state. `find` is `None`
        /// for a tag on a page that is not built (the recordings page has neither button), which
        /// is exactly the right no-op. Called on every status change and after a page is (re)built,
        /// so a page entered mid-recording shows `# Stop`, not the design's static `* Record`.
        fn set_transport_labels(&mut self) {
                let (rec_label, play_label) = match self.status {
                        AudioStatus::Idle => ("* Record", "Play last"),
                        AudioStatus::Recording { .. } => ("# Stop", "Play last"),
                        AudioStatus::Playing { .. } => ("* Record", "# Stop"),
                };
                if let Some(id) = self.ui.find(TAG_REC) {
                        self.ui.set_label(id, rec_label);
                }
                if let Some(id) = self.ui.find(TAG_PLAY) {
                        self.ui.set_label(id, play_label);
                }
        }

        /// Move to a page of the design with the transition ([`Ui::navigate_lui`]) and re-apply the
        /// live transport state onto it (a `# Stop` mid-recording, the title-bar time and
        /// indicator), which the design's static labels do not carry.
        fn show_lui_page(&mut self, page: usize, back: bool) {
                let lui = self.cfg.source;
                let Some(p) = lui.page(page) else {
                        warn!("dictaphone: the design has no page {page}");
                        return;
                };
                //   The transition is the target page's authored descent going forward; going back
                // we hand navigate_lui the page we are LEAVING so it mirrors that entry as an exit.
                let descent = if back {
                        lui.page(self.page).and_then(|from| from.descent())
                } else {
                        p.descent()
                };
                if let Err(e) = self.ui.navigate_lui(&p, back, descent, map_ui_event::<X>) {
                        warn!("dictaphone: design page {page} did not build: {e:?}");
                        return;
                }
                self.page = page;
                self.set_transport_labels();
                self.render_status();
        }

        /// Navigate for a tapped action: open the recordings list, or return from it.
        fn nav_for(&mut self, action: UiAction) {
                match action {
                        UiAction::FilesOpen => self.show_lui_page(PAGE_FILES, false),
                        UiAction::Back => {
                                if self.page != PAGE_MAIN {
                                        self.show_lui_page(PAGE_MAIN, true);
                                }
                        }
                        _ => {}
                }
        }

        /// Go back one page: return to the main page. `false`, changing nothing, when there is
        /// nowhere to go -- so a swipe or `UiBack` at the top means nothing there, as it did before.
        fn back(&mut self) -> bool {
                if self.page == PAGE_MAIN {
                        return false;
                }
                self.show_lui_page(PAGE_MAIN, true);
                true
        }

        /// The status, in the title bar: `"<page> - REC 0:12"` and the like, composed onto
        /// the current page's own name so it reads right whichever page is showing. The body
        /// carries only the flashing recording light now (see [`Self::blink_tick`]).
        fn render_status(&mut self) {
                let Some(root) = self.ui.root() else { return };
                //   the transport state shows as a drawn symbol -- a flashing dot recording, a
                // steady triangle playing -- with the time beside it. A two-row title bar (the
                // narrow portrait one) puts the name on row 1 with the symbol and the time on
                // row 2; a one-row bar (the wide one, with room) keeps it all inline.
                let (base, two_row) = self
                        .ui
                        .get(root)
                        .and_then(|w| w.window())
                        .map(|win| (win.title.unwrap_or(""), win.subtitle.is_some()))
                        .unwrap_or(("", false));
                let col = self.symbol_col(base);
                let mut detail = StackString::<{ TextSlot::CAP }>::new();
                let indicator = match self.status {
                        AudioStatus::Idle => None,
                        AudioStatus::Recording { secs, .. } => {
                                let _ = write!(detail, "{}:{:02}", secs / 60, secs % 60);
                                Some((IndicatorShape::Dot, self.blink_on, col))
                        }
                        AudioStatus::Playing { secs, total } => {
                                let _ = write!(detail, "{}:{:02} / {}:{:02}", secs / 60, secs % 60, total / 60, total % 60);
                                Some((IndicatorShape::Play, true, col))
                        }
                };
                if two_row {
                        // row 1 stays the static name; the time rides row 2 (blank when idle)
                        self.ui.set_subtitle(root, detail.as_str());
                } else {
                        // name, a blank cell for the symbol, then the inline time
                        let mut line = StackString::<{ TextSlot::CAP }>::new();
                        if detail.as_str().is_empty() {
                                let _ = write!(line, "{base}");
                        } else {
                                let _ = write!(line, "{base}   {}", detail.as_str());
                        }
                        self.ui.set_text(root, line.as_str());
                }
                self.ui.set_indicator(root, indicator);
                //   leaving the recording state resets the flash phase, so the next take
                // starts lit on its first tick
                if !matches!(self.status, AudioStatus::Recording { .. }) {
                        self.blink_on = false;
                        self.blink_last_us = 0;
                }
        }

        /// The title cell the status symbol sits on: one blank past the page's (static) name,
        /// so it tracks whichever page is showing and never overlaps the title.
        fn symbol_col(&self, base: &str) -> u16 {
                (base.chars().count() + 1) as u16
        }

        /// One step of the recording light's 2 Hz flash, driven from the display poll so it
        /// runs between the once-a-second status events. A no-op unless recording; toggles the
        /// inline title-bar dot between lit and dark at a fixed cell, so the title never
        /// reflows. Only the title bar's rect repaints.
        fn blink_tick(&mut self, now: u64) {
                if !matches!(self.status, AudioStatus::Recording { .. }) {
                        return;
                }
                if self.blink_last_us != 0 && now.saturating_sub(self.blink_last_us) < 500_000 {
                        return;
                }
                self.blink_last_us = now;
                self.blink_on = !self.blink_on;
                if let Some(root) = self.ui.root() {
                        let base = self.ui.get(root).and_then(|w| w.window()).and_then(|win| win.title).unwrap_or("");
                        let col = self.symbol_col(base);
                        self.ui.set_indicator(root, Some((IndicatorShape::Dot, self.blink_on, col)));
                }
        }

        fn render(&mut self) {
                if self.mode != RenderMode::Normal || (!self.ui.is_dirty() && !self.ui.is_animating()) {
                        return;
                }
                let now = log::now_us();
                //   one face for every role today; the theme is the UI's own, so the metrics it
                // laid out with and these glyphs are the one style
                let style = Style::new(*self.ui.theme(), Fonts::uniform(&self.font));
                let drew = self.ui.render(self.layer, &mut self.display, &style, now);
                let done = log::now_us();
                if drew || self.ui.is_animating() {
                        self.draw_us_max = self.draw_us_max.max(done - now);
                        self.push_started_us = Some(done);
                }
        }

        /// Drive an in-flight chunked push to the panel to completion, right now. The frame
        /// otherwise finishes over later polls; this is for the one case that cannot wait --
        /// painting a tap's acknowledgement before a blocking handler (a card open, a take
        /// starting) runs and stops the loop from polling. Bounded so a wedged transfer can
        /// never hang the app.
        fn flush_display(&mut self) {
                let mut spins = 0u32;
                while self.layer.busy(&self.display) {
                        if self.layer.poll(&mut self.display).is_err() {
                                break;
                        }
                        spins += 1;
                        if spins > 100_000 {
                                warn!("display flush did not settle");
                                break;
                        }
                }
        }
}

impl<D: DisplayDriver, C: Clock, X: Copy + core::fmt::Debug + 'static> Module for DisplayMod<D, C, X> {
        fn name(&self) -> &'static str {
                "display"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.display.init(&mut self.clock);
                self.layer.set_frame_rate(self.cfg.fps);
                self.layer.bg = self.ui.theme().bg;
                self.ui.fit(self.layer);
                if self.cfg.initial_rotation != Rotation::R0 {
                        self.ui.set_rotation(self.layer, self.cfg.initial_rotation);
                }
                self.ui.set_default_descent(self.cfg.default_descent);
                //   the layout axis is taken from the DESIGN (its orientation), so the design is the
                // single source and the editor and firmware cannot disagree about which way it lays out
                let axis = if self.cfg.source.landscape() { Axis::Horizontal } else { Axis::Vertical };
                self.ui.set_layout_axis(axis);
                //   the root is empty, so this first build snaps (navigate_lui starts no transition
                // until there is an outgoing page to slide off)
                self.show_lui_page(PAGE_MAIN, false);
                self.ui.invalidate_all();
                self.render();
                info!(
                        "display up: {}x{} ({}) at {} fps, font {}px ({} glyphs)",
                        self.cfg.width,
                        self.cfg.height,
                        self.cfg.desc,
                        self.cfg.fps,
                        self.font.pixel_size(),
                        self.font.glyph_count()
                );
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                match self.layer.poll(&mut self.display) {
                        Ok(_) => {}
                        Err(UpdateError::Timeout) => warn!("display chunk timed out; update abandoned"),
                        Err(UpdateError::Busy) => unreachable!(),
                }
                if let Some(started) = self.push_started_us {
                        if !self.layer.busy(&self.display) {
                                self.push_us_max = self.push_us_max.max(log::now_us() - started);
                                self.push_started_us = None;
                        }
                }
                while let Some(ev) = self.bus.poll(&self.sub) {
                        self.handle(ev);
                }
                self.blink_tick(log::now_us());
                self.render();
                let busy = self.layer.busy(&self.display);
                PUSHING.store(busy, core::sync::atomic::Ordering::Relaxed);
                if self.ui.is_dirty() || self.ui.is_animating() || busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                let _ = self.display.wait();
                info!("display down");
        }
}

// --- the recorder, and the dictaphone's own side of it ---------------------------------------

//   the recording/playback engine is the framework's light-recorder; the dictaphone reaches it the
// way any application does -- by the three functions below -- and re-exports what a board wires
pub use light_recorder::{wav_header, AudioStatus, Playback, Recorder, RecorderCommand, RecorderConfig, RecorderEvents, RecorderSlots, Recording, MAX_REC_SECS};

fn recorder_command<X: Copy>(e: &Event<X>) -> Option<RecorderCommand> {
        if let Event::Command(Command::Audio(c)) = e { Some(*c) } else { None }
}

fn is_stats<X: Copy>(e: &Event<X>) -> bool {
        matches!(e, Event::Command(Command::Stats))
}

/// How the recorder reaches this application's events: its commands ride
/// `Command::Audio`, and its status arrives as [`Event::Status`].
pub fn recorder_events<X: Copy + 'static>() -> RecorderEvents<Event<X>> {
        RecorderEvents { command_of: recorder_command::<X>, is_stats: is_stats::<X>, status_event: Event::<X>::Status }
}

/// `REC_NNNN.WAV` -> `NNNN`; anything else is not one of ours.
fn rec_number(name: &str) -> Option<u32> {
        let digits = name.strip_prefix("REC_")?.strip_suffix(".WAV")?;
        if digits.len() != 4 {
                return None;
        }
        digits.parse().ok()
}

/// The [`FilePicker`] filter for the recordings list: our `REC_NNNN.WAV` takes, and only ones
/// with PCM past the 44-byte header -- a header-only take has nothing to play, so it earns no
/// row. The board hands this to the picker at construction; the picker never learns what a
/// recording is.
pub fn keep_recording(entry: &DirEntry) -> bool {
        !entry.is_dir && entry.size > 44 && rec_number(entry.name()).is_some()
}

/// The dictaphone's side of the recorder: what its buttons mean, what a take is called, and the
/// recordings list. It drives the [`Recorder`] only by [`RecorderCommand`]s on the bus and learns
/// what the recorder is doing only from [`Event::Status`], so neither module holds the other's
/// state. It also serves the `fs` console command over the same card.
pub struct FilesMod<S: Store, X: Copy + 'static> {
        store: S,
        /// The recordings list, newest first: the system file-picker component, scanning the card
        /// and driving the list rows. In .bss like everything sizeable here.
        picker: &'static mut FilePicker<LIST_ROWS>,
        /// The most recent take -- what "Play last" plays.
        last_name: Option<FsPath>,
        /// The recorder's state, as last published.
        status: AudioStatus,
        bus: &'static dyn Bus<Event<X>>,
        sub: Subscription,
}

impl<S: Store, X: Copy + 'static> FilesMod<S, X> {
        pub fn new(store: S, picker: &'static mut FilePicker<LIST_ROWS>, bus: &'static dyn Bus<Event<X>>) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { store, picker, last_name: None, status: AudioStatus::Idle, bus, sub }
        }

        fn command(&self, c: RecorderCommand) {
                let _ = self.bus.publish(Event::Command(Command::Audio(c)));
        }

        /// Mount the card for a UI operation, or say why not.
        fn mount(&mut self) -> Option<Fat<S::Dev>> {
                let dev = self.store.open()?;
                match Fat::mount(dev) {
                        Ok(fs) => Some(fs),
                        Err(e) => {
                                info!("card: mount failed: {e:?}");
                                None
                        }
                }
        }

        /// The next free auto-name, `REC_NNNN.WAV`: one past the highest on the card.
        fn next_name(&mut self) -> Option<FsPath> {
                let mut fs = self.mount()?;
                let mut max = 0u32;
                let _ = fs.list_dir("/", |e| {
                        if let Some(n) = rec_number(e.name()) {
                                max = max.max(n);
                        }
                });
                let mut s = StackString::<16>::new();
                let _ = write!(s, "REC_{:04}.WAV", (max + 1) % 10000);
                FsPath::new(s.as_str())
        }

        /// Open the list page: scan the card fresh (first page) and publish it. The scan, the
        /// newest-first ordering, and the [`keep_recording`] filter are all the system
        /// [`FilePicker`]; this keeps the RowText/ListNav bridge to the display module.
        fn scan_files(&mut self) {
                match self.mount() {
                        Some(mut fs) => {
                                let _ = self.picker.scan(&mut fs, "/");
                        }
                        None => self.picker.clear(),
                }
                self.publish_list();
                if self.last_name.is_none() {
                        self.last_name = self.picker.name(0).and_then(FsPath::new);
                }
        }

        /// Page the list one step forward (`forward`) or back, then publish the new page.
        fn page_files(&mut self, forward: bool) {
                if let Some(mut fs) = self.mount() {
                        let _ = if forward { self.picker.next_page(&mut fs, "/") } else { self.picker.prev_page(&mut fs, "/") };
                }
                self.publish_list();
        }

        /// Publish the current page: a RowText per row (an empty name is an unused row), and the
        /// ListNav telling the display which paging buttons to show.
        fn publish_list(&mut self) {
                let empty = FsPath::EMPTY;
                for i in 0..LIST_ROWS {
                        let name = self.picker.name(i).and_then(FsPath::new).unwrap_or(empty);
                        let _ = self.bus.publish(Event::RowText { row: i as u8, name });
                }
                let _ = self.bus.publish(Event::ListNav { prev: self.picker.has_prev(), next: self.picker.has_next() });
        }

        /// "Play last" must name a take that is still there: the recorder discards an empty take
        /// by itself, so the name it last announced may be gone. A missing one falls back to the
        /// newest take on the card.
        fn resolve_last(&mut self) {
                if let Some(p) = self.last_name {
                        let present = self.mount().map(|mut fs| fs.stat(p.as_str()).is_ok()).unwrap_or(false);
                        if !present {
                                self.last_name = None;
                        }
                }
                if self.last_name.is_none() {
                        //   fresh boot, or the last take was discarded: the newest on the card is "last"
                        self.scan_files();
                }
        }
}

impl<S: Store, X: Copy + 'static> Module for FilesMod<S, X> {
        fn name(&self) -> &'static str {
                "files"
        }

        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = self.bus.poll(&self.sub) {
                        match ev {
                                Event::Status(s) => {
                                        if let AudioStatus::Recording { take, .. } = s {
                                                if !take.is_empty() {
                                                        self.last_name = Some(take);
                                                }
                                        }
                                        self.status = s;
                                }
                                Event::Ui(UiAction::RecToggle) => {
                                        busy = true;
                                        match self.status {
                                                AudioStatus::Recording { .. } => self.command(RecorderCommand::RecStop),
                                                status => {
                                                        if let AudioStatus::Playing { .. } = status {
                                                                self.command(RecorderCommand::PlayStop);
                                                        }
                                                        if let Some(name) = self.next_name() {
                                                                self.command(RecorderCommand::RecStart(name));
                                                        }
                                                }
                                        }
                                }
                                Event::Ui(UiAction::PlayToggle) => {
                                        busy = true;
                                        match self.status {
                                                AudioStatus::Playing { .. } => self.command(RecorderCommand::PlayStop),
                                                AudioStatus::Recording { .. } => info!("play: stop the recording first"),
                                                AudioStatus::Idle => {
                                                        self.resolve_last();
                                                        match self.last_name {
                                                                Some(p) => self.command(RecorderCommand::PlayStart(p)),
                                                                None => info!("play: nothing recorded yet"),
                                                        }
                                                }
                                        }
                                }
                                Event::Ui(UiAction::FilesOpen) => {
                                        busy = true;
                                        self.scan_files();
                                }
                                Event::Ui(UiAction::FilesNext) => {
                                        busy = true;
                                        self.page_files(true);
                                }
                                Event::Ui(UiAction::FilesPrev) => {
                                        busy = true;
                                        self.page_files(false);
                                }
                                Event::Ui(UiAction::PlayRow(i)) => {
                                        if let Some(p) = self.picker.name(usize::from(i)).and_then(FsPath::new) {
                                                match self.status {
                                                        AudioStatus::Recording { .. } => info!("play: stop the recording first"),
                                                        status => {
                                                                if let AudioStatus::Playing { .. } = status {
                                                                        self.command(RecorderCommand::PlayStop);
                                                                }
                                                                self.command(RecorderCommand::PlayStart(p));
                                                                self.last_name = Some(p);
                                                        }
                                                }
                                        }
                                }
                                Event::Command(Command::Fs { op, path, arg }) => {
                                        busy = true;
                                        light_fs::console::run(&mut self.store, op, path.as_str(), arg.as_str());
                                }
                                _ => {}
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}

// --- the console ---------------------------------------------------------------------------

/// The console: the shared CLI owns the grammar and the built-ins; the COMMAND TABLE is
/// the board's (this crate's rows plus whatever the board adds), so it lives as a static
/// in the board crate and arrives here by reference.
pub struct ConsoleMod<X: Copy + 'static> {
        reader: LineReader<96>,
        cli: &'static Cli<Event<X>>,
        bus: &'static dyn Bus<Event<X>>,
}

impl<X: Copy + core::fmt::Debug + 'static> ConsoleMod<X> {
        pub fn new(cli: &'static Cli<Event<X>>, bus: &'static dyn Bus<Event<X>>) -> Self {
                Self { reader: LineReader::new(), cli, bus }
        }

        fn dispatch(&mut self, line: &str) -> Poll {
                match self.cli.dispatch(line) {
                        Outcome::Quiet => Poll::Idle,
                        Outcome::Shutdown => Poll::Shutdown,
                        Outcome::Event(ev) => {
                                if let Event::Command(Command::Stats) = ev {
                                        info!("console: {} bytes dropped, {} lines dropped; bus: {} refused, {} backlog", CONSOLE_BYTES.dropped(), self.reader.dropped_lines, self.bus.refused(), self.bus.backlog());
                                }
                                if let Err(e) = self.bus.publish(ev) {
                                        warn!("event bus full; dropped {e:?}");
                                }
                                Poll::Busy
                        }
                        Outcome::Handled => Poll::Busy,
                }
        }
}

impl<X: Copy + core::fmt::Debug + 'static> Module for ConsoleMod<X> {
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

// --- the command parsers, for the board's command table ------------------------------------

pub fn parse_stats<X: Copy>(_w: &mut Words) -> Parsed<Event<X>> {
        Parsed::Event(Event::Command(Command::Stats))
}

/// A recorder command as this application's event.
fn audio<X: Copy>(c: RecorderCommand) -> Event<X> {
        Event::Command(Command::Audio(c))
}

pub fn parse_play<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        recorder_console::wrap(recorder_console::play(w), audio)
}

pub fn parse_rec<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        recorder_console::wrap(recorder_console::rec(w), audio)
}

pub fn parse_fs<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        light_fs::console::parse_command(w, |op, path, arg| Event::Command(Command::Fs { op, path, arg }))
}

pub fn parse_backlight<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next().and_then(|s| s.parse::<u16>().ok()) {
                Some(level) if level <= BACKLIGHT_LEVEL_MAX => Parsed::Event(Event::Command(Command::Backlight(level))),
                _ => Parsed::Usage,
        }
}

pub fn parse_ui<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match (w.next(), w.next(), w.next()) {
                (Some("focus"), Some("next"), _) => Parsed::Event(Event::Command(Command::UiFocus { next: true })),
                (Some("focus"), Some("prev"), _) => Parsed::Event(Event::Command(Command::UiFocus { next: false })),
                (Some("activate"), _, _) => Parsed::Event(Event::Command(Command::UiActivate)),
                (Some("press"), Some(x), Some(y)) => match (x.parse(), y.parse()) {
                        (Ok(x), Ok(y)) => Parsed::Event(Event::Command(Command::UiPress { x, y })),
                        _ => Parsed::Usage,
                },
                (Some("back"), _, _) => Parsed::Event(Event::Command(Command::UiBack)),
                _ => Parsed::Usage,
        }
}

pub fn parse_touch<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next() {
                Some("hold") => {
                        TOUCH_HOLD.store(true, core::sync::atomic::Ordering::Relaxed);
                        info!("touch: reads held while the panel is being pushed");
                        Parsed::Done
                }
                Some("free") => {
                        TOUCH_HOLD.store(false, core::sync::atomic::Ordering::Relaxed);
                        info!("touch: reads not held");
                        Parsed::Done
                }
                _ => Parsed::Usage,
        }
}

pub fn parse_render<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        match w.next() {
                Some("pause") => Parsed::Event(Event::Command(Command::RenderMode(RenderMode::Paused))),
                Some("resume") => Parsed::Event(Event::Command(Command::RenderMode(RenderMode::Normal))),
                Some("repush") => Parsed::Event(Event::Command(Command::RenderMode(RenderMode::Repush))),
                _ => Parsed::Usage,
        }
}

pub fn parse_tone<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        recorder_console::wrap(recorder_console::tone(w), audio)
}

pub fn parse_volume<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        recorder_console::wrap(recorder_console::volume(w), audio)
}

pub fn parse_micmon<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        recorder_console::wrap(recorder_console::micmon(w), audio)
}

pub fn parse_micgain<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        recorder_console::wrap(recorder_console::micgain(w), audio)
}

pub fn parse_micdbg<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        recorder_console::wrap(recorder_console::micdbg(w), audio)
}

pub fn parse_synth<X: Copy>(w: &mut Words) -> Parsed<Event<X>> {
        recorder_console::wrap(recorder_console::synth(w), audio)
}

//   the command-row type, for the dictaphone_commands! expansion
#[doc(hidden)]
pub use light_core::cli::Command as DictCliRow;

/// The command table: the rows this application carries, spelled once, plus the board's
/// own. Expands to an ARRAY, so take a reference for the CLI's slice.
#[macro_export]
macro_rules! dictaphone_commands {
        ($ext:ty $(; $($extra:expr),* $(,)?)?) => {
                [
                        $crate::DictCliRow { name: "stats", usage: "stats", parse: $crate::parse_stats::<$ext> },
                        $crate::DictCliRow { name: "tone", usage: $crate::recorder_console::TONE_USAGE, parse: $crate::parse_tone::<$ext> },
                        $crate::DictCliRow { name: "volume", usage: $crate::recorder_console::VOLUME_USAGE, parse: $crate::parse_volume::<$ext> },
                        $crate::DictCliRow { name: "fs", usage: $crate::FS_USAGE, parse: $crate::parse_fs::<$ext> },
                        $crate::DictCliRow { name: "rec", usage: $crate::recorder_console::REC_USAGE, parse: $crate::parse_rec::<$ext> },
                        $crate::DictCliRow { name: "play", usage: $crate::recorder_console::PLAY_USAGE, parse: $crate::parse_play::<$ext> },
                        $crate::DictCliRow { name: "micmon", usage: $crate::recorder_console::MICMON_USAGE, parse: $crate::parse_micmon::<$ext> },
                        $crate::DictCliRow { name: "micdbg", usage: "micdbg", parse: $crate::parse_micdbg::<$ext> },
                        $crate::DictCliRow { name: "synth", usage: "synth", parse: $crate::parse_synth::<$ext> },
                        $crate::DictCliRow { name: "micgain", usage: $crate::recorder_console::MICGAIN_USAGE, parse: $crate::parse_micgain::<$ext> },
                        $crate::DictCliRow { name: "backlight", usage: "backlight 0..1000", parse: $crate::parse_backlight::<$ext> },
                        $crate::DictCliRow { name: "ui", usage: "ui focus next|prev | ui activate | ui press X Y | ui back", parse: $crate::parse_ui::<$ext> },
                        $crate::DictCliRow { name: "touch", usage: "touch hold|free", parse: $crate::parse_touch::<$ext> },
                        $crate::DictCliRow { name: "render", usage: "render pause|resume|repush", parse: $crate::parse_render::<$ext> },
                        $($($extra),*)?
                ]
        };
}

/// A fixed-capacity string for formatting a line without an allocator.
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
