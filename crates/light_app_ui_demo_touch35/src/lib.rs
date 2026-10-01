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
use demo::{demo_commands, BoardHook, Command, DemoEvent, DemoView, DisplayConfig, DisplayMod};
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

// --- the driver-specific edges -------------------------------------------------------------

struct Hook;

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
                        Ext::Pmic | Ext::RtcShow | Ext::RtcSet(_) | Ext::Sd | Ext::Fs { .. } => {}
                }
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
];

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
                Hook,
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
