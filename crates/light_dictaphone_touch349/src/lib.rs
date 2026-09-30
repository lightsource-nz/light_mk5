//! The dictaphone's shared board wiring for the Waveshare RP2350-Touch-LCD-3.49 -- a 172x640 QSPI
//! bar of glass with the ES8311 codec, an analog microphone and a TF slot. Both dictaphone
//! executables (the portrait `dictaphone_touch349` and the landscape `dictaphone_wide_touch349`)
//! build against this: the AXS15231B panel and its touch half, the QMI8658 IMU, the PCF85063A RTC,
//! the PIO I2S transport and its buffers, the card and the battery latch, the console table, and the
//! runtime that binds them to the `light_dictaphone_core` engine -- all identical between the two.
//! [`run`] is the whole firmware; each executable passes only its orientation [`RunConfig`] (the
//! rotation map, the starting rotation and page-transition flow) and its embedded blobs.

#![no_std]

use core::cell::RefCell;
use light_dictaphone_core as dict;
use dict::{dictaphone_commands, keep_recording, AudioSlots, AudioStatus, Command, DisplayConfig, DisplayMod, Event, FilePicker, Order};
use light_input::drivers::axs15231b::Axs15231bTouch;
use light_input::imu::{Imu, Orientation};
use light_input::drivers::qmi8658::Qmi8658;
use light_display::axs15231b::Axs15231b;
use light_input::touch::Tracker;
use light_ui::{Fonts, Lui, Style, Theme, Ui};
use light_core::cli::{Cli, Command as CliCommand, Parsed, Words};
use light_core::{info, log, warn, ConstStaticCell, CoreSlot, EventBus, Module, Poll, Runtime, StaticCell, Subscription};
use light_display::{Display, FrameLayer};
use light_draw::{PixelFormat, Rotation};
use light_audio::Es8311;
use light_font::Font;
use light_rtc::{Datetime, Pcf85063a, RtcMod};
use light_sd::SpiSd;
use light_board_touch349::{board, Touch349Power};
use light_power_manager::PowerMod;
use light_input::{ImuMod, TouchMod};
use light_rp2::shell::{core1_ticks, stack_free, stack_paint};
//   re-exported for the thin executables that link this crate: they hold the #[panic_handler] and
// the core-1 entry point, and take the shell's info -- all from the port's shell module. Core 1
// is the console loop; this board has no UART console (GPIO 0/1 carry the audio lines), so it
// runs on the USB CDC alone
pub use light_rp2::shell::{panic_report, ShellInfo};

/// Core 1's whole life, for a thin executable's `light_app_core1_main`: the port's console loop
/// over the USB CDC, feeding the engine's console mailbox. Never returns.
pub fn core1_console(_info: &ShellInfo) -> ! {
        light_rp2::shell::core1_main(push_console_byte, None)
}
use board::*;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::{I2c0, I2c1};
use light_rp2::i2s::I2sStream;
use light_rp2::spi_bus::Spi1Bus;
use light_rp2::qspi::PioQspiDisplayBus;
use light_rp2::{Breathe, Clocks, SysClock};

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);

/// One frame buffer, 215 KB of the RP2350's 520. Region (partial) buffering keeps the page-slide
/// transition without a second full frame: the outgoing image is scrolled off this buffer in place
/// while the incoming is painted into the strip it uncovers (see [`Display::set_region_buffering`]).
/// Rotation snaps rather than animating -- the one animation a single buffer cannot carry.
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

//   each executable pumps console bytes into the core's mailbox from its own core-1 service, and the
// landscape one names a `Descent` for its config; the re-exports save both a direct
// light_dictaphone_core dependency
pub use dict::{push_console_byte, Descent};

/// What one dictaphone executable hands [`run`]: its embedded blobs and the few things the two
/// orientations differ by. Everything else -- the wiring, the modules, the runtime -- is shared.
pub struct RunConfig {
        /// The font, theme and UI-design blobs, `include_bytes!`'d in the executable (their paths are
        /// per-build env vars the shared crate cannot see).
        pub font: &'static [u8],
        pub theme: &'static [u8],
        pub ui: &'static [u8],
        /// This orientation's IMU-pose-to-display-rotation map.
        pub rotation_map: fn(Orientation) -> Option<Rotation>,
        /// The rotation the interface boots in, before the first IMU report.
        pub initial_rotation: Rotation,
        /// The page-transition flow, or `None` for the toolkit's layout-derived default.
        pub default_descent: Option<Descent>,
        /// The panel description for the boot log.
        pub desc: &'static str,
}

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

//   the recognizers the framework power module (light_power_manager::PowerMod) reads this app's
// events through: a backlight command carries a level; `stats` asks for the battery report (this
// board has a gauge); and audio in flight (a non-idle status) defers the on-battery power-off.
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

//   the recognizers the framework RTC module (light_rtc::RtcMod) reads this app's events through:
// report the clock on `stats` or `rtc show`, and set it on `rtc set` (both in this board's Ext).
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

//   Bar glass, corners unmeasured: the theme's screen_radius keeps its default of 0
// until the glass says otherwise.
const FPS: u32 = 30;

// --- storage: the TF slot as the app's Store -----------------------------------------------

/// The TF slot as a shareable block device: the board's one card, borrowed per block operation.
type SdRef = light_sd::SharedCard<'static, Spi1Bus, Output>;

//   the recorder's card-borrowing state machines live in .bss, not on core 0's 4 KB stack
static REC_SLOT: CoreSlot<dict::Recording<SdRef>> = CoreSlot::new();
static PLAY_SLOT: CoreSlot<dict::Playback<SdRef>> = CoreSlot::new();

/// The app's [`dict::Store`]: the shared card, brought up on first touch since insertion.
type SdStore = light_sd::CardStore<Spi1Bus, Output, SysClock>;

// --- the board's own modules ---------------------------------------------------------------

//   the touch and IMU modules are the board's, shared by every touch349 app (see
// light_board_touch349::input) and generic over the event bus through light_input::BoardEvent; only
// the RTC and power/board modules below are the dictaphone's own

/// The PCF85063A on the shared i2c1, beside the IMU. Battery-backed: it keeps time across
/// power-off, and says so -- the oscillator-stop flag marks a time nobody set.

//   the storage-and-diagnostics module: the SD slot, the on-demand SD command, and the core-1 and
// stack diagnostics on `stats`. The power behaviour (backlight, battery, the audio-busy defer and
// the shutdown lifecycle) is the framework PowerMod now (proposal B unfused the two).
struct BoardMod {
        /// The TF slot; probed on demand (the `sd` command), not at boot -- an empty slot
        /// is this board's ordinary state. Shared with the recorder, per-operation.
        sd: &'static RefCell<SpiSd<Spi1Bus, Output>>,
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
                                        //   the instruments that convicted a core-1 stack kill once:
                                        // core 1's pulse, and how deep core 0's deepest chain reached
                                        info!("cores: core1 ticks {}, core0 stack low-water {} B above the runway base", core1_ticks(), stack_free());
                                }
                                AppEvent::Ext(Ext::Sd) => {
                                        busy = true;
                                        light_sd::shared::report(&mut self.sd.borrow_mut(), &mut SysClock);
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

/// The whole dictaphone firmware: bring the board up, wire the modules to the engine, and run the
/// runtime forever. An executable calls this from its `light_app_main` with its [`RunConfig`].
pub fn run(info: &ShellInfo, cfg: RunConfig) -> ! {
        stack_paint();
        log::set_clock(light_rp2::now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, peri {} Hz; touch i2c0 at {} Hz, imu i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.touch_bus.actual_hz, p.imu_bus.actual_hz);

        let front: &'static mut [u8] = FRAME_FRONT.take();
        let mut display = Display::new(Axs15231b::new(p.display_bus), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        //   region buffering: the page slide runs on this one buffer, no 215 KB second frame
        display.set_region_buffering(true);
        let font = match Font::parse(cfg.font) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        //   the IMU has i2c1 to itself on this board, but the RefCell keeps the same shape
        // as the other boards' shared-bus wiring
        static IMU_I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let imu_i2c: &'static RefCell<I2c1> = IMU_I2C.init(RefCell::new(p.imu_bus));
        let touch = Axs15231bTouch::new(p.touch_bus, p.touch_int, TOUCH_MAP, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(imu_i2c));

        //   the one card, shared: the board module's sd probe and the app's recorder and
        // fs commands all borrow it per operation
        static SD_CELL: StaticCell<RefCell<SpiSd<Spi1Bus, Output>>> = StaticCell::new();
        let sd: &'static RefCell<SpiSd<Spi1Bus, Output>> = SD_CELL.init(RefCell::new(SpiSd::new(p.sd_spi, p.sd_cs)));
        let mut power_mod = PowerMod::new(
                Touch349Power::new(p.backlight, p.sys_en, p.power_button, p.battery, p.charge_stat),
                SysClock,
                &EVENTS,
                power_backlight,
                power_is_stats,
                power_busy,
        );
        let mut board_mod = BoardMod { sd, events: EVENTS.subscribe().expect("subscriber slot") };
        let mut imu_mod = ImuMod::new(imu, &EVENTS, SysClock, IMU_AXIS_MAP);
        let mut rtc_mod = RtcMod::new(Pcf85063a::new(imu_i2c), &EVENTS, rtc_is_report, rtc_get_set);

        //   the app's big audio state, in .bss slots this module owns -- the AudioMod
        // value itself lives on core 0's small stack (its &RefCell fields cannot ride a
        // StaticCell), and a ~1.2 KB Recording inline once overflowed that stack straight
        // into core 1's
        //   a small frame-aligned read scratch for the playback path; the stall-riding DEPTH
        // is the transport's IRQ-drained prefetch ring (see I2sStream::start), not this
        static STAGE: ConstStaticCell<[u8; 2048]> = ConstStaticCell::new([0; 2048]);
        static PICKER: ConstStaticCell<FilePicker<{ dict::LIST_ROWS }>> = ConstStaticCell::new(FilePicker::new(Order::NameDescending, keep_recording));
        let mut audio_mod = dict::AudioMod::new(
                Es8311::new(imu_i2c),
                I2sStream::new(p.i2s),
                p.audio_pa,
                SysClock,
                SdStore { card: sd, clock: SysClock },
                AUDIO_SAMPLE_HZ,
                //   tuned on the glass: PGA code 7 with +18 dB digital puts normal speech
                // peaks ~45% of full scale -- and a DELIBERATELY loud take measured at
                // +2 dB more gain pinned full scale, so this is as hot as a recorder
                // without a limiter should default. The analog field is treacherous --
                // 0x17 -> 0x18 COLLAPSED the gain ~20 dB (the encoding is not linear);
                // retune digitally, in REG17, only
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
        //   the look-and-feel, from the embedded blob: a bad blob is a build-system bug
        // worth halting on, not styling to guess past
        let theme = match Theme::parse(cfg.theme) {
                Ok(t) => t,
                Err(e) => panic!("the embedded theme does not parse: {e:?}"),
        };
        layer.bg = theme.bg;
        ui.set_style(&Style::new(theme, Fonts::uniform(&font)));
        //   the interface, from the embedded blob: a bad blob is a build-system bug worth halting
        // on, like the theme
        let lui = match Lui::parse(cfg.ui) {
                Ok(l) => l,
                Err(e) => panic!("the embedded UI design does not parse: {e:?}"),
        };
        type BoardDisplayMod = DisplayMod<Axs15231b<PioQspiDisplayBus>, SysClock, Ext>;
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
                        desc: cfg.desc,
                        rotation_map: cfg.rotation_map,
                        initial_rotation: cfg.initial_rotation,
                        source: lui,
                        //   the layout axis comes from the design's orientation; the descent is this
                        // orientation's page-transition flow (portrait defaults, landscape rises)
                        default_descent: cfg.default_descent,
                },
        ));
        static TOUCH_MOD: StaticCell<TouchMod<AppEvent, Axs15231bTouch<I2c0, Input>, SysClock>> = StaticCell::new();
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
