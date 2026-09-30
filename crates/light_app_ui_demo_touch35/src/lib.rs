//! The touch35 firmware: the widget demo on the Waveshare RP2350-Touch-LCD-3.5 -- the first
//! board with the ST7796S panel and the FT6336U touch controller, whose bring-up this firmware
//! exists to run. The application is `light_app_ui_demo`, with no hardware in it; this crate is
//! the tangible 3.5: the ST7796 over SPI0, the FT6336, the IMU, the shell ABI and the panic
//! handler.
//!
//! Bring-up status: the panel shows true colours upright (MADCTL BGR, inversion on), the
//! FT6336 answers and taps land under the finger, and the SPI clock runs at 75 MHz (a full
//! repush peaks near 41 ms) -- all confirmed on the hardware. Still open: the IMU axis map is
//! IDENTITY until the three-observation calibration is done, so orientation changes will likely
//! be WRONG -- that is the measurement, not a bug.

#![no_std]

use core::cell::RefCell;
use light_app_ui_demo as demo;
use demo::{demo_commands, BoardHook, Command, DemoEvent, DemoView, DisplayConfig, DisplayMod};
use light_board_touch35::{board, Touch35Power};
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
use light_core::{info, log, warn, ConstStaticCell, EventBus, Runtime, StaticCell};
use light_power_manager::PowerMod;
use light_draw::{PixelFormat, Rotation};
use light_font::Font;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::I2c1;
use light_rp2::spi::Spi0Display;
use light_rp2::shell::{panic_report, ShellInfo};
use light_rp2::{Breathe, Clocks, SysClock};

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);

/// One frame buffer, 300 KB of the RP2350's 520 -- a second would not fit. Region (partial)
/// buffering keeps the page-slide transition without it: the outgoing image is scrolled off this
/// buffer in place while the incoming is painted into the strip it uncovers (see
/// [`Display::set_region_buffering`] and the toolkit's page step). Rotation snaps rather than
/// animating -- the one animation a single buffer cannot carry.
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

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

static EVENTS: EventBus<AppEvent, 16, 5> = EventBus::new();

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

/// The fleet's table; the axis map is the UNMEASURED identity, so until the calibration
/// session these rotations are the thing under test, not a fact.
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
                }
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
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

// --- entry ----------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        log::set_clock(light_rp2::now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, peri {} Hz; spi0 at {} Hz, i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.display_bus.actual_hz, p.i2c.actual_hz);

        let front: &'static mut [u8] = FRAME_FRONT.take();
        let mut display = Display::new(St7796::new(p.display_bus, DISPLAY_MADCTL, DISPLAY_INVERT), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        //   region buffering: the page slide runs on this one buffer -- reveal off on open, cover
        // back on close
        display.set_region_buffering(true);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        static I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let i2c: &'static RefCell<I2c1> = I2C.init(RefCell::new(p.i2c));
        let touch = Ft6336::new(i2c, p.touch_int, p.touch_reset, TOUCH_MAP, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(i2c));
        //   the amplifier stays off: nothing here drives audio
        let _amp_en = p.amp_en;

        let mut power_mod = PowerMod::new(Touch35Power { backlight: p.backlight }, SysClock, &EVENTS, power_backlight, power_is_stats, |_| None);
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
                        desc: "ST7796 over SPI, single-buffered (region)",
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

        let mut rt: Runtime<5> = Runtime::new();
        rt.add(&mut power_mod).expect("capacity");
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
