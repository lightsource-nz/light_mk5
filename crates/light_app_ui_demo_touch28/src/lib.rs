//! The touch28 firmware: the widget demo on the Waveshare RP2350-Touch-LCD-2.8 -- the
//! first board with the CST328 touch controller, whose bring-up this firmware exists to
//! run. The application is `light_app_ui_demo`, with no hardware in it; this crate is the
//! tangible 2.8: the wiring (`board.rs`), the ST7789 over SPI, the CST328, the IMU, the
//! shell ABI and the panic handler.
//!
//! Bring-up checklist (this board's support was authored without hardware): the CST328's
//! 0xCACA probe answers; coordinates track a finger; the IMU axis map is IDENTITY until the
//! three-observation calibration is done, so orientation changes will likely be WRONG at
//! first -- that is the measurement, not a bug; the SPI clock is 40 MHz with reported
//! headroom to 62.5; the `touch`/`render` console instruments are carried for the same
//! wedge-hunting they did on the 1.69.

#![no_std]

use core::cell::RefCell;
use light_app_ui_demo as demo;
use demo::{demo_commands, BoardHook, Command, DemoEvent, DemoView, DisplayConfig, DisplayMod, UiSource};
use light_board_touch28::{board, Touch28Power};
use board::*;
use light_input::drivers::cst328::Cst328;
use light_input::drivers::qmi8658::Qmi8658;
use light_input::imu::{Imu, Orientation};
use light_input::touch::Tracker;
use light_input::{ImuMod, TouchMod};
use light_display::st7789::St7789;
use light_display::{Display, FrameLayer};
use light_ui::{Fonts, Lui, Style, Theme, Ui};
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{info, log, warn, ConstStaticCell, EventBus, Module, Poll, Runtime, StaticCell, Subscription};
use light_power_manager::PowerManager;
use light_draw::{PixelFormat, Rotation};
use light_font::Font;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::I2c1;
use light_rp2::spi::Spi1Display;
use light_rp2::shell::{panic_report, service_core1, ShellInfo};
use light_rp2::{Breathe, Clocks, SysClock};

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);

/// Two frame buffers, 150 KB each, in .bss: the panel is pushed from one while the next
/// frame is drawn into the other.
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);
static FRAME_BACK: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
/// The look-and-feel: the framework's steel theme, the default for every board with
/// color support. A board-specific override would be a local theme file extending it.
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));
/// The demo interface, authored as data: light_app_ui_demo's shared design with this board's
/// title and size overlaid, compiled to an LUI blob. The demo core builds its tree from this instead
/// of a const page tree -- the UI-as-data path the dictaphone runs on.
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

static EVENTS: EventBus<AppEvent, 16, 5> = EventBus::new();

// --- core 1 --------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_service() {
        service_core1(demo::push_console_byte);
}

// --- the interface, as data ---------------------------------------------------------------

//   Square glass on this board: product photos show no rounding worth declaring, so the
// theme's screen_radius keeps its default of 0. TO BE CONFIRMED: if the glass clips
// corner content, measure the radius the way the 1.69's was measured and set the
// `screen_radius` metric in a local theme file extending "default" (see the 1.69's).
const FPS: u32 = 30;
const BACKLIGHT_DIM: u16 = BACKLIGHT_LEVEL_MAX / 10;

//   the widget tree is no longer a const page tree here: this board runs the demo from its LUI
// design blob (UI_BLOB). The title and size demo_pages! once baked in are the design's now -- the
// shared design this board's design.json extends.

/// The same table as the 1.69's -- but the axis map is the UNMEASURED identity, so until
/// the calibration session these rotations are the thing under test, not a fact.
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

impl BoardHook<St7789<Spi1Display>, Ext> for Hook {
        fn after_init(&mut self, display: &mut Display<'static, St7789<Spi1Display>>, bg: u16) {
                //   no set_offset: 240x320 is the ST7789's full GDDRAM, so the power-on
                // (0,0) is already correct -- the 1.69's row offset of 20 is a fact about
                // its 240x280 window, not about the driver
                display.driver().clear(bg);
        }
        fn on_unload(&mut self, display: &mut Display<'static, St7789<Spi1Display>>, bg: u16) {
                display.driver().clear(bg);
        }
        fn on_ext(&mut self, view: &mut DemoView<'_, St7789<Spi1Display>, Ext>, ext: Ext) {
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

// --- the board's own modules ---------------------------------------------------------------


struct BoardMod {
        power: PowerManager<Touch28Power, SysClock>,
        events: Subscription,
}

impl Module for BoardMod {
        fn name(&self) -> &'static str {
                "board"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.power.on_load();
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        if let AppEvent::Command(Command::Backlight(level)) = ev {
                                busy = true;
                                self.power.set_backlight(level);
                                info!("backlight {level}");
                        }
                }
                //   the shared power policy: dim on idle, power off on battery (touch/IMU/key feed
                // the activity beacon it watches), and the key-hold shutdown
                if let Poll::Shutdown = self.power.tick() {
                        return Poll::Shutdown;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.power.on_unload();
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
        info!("clocks: sys {} Hz, peri {} Hz; spi1 at {} Hz, i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.display_bus.actual_hz, p.touch_bus.actual_hz);

        let front: &'static mut [u8] = FRAME_FRONT.take();
        let back: &'static mut [u8] = FRAME_BACK.take();
        let mut display = Display::new(St7789::new(p.display_bus), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        display.set_back_buffer(back);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        static I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let i2c: &'static RefCell<I2c1> = I2C.init(RefCell::new(p.touch_bus));
        let touch = Cst328::new(i2c, p.touch_int, p.touch_reset, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(i2c));

        let power = PowerManager::new(Touch28Power { backlight: p.backlight, bat_en: p.bat_en, key_bat: p.key_bat }, SysClock);
        let mut board_mod = BoardMod { power, events: EVENTS.subscribe().expect("subscriber slot") };
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
        //   the interface as data: the demo core builds its tree from this blob and navigates off the
        // design's goto/back. A bad blob is a build-system bug worth halting on.
        let lui = match Lui::parse(UI_BLOB) {
                Ok(l) => l,
                Err(e) => panic!("the embedded UI blob does not parse: {e:?}"),
        };
        type BoardDisplayMod = DisplayMod<St7789<Spi1Display>, SysClock, Ext, Hook>;
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
                        desc: "ST7789 over SPI, double-buffered",
                        repush: true,
                        draw_over: false,
                        rotation_map,
                        source: UiSource::Blob(lui),
                        backlight_dim: BACKLIGHT_DIM,
                },
                Hook,
        ));
        static TOUCH_MOD: StaticCell<TouchMod<AppEvent, Cst328<&'static RefCell<I2c1>, Input, Output>, SysClock>> = StaticCell::new();
        let touch_mod = TOUCH_MOD.init(TouchMod::new(touch, Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), &EVENTS, SysClock, demo::touch_reads_held));
        let mut console_mod = demo::ConsoleMod::new(&CLI, &EVENTS);

        let mut rt: Runtime<5> = Runtime::new();
        rt.add(&mut board_mod).expect("capacity");
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
