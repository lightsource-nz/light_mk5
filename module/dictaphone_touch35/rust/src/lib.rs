//! The dictaphone on the Waveshare RP2350-Touch-LCD-3.5: the `light_dictaphone_core` engine with the
//! portrait `light_app_dictaphone` interface, over this board's ES8311 codec, microphone, speaker
//! amplifier and TF slot. This crate is the tangible 3.5 -- the ST7796 on SPI0, the FT6336, the IMU,
//! the RTC and the AXP2101 on the one shared I2C bus, the PIO I2S transport, the card -- plus the
//! shell entry points.

#![no_std]

use core::cell::RefCell;
use light_dictaphone_core as dict;
use dict::{dictaphone_commands, keep_recording, AudioSlots, AudioStatus, Command, DisplayConfig, DisplayMod, Event, FilePicker, Order};
use light_audio::Es8311;
use light_board_touch35::{board, Pmic, Touch35Power};
use board::*;
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{info, log, warn, ConstStaticCell, CoreSlot, EventBus, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::st7796::St7796;
use light_display::{Display, FrameLayer};
use light_draw::{PixelFormat, Rotation};
use light_font::Font;
use light_input::drivers::ft6336::Ft6336;
use light_input::drivers::qmi8658::Qmi8658;
use light_input::imu::{Imu, Orientation};
use light_input::touch::Tracker;
use light_input::{ImuMod, TouchMod};
use light_power_manager::PowerMod;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::I2c1;
use light_rp2::i2s::I2sStream;
use light_rp2::shell::{core1_ticks, panic_report, stack_free, stack_paint, ShellInfo};
use light_rp2::spi::Spi0Display;
use light_rp2::spi_bus::Spi1Bus;
use light_rp2::{Breathe, Clocks, SysClock};
use light_rtc::{Datetime, Pcf85063a, RtcMod};
use light_sd::SpiSd;
use light_ui::{Fonts, Lui, Style, Theme, Ui};

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);

/// One frame buffer, 300 KB of the RP2350's 520: region buffering keeps the page slide without a
/// second (see the widget demo on this board).
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));
static UI_BLOB: &[u8] = include_bytes!(env!("LIGHT_UI_LUI"));

// --- the event bus --------------------------------------------------------------------------

/// This board's extension events -- the RTC and the raw card probe -- riding the app's bus.
#[derive(Clone, Copy, Debug)]
enum Ext {
        RtcShow,
        RtcSet(Datetime),
        /// Probe the card from CMD0 and read block 0: the layer below `fs`.
        Sd,
}

type AppEvent = Event<Ext>;

//   the recognizers the framework power module reads this app's events through: a backlight
// command carries a level; `stats` asks for the battery report; and audio in flight (a non-idle
// status) defers the on-battery power-off
fn power_backlight(e: &AppEvent) -> Option<u16> {
        if let Event::Command(Command::Backlight(level)) = e {
                Some(*level)
        } else {
                None
        }
}
fn power_is_stats(e: &AppEvent) -> bool {
        matches!(e, Event::Command(Command::Stats))
}
fn power_busy(e: &AppEvent) -> Option<bool> {
        if let Event::Status(s) = e {
                Some(!matches!(s, AudioStatus::Idle))
        } else {
                None
        }
}

//   the recognizers the framework RTC module reads this app's events through
fn rtc_is_report(e: &AppEvent) -> bool {
        matches!(e, Event::Command(Command::Stats) | Event::Ext(Ext::RtcShow))
}
fn rtc_get_set(e: &AppEvent) -> Option<Datetime> {
        if let Event::Ext(Ext::RtcSet(t)) = e {
                Some(*t)
        } else {
                None
        }
}

static EVENTS: EventBus<AppEvent, 16, 8> = EventBus::new();

const FPS: u32 = 30;

// --- storage: the TF slot as the app's Store -----------------------------------------------

type Card = SpiSd<Spi1Bus, Output>;
type SdRef = light_sd::SharedCard<'static, Spi1Bus, Output>;
type SdStore = light_sd::CardStore<Spi1Bus, Output, SysClock>;

//   the recorder's card-borrowing state machines live in .bss, not on core 0's stack
static REC_SLOT: CoreSlot<dict::Recording<SdRef>> = CoreSlot::new();
static PLAY_SLOT: CoreSlot<dict::Playback<SdRef>> = CoreSlot::new();

/// Portrait only: the interface is a portrait design, so the board turns end-for-end and otherwise
/// holds still.
fn rotation_map(o: Orientation) -> Option<Rotation> {
        match o {
                Orientation::Portrait => Some(Rotation::R0),
                Orientation::PortraitFlip => Some(Rotation::R180),
                _ => None,
        }
}

// --- the board's own modules ---------------------------------------------------------------

//   the storage-and-diagnostics module: the on-demand card probe, and the core-1 and stack
// diagnostics on `stats`
struct BoardMod {
        card: &'static RefCell<Card>,
        events: Subscription,
}

impl Module for BoardMod {
        fn name(&self) -> &'static str {
                "board"
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = EVENTS.poll(&self.events) {
                        match ev {
                                AppEvent::Command(Command::Stats) => {
                                        info!("cores: core1 ticks {}, core0 stack low-water {} B above the runway base", core1_ticks(), stack_free());
                                }
                                AppEvent::Ext(Ext::Sd) => {
                                        busy = true;
                                        light_sd::shared::report(&mut self.card.borrow_mut(), &mut SysClock);
                                }
                                _ => {}
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}

// --- the console table ---------------------------------------------------------------------

fn parse_rtc(w: &mut Words) -> Parsed<AppEvent> {
        light_rtc::command::parse_command(w, Event::Ext(Ext::RtcShow), |t| Event::Ext(Ext::RtcSet(t)))
}

static COMMANDS: &[CliCommand<AppEvent>] = &dictaphone_commands![Ext;
        CliCommand { name: "rtc", usage: light_rtc::command::USAGE, parse: parse_rtc },
        CliCommand { name: "sd", usage: "sd", parse: |_| Parsed::Event(Event::Ext(Ext::Sd)) },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

// --- entry ----------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        stack_paint();
        log::set_clock(light_rp2::now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, peri {} Hz; spi0 at {} Hz, i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.display_bus.actual_hz, p.i2c.actual_hz);

        let front: &'static mut [u8] = FRAME_FRONT.take();
        let mut display = Display::new(St7796::new(p.display_bus, DISPLAY_MADCTL, DISPLAY_INVERT), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        display.set_region_buffering(true);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        //   the one bus every device on this board shares
        static I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let i2c: &'static RefCell<I2c1> = I2C.init(RefCell::new(p.i2c));
        let touch = Ft6336::new(i2c, p.touch_int, p.touch_reset, TOUCH_MAP, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(i2c));
        let mut pmic = Pmic::new(i2c);
        match pmic.probe().and_then(|found| if found { pmic.init().map(|()| true) } else { Ok(false) }) {
                Ok(true) => info!("pmic: AXP2101 up"),
                Ok(false) => warn!("pmic: chip id does not match an AXP2101"),
                Err(e) => warn!("pmic: not answering ({e:?})"),
        }

        //   the one card, shared: the board module's probe and the engine's recorder and fs
        // commands all borrow it per operation
        static CARD: StaticCell<RefCell<Card>> = StaticCell::new();
        let card: &'static RefCell<Card> = CARD.init(RefCell::new(SpiSd::new(p.sd_spi, p.sd_cs)));
        let mut power_mod = PowerMod::new(
                Touch35Power { backlight: p.backlight, pmic, power_key: p.power_key },
                SysClock,
                &EVENTS,
                power_backlight,
                power_is_stats,
                power_busy,
        );
        let mut board_mod = BoardMod { card, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut imu_mod = ImuMod::new(imu, &EVENTS, SysClock, IMU_AXIS_MAP);
        let mut rtc_mod = RtcMod::new(Pcf85063a::new(i2c), &EVENTS, rtc_is_report, rtc_get_set);

        //   a small frame-aligned read scratch for the playback path; the stall-riding depth is the
        // transport's IRQ-drained prefetch ring
        static STAGE: ConstStaticCell<[u8; 2048]> = ConstStaticCell::new([0; 2048]);
        static PICKER: ConstStaticCell<FilePicker<{ dict::LIST_ROWS }>> = ConstStaticCell::new(FilePicker::new(Order::NameDescending, keep_recording));
        let mut audio_mod = dict::AudioMod::new(
                Es8311::new(i2c),
                I2sStream::new(p.i2s),
                p.amp_en,
                SysClock,
                SdStore { card, clock: SysClock },
                AUDIO_SAMPLE_HZ,
                //   the codec's mic gain as tuned on the 3.49's microphone: a starting point for this
                // board's, to be measured on it
                (0x17, 0xE3),
                AudioSlots {
                        // SAFETY: the one take of each slot, on the application core
                        rec: unsafe { REC_SLOT.take() },
                        play: unsafe { PLAY_SLOT.take() },
                        play_stage: STAGE.take(),
                        picker: PICKER.take(),
                },
                &EVENTS,
        );

        static LAYER: ConstStaticCell<FrameLayer> = ConstStaticCell::new(FrameLayer::new(DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565));
        static UI: ConstStaticCell<Ui<AppEvent, { dict::UI_WIDGETS }>> = ConstStaticCell::new(Ui::new());
        let layer: &'static mut FrameLayer = LAYER.take();
        let ui: &'static mut Ui<AppEvent, { dict::UI_WIDGETS }> = UI.take();
        let theme = match Theme::parse(THEME_BLOB) {
                Ok(t) => t,
                Err(e) => panic!("the embedded theme does not parse: {e:?}"),
        };
        layer.bg = theme.bg;
        ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
        let lui = match Lui::parse(UI_BLOB) {
                Ok(l) => l,
                Err(e) => panic!("the embedded UI design does not parse: {e:?}"),
        };
        type BoardDisplayMod = DisplayMod<St7796<Spi0Display>, SysClock, Ext>;
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
                        rotation_map,
                        initial_rotation: Rotation::R0,
                        source: lui,
                        default_descent: None,
                },
        ));
        static TOUCH_MOD: StaticCell<TouchMod<AppEvent, Ft6336<&'static RefCell<I2c1>, Input, Output>, SysClock>> = StaticCell::new();
        let touch_mod = TOUCH_MOD.init(TouchMod::new(touch, Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), &EVENTS, SysClock, dict::touch_reads_held));
        let mut console_mod = dict::ConsoleMod::new(&CLI, &EVENTS);

        let mut rt: Runtime<8> = Runtime::new();
        rt.add(&mut power_mod).expect("capacity");
        rt.add(&mut board_mod).expect("capacity");
        rt.add(display_mod).expect("capacity");
        rt.add(touch_mod).expect("capacity");
        rt.add(&mut imu_mod).expect("capacity");
        rt.add(&mut rtc_mod).expect("capacity");
        rt.add(&mut audio_mod).expect("capacity");
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

//   core 1 is the port's console loop over the USB CDC, feeding the engine's console mailbox; the
// shell's default UART pins are camera lines on this board
#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_main(_info: &ShellInfo) -> ! {
        light_rp2::shell::core1_main(dict::push_console_byte, None)
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        panic_report(info)
}
