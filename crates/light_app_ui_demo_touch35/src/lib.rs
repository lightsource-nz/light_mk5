//! The touch35 firmware: the widget demo on the Waveshare RP2350-Touch-LCD-3.5 -- the first
//! board with the ST7796S panel and the FT6336U touch controller, whose bring-up this firmware
//! exists to run. The application is `light_app_ui_demo`, with no hardware in it; this crate is
//! the tangible 3.5: the ST7796 over SPI0, the FT6336, the IMU, the shell ABI and the panic
//! handler.
//!
//! Bring-up status: the panel shows true colours upright (MADCTL BGR, inversion on), the
//! FT6336 answers and taps land under the finger, and the SPI clock runs at 75 MHz (a full
//! repush peaks near 41 ms), and the IMU axis map is measured -- all confirmed on the hardware.

#![no_std]

use core::cell::RefCell;
use light_app_ui_demo as demo;
use demo::{demo_commands, BoardHook, Command, DemoEvent, DemoView, DisplayConfig, DisplayMod, RenderMode};
use light_camera::Ov5640;
use light_core::hal::CameraBus;
use light_core::{Clock, SpiDisplayBus};
use light_rp2::camera::PioCamera;
use light_rp2::pwm::PwmOutput;
use light_board_touch35::{board, Pmic, Touch35Power};
use board::*;
use light_input::drivers::ft6336::Ft6336;
use light_input::drivers::qmi8658::Qmi8658;
use light_input::imu::{Imu, Orientation};
use light_input::touch::Tracker;
use light_input::{ImuMod, TouchMod};
use light_display::st7796::St7796;
use light_display::{Display, FrameLayer};
use light_ui::{Fonts, Lui, Style, Theme, Ui};
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{info, log, warn, ConstStaticCell, EventBus, Module, Poll, Runtime, StaticCell, Subscription};
use light_power_manager::PowerMod;
use light_rtc::{Datetime, Pcf85063a, RtcMod};
use light_fs::console::{FsOp, FsPath};
use light_sd::{CardStore, SpiSd};
use light_rp2::spi_bus::Spi1Bus;
use light_draw::{PixelFormat, Rotation};
use light_font::Font;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::I2c1;
use light_rp2::spi::Spi0Display;
use light_rp2::shell::{panic_report, ShellInfo};
use light_rp2::{Breathe, Clocks, SysClock};

const BAND_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_BAND_ROWS);

/// No frame buffer: the ST7796 keeps its own frame, so the display is BANDED -- two band buffers
/// (one drawn while the other is pushed), 40 KB in place of a 300 KB frame. The page slide is a
/// cover (the incoming page slides in over the outgoing, which stays on the panel); rotation
/// snaps.
static BAND_A: ConstStaticCell<[u8; BAND_BYTES]> = ConstStaticCell::new([0; BAND_BYTES]);
static BAND_B: ConstStaticCell<[u8; BAND_BYTES]> = ConstStaticCell::new([0; BAND_BYTES]);

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
/// The look-and-feel: the framework's steel theme, the default for every board with
/// color support.
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));
/// The demo interface, authored as data: light_app_ui_demo's shared design with this board's
/// title and size overlaid, compiled to an LUI blob.
static UI_BLOB: &[u8] = include_bytes!(env!("LIGHT_UI_LUI"));

// --- the event bus --------------------------------------------------------------------------

/// This board's extension events, riding the demo's bus.
#[derive(Clone, Copy, Debug)]
enum Ext {
        /// Re-clock the display bus live: the SPI-headroom probe. Too fast shows up as
        /// corrupt pixels rather than a clean failure, so the eye is the instrument.
        SpiHz(u32),
        /// The PMIC readout: presence, charge state, rails, gauge, and the power key.
        Pmic,
        /// Report the real-time clock.
        RtcShow,
        /// Set the real-time clock.
        RtcSet(Datetime),
        /// Bring the TF card up and report it.
        Sd,
        /// One filesystem command against the TF card.
        Fs { op: FsOp, path: FsPath, arg: FsPath },
        /// The camera preview.
        Cam(CamOp),
        /// Recover the panel controller, for a panel that has stopped showing what it is sent: `true`
        /// is the full reset and init, `false` only sleep-out and display-on. Which one brings the
        /// picture back tells a controller that was reset from one that was put to sleep.
        Panel { full: bool },
}

#[derive(Clone, Copy, Debug)]
enum CamOp {
        /// Power the sensor up and read its chip id.
        Id,
        /// The live preview, with the sensor's colour bars in place of the image or not.
        Live { bars: bool },
        /// Stop the preview, power the sensor down, and give the panel back to the interface.
        Off,
        /// Sample the sync lines and report where the capture is parked.
        Probe,
        /// Read a frame's first words straight from the capture FIFO, bypassing the DMA.
        Raw,
}

type AppEvent = DemoEvent<Ext>;

//   the recognizers the framework power module reads this app's events through
fn power_backlight(e: &AppEvent) -> Option<u16> {
        if let DemoEvent::Command(Command::Backlight(level)) = e {
                Some(*level)
        } else {
                None
        }
}
fn power_is_stats(e: &AppEvent) -> bool {
        matches!(e, DemoEvent::Command(Command::Stats))
}

//   the recognizers the framework RTC module reads this app's events through: report the clock on
// `stats` or `rtc`, set it on `rtc set`
fn rtc_is_report(e: &AppEvent) -> bool {
        matches!(e, DemoEvent::Command(Command::Stats) | DemoEvent::Ext(Ext::RtcShow))
}
fn rtc_get_set(e: &AppEvent) -> Option<Datetime> {
        if let DemoEvent::Ext(Ext::RtcSet(t)) = e {
                Some(*t)
        } else {
                None
        }
}

static EVENTS: EventBus<AppEvent, 16, 8> = EventBus::new();

// --- core 1 --------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_main(_info: &ShellInfo) -> ! {
        //   USB only: the shell's default UART pins (GPIO 0/1) are camera data lines on this
        // board, and its header UART is on 45/46
        light_rp2::shell::core1_main(demo::push_console_byte, None)
}

// --- the interface, as data ---------------------------------------------------------------

const FPS: u32 = 30;
const BACKLIGHT_DIM: u16 = BACKLIGHT_LEVEL_MAX / 10;

/// The fleet's table, over the measured axis map.
fn rotation_map(o: Orientation) -> Option<Rotation> {
        match o {
                Orientation::Portrait => Some(Rotation::R0),
                Orientation::PortraitFlip => Some(Rotation::R180),
                Orientation::LandscapeL => Some(Rotation::R270),
                Orientation::LandscapeR => Some(Rotation::R90),
                _ => None,
        }
}

// --- the camera preview ---------------------------------------------------------------------

const CAMERA_FRAME_BYTES: usize = CAMERA_WIDTH as usize * CAMERA_HEIGHT as usize * 2;
/// The panel row the console preview's top lands on: centred.
const CAMERA_Y0: u16 = (DISPLAY_HEIGHT - CAMERA_HEIGHT) / 2;
/// The camera page, and the tag of the empty area on it the preview fills -- both this board's
/// design facts (module/ui_demo_touch35's design.json appends the page).
const CAMERA_PAGE: usize = 4;
const CAMERA_VIEW_TAG: u8 = 200;
/// A frame that has not landed in this long is reported once: the sensor is not streaming, or
/// the capture is not seeing it.
const CAMERA_STALL_US: u64 = 500_000;

/// The two camera frames: one being captured into while the other is pushed to the panel, so the
/// push overlaps the next capture and the preview keeps up with every sensor frame. Word-aligned:
/// the capture lands them in word writes.
#[repr(C, align(4))]
struct CameraFrame([u8; CAMERA_FRAME_BYTES]);
static CAMERA_FRAME_A: ConstStaticCell<CameraFrame> = ConstStaticCell::new(CameraFrame([0; CAMERA_FRAME_BYTES]));
static CAMERA_FRAME_B: ConstStaticCell<CameraFrame> = ConstStaticCell::new(CameraFrame([0; CAMERA_FRAME_BYTES]));

/// The OV5640 on its pixel bus, previewed straight to the panel: when a frame lands, the next is
/// armed into the other buffer at once and the landed one is pushed while it captures.
///
/// Two ways in. The camera PAGE previews into the empty area its design leaves for it, with the
/// interface drawing around it as usual: it starts once the page's transition has finished and
/// stops the moment the page is left. The CONSOLE (`cam on`/`bars`) previews centred on whatever
/// is showing, with the interface's rendering paused, and repaints it in full at `cam off`.
struct Camera {
        sensor: Ov5640<&'static RefCell<I2c1>>,
        bus: PioCamera,
        xclk: PwmOutput,
        pwdn: Output,
        frames: [&'static mut [u8; CAMERA_FRAME_BYTES]; 2],
        /// The frame the capture is writing (or last wrote); the other is the panel's.
        capturing: usize,
        powered: bool,
        configured: bool,
        live: bool,
        /// The panel row the running preview's top is at.
        y0: u16,
        /// The running preview paused the interface (the console's), so stopping resumes it.
        paused_ui: bool,
        /// The camera page is showing, so a preview belongs on it.
        on_page: bool,
        armed_us: u64,
        stall_reported: bool,
        /// Frames shown since the last `stats`, and when that count started.
        shown: u32,
        shown_since_us: u64,
}

impl Camera {
        fn power_up(&mut self) {
                if self.powered {
                        return;
                }
                //   the clock first: the sensor's SCCB needs it running to answer at all
                self.xclk.set_duty(CAMERA_XCLK_TOP / 2);
                self.pwdn.set(false);
                SysClock.delay_ms(30);
                self.powered = true;
        }

        fn power_down(&mut self) {
                self.pwdn.set(true);
                self.xclk.set_duty(0);
                self.powered = false;
                self.configured = false;
        }

        fn arm(&mut self) {
                // SAFETY: the capturing buffer is not touched again until frame_complete answers
                // true; only the other one is pushed meanwhile
                unsafe { self.bus.start_frame(&mut self.frames[self.capturing][..]) };
                self.armed_us = light_rp2::now_us();
                self.stall_reported = false;
        }

        fn report_probe(&self) {
                let p = self.bus.probe(20_000);
                info!(
                        "cam probe: vsync {}/{}, href {}/{}, pclk {}/{} high; data in-line OR {:#04x} AND {:#04x}; sm pc {}, {} words left, {}",
                        p.vsync_highs,
                        p.samples,
                        p.href_highs,
                        p.samples,
                        p.pclk_highs,
                        p.samples,
                        p.data_or,
                        p.data_and,
                        p.pc,
                        p.words_left,
                        if p.armed { "armed" } else { "idle" }
                );
                //   what the buffer holds: how much of it is not zero, and the first pixels of the
                // top, middle and bottom rows
                let frame = &self.frames[self.capturing];
                info!("cam dma: ctrl {:#010x}, write addr {:#010x} (frame at {:#010x}), abort {:#x}", p.dma_ctrl, p.dma_write_addr, frame.as_ptr() as u32, p.dma_abort);
                let nonzero = frame.iter().filter(|b| **b != 0).count();
                info!("cam frame: {nonzero}/{} bytes nonzero", frame.len());
                let stride = CAMERA_WIDTH as usize * 2;
                for row in [0usize, CAMERA_HEIGHT as usize / 2, CAMERA_HEIGHT as usize - 1] {
                        let r = &frame[row * stride..row * stride + 16];
                        info!("cam frame row {row}: {:02x?}", r);
                }
        }

        fn command(&mut self, view: &mut DemoView<'_, St7796<Spi0Display>, Ext>, op: CamOp) {
                match op {
                        CamOp::Id => {
                                self.power_up();
                                match self.sensor.chip_id() {
                                        Ok(id) => info!("cam: chip id {id:#06x}{}", if id == light_camera::ov5640::CHIP_ID { " (OV5640)" } else { " -- not an OV5640" }),
                                        Err(e) => warn!("cam: not answering ({e:?})"),
                                }
                        }
                        CamOp::Live { bars } => {
                                if !self.configure(bars) {
                                        return;
                                }
                                if !self.live {
                                        //   the interface stops drawing; what it last drew stays around
                                        // the preview
                                        self.start(view, CAMERA_Y0, true);
                                }
                                info!("cam: live{}", if bars { ", colour bars" } else { "" });
                        }
                        CamOp::Off => {
                                //   off means off, on the camera page too, until it is next opened
                                self.on_page = false;
                                self.stop(view);
                                info!("cam: off");
                        }
                        CamOp::Probe => self.report_probe(),
                        CamOp::Raw => {
                                let was_live = self.live;
                                let mut words = [0u32; 8];
                                let n = self.bus.capture_raw(&mut words, 200_000, light_rp2::now_us);
                                info!("cam raw: {n} words: {:08x?}", &words[..n]);
                                if was_live {
                                        self.arm();
                                }
                        }
                }
        }

        /// Power the sensor up and configure it if it is not already, then set the test pattern.
        /// Blocking the first time (about 370 ms). `false` when the sensor would not come up.
        fn configure(&mut self, bars: bool) -> bool {
                self.power_up();
                if !self.configured {
                        let t0 = light_rp2::now_us();
                        if let Err(e) = self.sensor.init(&mut SysClock, CAMERA_WIDTH, CAMERA_HEIGHT, CAMERA_PLL) {
                                warn!("cam: init failed ({e:?})");
                                return false;
                        }
                        if let Err(e) = self.sensor.set_orientation(CAMERA_MIRROR, CAMERA_FLIP) {
                                warn!("cam: orientation not set ({e:?})");
                        }
                        self.configured = true;
                        info!("cam: OV5640 configured {}x{} RGB565 in {} ms", CAMERA_WIDTH, CAMERA_HEIGHT, (light_rp2::now_us() - t0) / 1000);
                }
                if let Err(e) = self.sensor.set_test_pattern(bars) {
                        warn!("cam: test pattern not set ({e:?})");
                }
                true
        }

        /// Start previewing with the top at panel row `y0`, pausing the interface or not.
        fn start(&mut self, view: &mut DemoView<'_, St7796<Spi0Display>, Ext>, y0: u16, pause_ui: bool) {
                let _ = view.display.wait();
                if pause_ui {
                        *view.mode = RenderMode::Paused;
                }
                self.paused_ui = pause_ui;
                self.y0 = y0.min(DISPLAY_HEIGHT - CAMERA_HEIGHT);
                self.live = true;
                self.shown = 0;
                self.shown_since_us = light_rp2::now_us();
                self.arm();
        }

        /// Stop the preview and power the sensor down; resume the interface if the preview paused it.
        fn stop(&mut self, view: &mut DemoView<'_, St7796<Spi0Display>, Ext>) {
                self.bus.stop();
                self.power_down();
                if self.live {
                        self.live = false;
                        if self.paused_ui {
                                *view.mode = RenderMode::Normal;
                                view.ui.invalidate_all();
                        }
                }
        }

        /// The interface changed page. Leaving the camera page stops its preview at once, so the
        /// transition away covers the last frame rather than racing the next one.
        fn page_changed(&mut self, view: &mut DemoView<'_, St7796<Spi0Display>, Ext>, page: usize) {
                self.on_page = page == CAMERA_PAGE;
                if !self.on_page && self.live && !self.paused_ui {
                        self.stop(view);
                        info!("cam: page preview stopped");
                }
        }

        /// The camera page's preview area, in panel rows: the top of the design's empty view widget.
        fn page_view_y0(view: &DemoView<'_, St7796<Spi0Display>, Ext>) -> Option<u16> {
                let id = view.ui.find(CAMERA_VIEW_TAG)?;
                let r = view.ui.get(id)?.rect;
                u16::try_from(r.y0).ok()
        }

        /// One preview step: a landed frame goes to the panel and the next is armed. On the camera
        /// page, the preview starts here once the page has finished arriving.
        fn poll(&mut self, view: &mut DemoView<'_, St7796<Spi0Display>, Ext>) -> bool {
                if self.on_page && !self.live && !view.ui.is_animating() {
                        let Some(y0) = Self::page_view_y0(view) else {
                                warn!("cam: the camera page has no view area (tag {CAMERA_VIEW_TAG})");
                                self.on_page = false;
                                return false;
                        };
                        if !self.configure(false) {
                                self.on_page = false;
                                return false;
                        }
                        self.start(view, y0, false);
                        info!("cam: page preview at row {}", self.y0);
                }
                if !self.live {
                        return false;
                }
                if !self.bus.frame_complete() {
                        if !self.stall_reported && light_rp2::now_us() - self.armed_us > CAMERA_STALL_US {
                                self.stall_reported = true;
                                warn!("cam: no frame in {} ms", CAMERA_STALL_US / 1000);
                                self.report_probe();
                        }
                        return true;
                }
                //   the next capture first, so it is armed before the sensor's next frame starts --
                // the push below outlasts the vertical blanking, and arming after it would miss
                // that frame
                let landed = self.capturing;
                self.capturing ^= 1;
                self.arm();
                if let Err(e) = view.display.push_rows_blocking(&self.frames[landed][..], self.y0, CAMERA_HEIGHT) {
                        warn!("cam: push failed ({e:?})");
                }
                self.shown += 1;
                true
        }

        fn stats(&mut self) {
                let now = light_rp2::now_us();
                let ms = (now - self.shown_since_us) / 1000;
                if self.live && ms > 0 {
                        let centi_fps = u64::from(self.shown) * 100_000 / ms;
                        info!("cam: {} frames in {} ms, {}.{:02} fps; {} captured since boot", self.shown, ms, centi_fps / 100, centi_fps % 100, self.bus.frames);
                } else {
                        info!("cam: {}; {} frames captured since boot", if self.live { "live" } else { "off" }, self.bus.frames);
                }
                self.shown = 0;
                self.shown_since_us = now;
        }
}

// --- the driver-specific edges -------------------------------------------------------------

struct Hook {
        camera: Camera,
}

impl BoardHook<St7796<Spi0Display>, Ext> for Hook {
        fn after_init(&mut self, display: &mut Display<'static, St7796<Spi0Display>>, bg: u16) {
                display.driver().clear(bg);
        }
        fn on_unload(&mut self, display: &mut Display<'static, St7796<Spi0Display>>, bg: u16) {
                display.driver().clear(bg);
        }
        fn on_ext(&mut self, view: &mut DemoView<'_, St7796<Spi0Display>, Ext>, ext: Ext) {
                match ext {
                        Ext::SpiHz(hz) => {
                                //   never mid-push: a divider change under a DMA burst
                                // tears the transfer
                                let _ = view.display.wait();
                                let actual = view.display.driver().bus_mut().set_baudrate(hz);
                                info!("spi re-clocked: asked {hz} Hz, running {actual} Hz");
                                //   repaint everything at the new clock, so corruption
                                // shows immediately rather than on the next interaction
                                view.ui.invalidate_all();
                        }
                        Ext::Cam(op) => self.camera.command(view, op),
                        Ext::Panel { full } => {
                                let _ = view.display.wait();
                                let t0 = light_rp2::now_us();
                                if full {
                                        view.display.init(&mut SysClock);
                                        view.display.driver().clear(view.ui.theme().bg);
                                } else {
                                        let bus = view.display.driver().bus_mut();
                                        bus.command(light_display::st7796::CMD_SLPOUT);
                                        SysClock.delay_ms(120);
                                        bus.command(light_display::st7796::CMD_DISPON);
                                }
                                view.ui.invalidate_all();
                                info!("panel: {} in {} ms; the whole interface repaints", if full { "reset and initialised" } else { "woken (sleep out, display on)" }, (light_rp2::now_us() - t0) / 1000);
                        }
                        Ext::Pmic | Ext::RtcShow | Ext::RtcSet(_) | Ext::Sd | Ext::Fs { .. } => {}
                }
        }
        fn on_poll(&mut self, view: &mut DemoView<'_, St7796<Spi0Display>, Ext>) -> bool {
                self.camera.poll(view)
        }
        fn on_page(&mut self, view: &mut DemoView<'_, St7796<Spi0Display>, Ext>, page: usize) {
                self.camera.page_changed(view, page);
        }
        fn on_stats(&mut self) {
                self.camera.stats();
        }
}

// --- the board's own modules ---------------------------------------------------------------

/// How often the PMIC's latched power-key events are collected: a person's press, not a signal.
const KEY_POLL_MS: u64 = 100;

//   the PMIC diagnostics: the `pmic` readout, and a log line per power-key event the PMIC latched
// (the key itself is the power module's button, read from its GPIO)
struct PmicMod {
        pmic: Pmic,
        events: Subscription,
        next_key_poll_us: u64,
}

impl PmicMod {
        fn report(&mut self) {
                match self.pmic.status() {
                        Ok(s) => info!("pmic: vbus {}, battery {}, charge {:?}", if s.vbus_good { "good" } else { "absent" }, if s.battery_present { "present" } else { "absent" }, s.charge),
                        Err(e) => {
                                warn!("pmic: not answering ({e:?})");
                                return;
                        }
                }
                let vbat = self.pmic.battery_mv().ok().flatten();
                let pct = self.pmic.battery_percent().ok().flatten();
                let vbus = self.pmic.vbus_mv().unwrap_or(0);
                let vsys = self.pmic.vsys_mv().unwrap_or(0);
                let t = self.pmic.die_decidegrees().unwrap_or(0);
                info!("pmic: vbat {:?} mV ({:?}%), vbus {} mV, vsys {} mV, die {}.{} C", vbat, pct, vbus, vsys, t / 10, (t % 10).abs());
        }
}

impl Module for PmicMod {
        fn name(&self) -> &'static str {
                "pmic"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::Ext(Ext::Pmic) = ev {
                                busy = true;
                                self.report();
                        }
                }
                let now = light_rp2::now_us();
                if now >= self.next_key_poll_us {
                        self.next_key_poll_us = now + KEY_POLL_MS * 1000;
                        if let Ok(k) = self.pmic.take_key_events() {
                                if k.any() {
                                        busy = true;
                                        info!(
                                                "pmic: power key{}{}{}{}",
                                                if k.pressed() { " pressed" } else { "" },
                                                if k.released() { " released" } else { "" },
                                                if k.short() { " short" } else { "" },
                                                if k.long() { " long" } else { "" }
                                        );
                                }
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}

/// The TF slot's card, shared between the console commands that use it.
type Card = SpiSd<Spi1Bus, Output>;

//   the TF slot: probed on demand (the `sd` command), not at boot -- an empty slot is ordinary
struct StorageMod {
        card: &'static RefCell<Card>,
        events: Subscription,
}

impl Module for StorageMod {
        fn name(&self) -> &'static str {
                "storage"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Ext(Ext::Sd) => {
                                        busy = true;
                                        light_sd::shared::report(&mut self.card.borrow_mut(), &mut SysClock);
                                }
                                AppEvent::Ext(Ext::Fs { op, path, arg }) => {
                                        busy = true;
                                        let mut store = CardStore { card: self.card, clock: SysClock };
                                        light_fs::console::run(&mut store, op, path.as_str(), arg.as_str());
                                }
                                _ => {}
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}

// --- the console table ---------------------------------------------------------------------

fn parse_spi(w: &mut Words) -> Parsed<AppEvent> {
        match w.next().and_then(|s| s.parse::<u32>().ok()) {
                //   the achievable rates at clk_peri 150 MHz are coarse (75, 37.5, 25...);
                // the driver reports what it actually got
                Some(hz) if (1_000_000..=100_000_000).contains(&hz) => Parsed::Event(DemoEvent::Ext(Ext::SpiHz(hz))),
                _ => Parsed::Usage,
        }
}

static COMMANDS: &[CliCommand<AppEvent>] = &demo_commands![Ext;
        CliCommand { name: "spi", usage: "spi HZ (1000000..100000000; the headroom probe)", parse: parse_spi },
        CliCommand { name: "pmic", usage: "pmic", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Pmic)) },
        CliCommand { name: "rtc", usage: light_rtc::command::USAGE, parse: parse_rtc },
        CliCommand { name: "sd", usage: "sd", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Sd)) },
        CliCommand { name: "fs", usage: light_fs::console::USAGE, parse: parse_fs },
        CliCommand { name: "cam", usage: "cam id|on|bars|off|probe|raw", parse: parse_cam },
        CliCommand { name: "panel", usage: "panel wake|init", parse: parse_panel },
];

fn parse_panel(w: &mut Words) -> Parsed<AppEvent> {
        match w.next() {
                Some("wake") => Parsed::Event(DemoEvent::Ext(Ext::Panel { full: false })),
                Some("init") => Parsed::Event(DemoEvent::Ext(Ext::Panel { full: true })),
                _ => Parsed::Usage,
        }
}

fn parse_cam(w: &mut Words) -> Parsed<AppEvent> {
        let op = match w.next() {
                Some("id") => CamOp::Id,
                Some("on") => CamOp::Live { bars: false },
                Some("bars") => CamOp::Live { bars: true },
                Some("off") => CamOp::Off,
                Some("probe") => CamOp::Probe,
                Some("raw") => CamOp::Raw,
                _ => return Parsed::Usage,
        };
        Parsed::Event(DemoEvent::Ext(Ext::Cam(op)))
}

fn parse_fs(w: &mut Words) -> Parsed<AppEvent> {
        light_fs::console::parse_command(w, |op, path, arg| DemoEvent::Ext(Ext::Fs { op, path, arg }))
}

fn parse_rtc(w: &mut Words) -> Parsed<AppEvent> {
        light_rtc::command::parse_command(w, DemoEvent::Ext(Ext::RtcShow), |t| DemoEvent::Ext(Ext::RtcSet(t)))
}
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

// --- entry ----------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        log::set_clock(light_rp2::now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, peri {} Hz; spi0 at {} Hz, i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.display_bus.actual_hz, p.i2c.actual_hz);

        let display = Display::new_banded(
                St7796::new(p.display_bus, DISPLAY_MADCTL, DISPLAY_INVERT),
                BAND_A.take(),
                BAND_B.take(),
                DISPLAY_WIDTH,
                DISPLAY_HEIGHT,
                DISPLAY_BAND_ROWS,
                PixelFormat::Rgb565,
                light_rp2::now_us,
        );
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        static I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let i2c: &'static RefCell<I2c1> = I2C.init(RefCell::new(p.i2c));
        let touch = Ft6336::new(i2c, p.touch_int, p.touch_reset, TOUCH_MAP, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(i2c));
        //   the amplifier stays off and the I2S idles: audio on this board is the dictaphone's
        let _amp_en = p.amp_en;
        let _i2s = p.i2s;
        let mut pmic = Pmic::new(i2c);
        match pmic.probe().and_then(|found| if found { pmic.init().map(|()| true) } else { Ok(false) }) {
                Ok(true) => info!("pmic: AXP2101 up"),
                Ok(false) => warn!("pmic: chip id does not match an AXP2101"),
                Err(e) => warn!("pmic: not answering ({e:?})"),
        }

        let mut power_mod = PowerMod::new(Touch35Power { backlight: p.backlight, pmic, power_key: p.power_key }, SysClock, &EVENTS, power_backlight, power_is_stats, |_| None);
        let mut rtc_mod = RtcMod::new(Pcf85063a::new(i2c), &EVENTS, rtc_is_report, rtc_get_set);
        static CARD: StaticCell<RefCell<Card>> = StaticCell::new();
        let card: &'static RefCell<Card> = CARD.init(RefCell::new(SpiSd::new(p.sd_spi, p.sd_cs)));
        let mut storage_mod = StorageMod { card, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut pmic_mod = PmicMod { pmic, events: EVENTS.subscribe().expect("subscriber slot"), next_key_poll_us: 0 };
        let mut imu_mod = ImuMod::new(imu, &EVENTS, SysClock, IMU_AXIS_MAP);
        //   powered down until a `cam` command wants it
        let camera = Camera {
                sensor: Ov5640::new(i2c),
                bus: p.camera,
                xclk: p.cam_xclk,
                pwdn: p.cam_pwdn,
                frames: [&mut CAMERA_FRAME_A.take().0, &mut CAMERA_FRAME_B.take().0],
                capturing: 0,
                powered: false,
                configured: false,
                live: false,
                y0: 0,
                paused_ui: false,
                on_page: false,
                armed_us: 0,
                stall_reported: false,
                shown: 0,
                shown_since_us: 0,
        };
        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565));
        static UI: ConstStaticCell<Ui<AppEvent, { demo::UI_WIDGETS }>> = ConstStaticCell::new(Ui::new());
        let layer: &'static mut FrameLayer = LAYER.take();
        let ui: &'static mut Ui<AppEvent, { demo::UI_WIDGETS }> = UI.take();
        //   the look-and-feel, from the embedded blob: a bad blob is a build-system bug
        // worth halting on, not styling to guess past
        let theme = match Theme::parse(THEME_BLOB) {
                Ok(t) => t,
                Err(e) => panic!("the embedded theme does not parse: {e:?}"),
        };
        layer.bg = theme.bg;
        ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
        let lui = match Lui::parse(UI_BLOB) {
                Ok(l) => l,
                Err(e) => panic!("the embedded UI blob does not parse: {e:?}"),
        };
        type BoardDisplayMod = DisplayMod<St7796<Spi0Display>, SysClock, Ext, Hook>;
        static DISPLAY_MOD: StaticCell<BoardDisplayMod> = StaticCell::new();
        let display_mod = DISPLAY_MOD.init(DisplayMod::new(
                display,
                layer,
                font,
                ui,
                SysClock,
                &EVENTS,
                DisplayConfig {
                        width: DISPLAY_WIDTH,
                        height: DISPLAY_HEIGHT,
                        fps: FPS,
                        desc: "ST7796 over SPI, banded (32-row bands)",
                        repush: true,
                        draw_over: false,
                        rotation_map,
                        source: lui,
                        backlight_dim: BACKLIGHT_DIM,
                },
                Hook { camera },
        ));
        static TOUCH_MOD: StaticCell<TouchMod<AppEvent, Ft6336<&'static RefCell<I2c1>, Input, Output>, SysClock>> = StaticCell::new();
        let touch_mod = TOUCH_MOD.init(TouchMod::new(touch, Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), &EVENTS, SysClock, demo::touch_reads_held));
        let mut console_mod = demo::ConsoleMod::new(&CLI, &EVENTS);

        let mut rt: Runtime<8> = Runtime::new();
        rt.add(&mut power_mod).expect("capacity");
        rt.add(&mut pmic_mod).expect("capacity");
        rt.add(&mut rtc_mod).expect("capacity");
        rt.add(&mut storage_mod).expect("capacity");
        rt.add(display_mod).expect("capacity");
        rt.add(touch_mod).expect("capacity");
        rt.add(&mut imu_mod).expect("capacity");
        rt.add(&mut console_mod).expect("capacity");
        rt.start().expect("start");
        info!("runtime started; type 'help' on the console");
        let mut idle = Breathe;
        let result = rt.run(|| light_core::Idle::idle(&mut idle));
        match result {
                Ok(()) => info!("runtime stopped cleanly; core 0 idle"),
                Err(e) => warn!("runtime stopped with {e:?}; core 0 idle"),
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
