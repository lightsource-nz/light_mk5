//! The touch349 firmware: the widget demo on the Waveshare RP2350-Touch-LCD-3.49 -- a
//! 172x640 bar of glass, the first QSPI panel (AXS15231B, LCD and touch in one chip), and
//! the first board on the RP2350's upper GPIO bank. The application is `light_app_ui_demo`;
//! this crate is the tangible 3.49: the wiring, the QSPI panel, the codec + TF-slot audio
//! leg, the battery latch, the shell ABI and the panic handler.
//!
//! Bring-up checklist (this board's support is authored from Waveshare's reference demo, not
//! a schematic): the panel lights and draws (if dark, SLPOUT/DISPON are the first suspects
//! -- see the driver); touch answers on i2c0 and coordinates track, with the axis directions
//! measured on the glass; the backlight really is inverted; the IMU axis map is
//! IDENTITY-until-measured, so orientation is suspect until calibrated.

#![no_std]

use core::cell::RefCell;
use light_app_ui_demo as demo;
use demo::{demo_commands, BoardHook, Command, DemoEvent, DisplayConfig, DisplayMod};
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
use light_fs::console::{FsOp, FsPath};
use light_recorder::console as recorder_console;
use light_recorder::{AudioStatus, Playback, Recorder, RecorderCommand, RecorderConfig, RecorderEvents, RecorderSlots, Recording};
use light_sd::{CardStore, SpiSd};
use light_board_touch349::{board, Touch349Power};
use light_power_manager::PowerMod;
use light_input::{ImuMod, TouchMod};
use light_rp2::shell::{panic_report, ShellInfo};
use board::*;
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::{I2c0, I2c1};
use light_rp2::i2s::I2sStream;
use light_rp2::spi_bus::Spi1Bus;
use light_rp2::qspi::PioQspiDisplayBus;
use light_rp2::{Breathe, Clocks, SysClock};

unsafe extern "C" {
        /// From this board's psram_info.c: what the SDK's runtime init detected on CS1.
        fn light_board_psram_size() -> u32;
}

/// The XIP CS1 window, through the UNCACHED alias -- a memtest through the cache would
/// largely test the cache.
const PSRAM_UNCACHED_BASE: u32 = 0x1500_0000;

/// Write-and-read three 4 KB regions (start, middle, end) with an address-derived pattern.
/// Returns (words checked, mismatches).
fn psram_test(size: u32) -> (u32, u32) {
        let (mut checked, mut bad) = (0u32, 0u32);
        for base in [0u32, size / 2, size.saturating_sub(4096)] {
                let p = (PSRAM_UNCACHED_BASE + base) as *mut u32;
                for i in 0..1024usize {
                        // SAFETY: within the detected PSRAM window; nothing else lives there
                        unsafe { core::ptr::write_volatile(p.add(i), (base ^ i as u32).wrapping_mul(0x9E37_79B9)) };
                }
                for i in 0..1024usize {
                        let want = (base ^ i as u32).wrapping_mul(0x9E37_79B9);
                        // SAFETY: as above
                        if unsafe { core::ptr::read_volatile(p.add(i)) } != want {
                                bad += 1;
                        }
                        checked += 1;
                }
        }
        (checked, bad)
}

/// The TF slot as a shareable block device: the board's one card, borrowed per block operation.
type SdRef = light_sd::SharedCard<'static, Spi1Bus, Output>;

//   the recorder's card-borrowing state machines live in .bss, not on core 0's small stack
static REC_SLOT: CoreSlot<Recording<SdRef>> = CoreSlot::new();
static PLAY_SLOT: CoreSlot<Playback<SdRef>> = CoreSlot::new();

const FRAME_BYTES: usize = PixelFormat::Rgb565.buffer_len(DISPLAY_WIDTH, DISPLAY_HEIGHT);

/// One frame buffer, 215 KB of the RP2350's 520. Region (partial) buffering keeps the page-slide
/// transition without the second full frame the capture path needs: the outgoing image is scrolled
/// off this buffer in place while the incoming is painted into the strip it uncovers (see
/// [`Display::set_region_buffering`] and the toolkit's page step). Rotation snaps rather than
/// animating -- the one animation a single buffer cannot carry.
static FRAME_FRONT: ConstStaticCell<[u8; FRAME_BYTES]> = ConstStaticCell::new([0; FRAME_BYTES]);

static FONT_BLOB: &[u8] = include_bytes!(env!("LIGHT_FONT_LGF"));
/// The look-and-feel: the framework's steel theme, the default for every board with
/// color support. A board-specific override would be a local theme file extending it.
static THEME_BLOB: &[u8] = include_bytes!(env!("LIGHT_THEME_LTH"));
/// The demo interface, authored as data: light_app_ui_demo's shared design compiled to an LUI blob,
/// with this board's title overlaid. The demo core builds its tree from this instead of a const page
/// tree -- the UI-as-data path the dictaphone already runs on.
static UI_BLOB: &[u8] = include_bytes!(env!("LIGHT_UI_LUI"));

// --- the event bus --------------------------------------------------------------------------

/// This board's extension events -- the codec + TF-slot audio leg, the RTC, the PSRAM
/// probe, the power path -- riding the demo's bus.
#[derive(Clone, Copy, Debug)]
enum Ext {
        RtcShow,
        RtcSet(Datetime),
        Psram,
        Sd,
        Fs { op: FsOp, path: FsPath, arg: FsPath },
        /// A command for the recorder (light-recorder): record, play, tone, volume, mic.
        Audio(RecorderCommand),
        /// The recorder's status, as it publishes it.
        AudioStatus(AudioStatus),
}

type AppEvent = DemoEvent<Ext>;

//   the recognizers the framework power module (light_power_manager::PowerMod) reads this app's
// events through: a backlight command carries a level, and `stats` asks for the battery report
// (this board has a gauge, so PowerMod prints it).
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
//   audio in flight defers the on-battery power-off
fn power_busy(e: &AppEvent) -> Option<bool> {
        if let DemoEvent::Ext(Ext::AudioStatus(s)) = e {
                Some(s.is_active())
        } else {
                None
        }
}

//   how the framework recorder (light_recorder::Recorder) reaches this app's events: its commands
// ride Ext::Audio, `stats` asks for its counters, and its status comes back as Ext::AudioStatus
fn recorder_command(e: &AppEvent) -> Option<RecorderCommand> {
        if let DemoEvent::Ext(Ext::Audio(c)) = e { Some(*c) } else { None }
}
fn recorder_status(s: AudioStatus) -> AppEvent {
        DemoEvent::Ext(Ext::AudioStatus(s))
}
fn audio(c: RecorderCommand) -> AppEvent {
        DemoEvent::Ext(Ext::Audio(c))
}

//   the recognizers the framework RTC module (light_rtc::RtcMod) reads this app's events through:
// report the clock on `stats` or `rtc show`, and set it on `rtc set` (both in this board's Ext).
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

//   the shell ABI glue (clocks, the core-1 console, panic) is the port's, in light_rp2::shell:
// the shell hands core 1 to the port's console loop once, and it feeds this app's console
// mailbox. No UART console on this board -- GPIO 0/1, the chip's default UART, carry the audio
// amplifier and data lines -- so the USB CDC is its one console
#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_main(_info: &ShellInfo) -> ! {
        light_rp2::shell::core1_main(demo::push_console_byte, None)
}

// --- the interface, as data ---------------------------------------------------------------

//   Bar glass, corners unmeasured: the theme's screen_radius keeps its default of 0
// until the glass says otherwise.
const FPS: u32 = 30;

/// The demo's Dim: through the floor mapping this lands ~59% LED-on time, in the lower
/// part of the panel's narrow usable band -- clearly dim, clearly lit. The original MAX/10
/// mapped below the driver's cutoff and read as OFF.
const BACKLIGHT_DIM: u16 = 250;

//   the widget tree is no longer a const page tree here: this board runs the demo from its LUI
// design blob (UI_BLOB). The title, row gap and list-row height that demo_pages! once baked in are
// the design's now -- the shared design this board's design.json extends.

/// Portrait only, MEASURED: the bar rests near-landscape on its long edge, so ordinary
/// handling flapped LandscapeL/R -- a 180-degree relayout per touch, and every tap then
/// landed where a widget used to be. A 172 px-tall landscape canvas was never worth having
/// anyway; the bar rotates end-for-end (a deliberate gesture, nowhere near the resting
/// pose) and otherwise holds still.
fn rotation_map(o: Orientation) -> Option<Rotation> {
        match o {
                Orientation::Portrait => Some(Rotation::R0),
                Orientation::PortraitFlip => Some(Rotation::R180),
                _ => None,
        }
}

/// No init or unload edges: the AXS15231B wants no GDDRAM offset, and its full-frame
/// pushes repaint everything anyway.
struct Hook;

impl BoardHook<Axs15231b<PioQspiDisplayBus>, Ext> for Hook {}

// --- the modules --------------------------------------------------------------------------

//   the touch and IMU modules are the board's, shared by every touch349 app (see
// light_board_touch349::input) and generic over the event bus through light_input::BoardEvent; only
// the RTC and board modules below are the demo's own on this board

//   the storage-and-diagnostics module: the SD slot and the on-demand SD / filesystem / PSRAM
// console commands. The power behaviour is the framework PowerMod now (proposal B unfused the two);
// this module holds only what is board-specific and app-coupled.
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
                                AppEvent::Ext(Ext::Psram) => {
                                        busy = true;
                                        let size = unsafe { light_board_psram_size() };
                                        if size == 0 {
                                                info!("psram: none detected");
                                        } else {
                                                let (checked, bad) = psram_test(size);
                                                info!("psram: {} KB detected; {} words tested, {} mismatches", size / 1024, checked, bad);
                                        }
                                }
                                AppEvent::Ext(Ext::Sd) => {
                                        busy = true;
                                        light_sd::shared::report(&mut self.sd.borrow_mut(), &mut SysClock);
                                }
                                AppEvent::Ext(Ext::Fs { op, path, arg }) => {
                                        busy = true;
                                        let mut store = light_sd::CardStore { card: self.sd, clock: SysClock };
                                        light_fs::console::run(&mut store, op, path.as_str(), arg.as_str());
                                }
                                _ => {}
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}

fn parse_fs(w: &mut Words) -> Parsed<AppEvent> {
        light_fs::console::parse_command(w, |op, path, arg| DemoEvent::Ext(Ext::Fs { op, path, arg }))
}

fn parse_rtc(w: &mut Words) -> Parsed<AppEvent> {
        light_rtc::command::parse_command(w, DemoEvent::Ext(Ext::RtcShow), |t| DemoEvent::Ext(Ext::RtcSet(t)))
}

static COMMANDS: &[CliCommand<AppEvent>] = &demo_commands![Ext;
        CliCommand { name: "rtc", usage: light_rtc::command::USAGE, parse: parse_rtc },
        CliCommand { name: "psram", usage: "psram", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Psram)) },
        CliCommand { name: "sd", usage: "sd", parse: |_| Parsed::Event(DemoEvent::Ext(Ext::Sd)) },
        CliCommand { name: "fs", usage: light_fs::console::USAGE, parse: parse_fs },
        //   the recorder's commands, parsed by light-recorder and carried in Ext::Audio
        CliCommand { name: "tone", usage: recorder_console::TONE_USAGE, parse: |w| recorder_console::wrap(recorder_console::tone(w), audio) },
        CliCommand { name: "volume", usage: recorder_console::VOLUME_USAGE, parse: |w| recorder_console::wrap(recorder_console::volume(w), audio) },
        CliCommand { name: "rec", usage: recorder_console::REC_USAGE, parse: |w| recorder_console::wrap(recorder_console::rec(w), audio) },
        CliCommand { name: "play", usage: recorder_console::PLAY_USAGE, parse: |w| recorder_console::wrap(recorder_console::play(w), audio) },
        CliCommand { name: "micmon", usage: recorder_console::MICMON_USAGE, parse: |w| recorder_console::wrap(recorder_console::micmon(w), audio) },
        CliCommand { name: "micgain", usage: recorder_console::MICGAIN_USAGE, parse: |w| recorder_console::wrap(recorder_console::micgain(w), audio) },
        CliCommand { name: "micdbg", usage: "micdbg", parse: |w| recorder_console::wrap(recorder_console::micdbg(w), audio) },
        CliCommand { name: "synth", usage: "synth", parse: |w| recorder_console::wrap(recorder_console::synth(w), audio) },
];
static CLI: Cli<AppEvent> = Cli::new(COMMANDS);

// --- entry ----------------------------------------------------------------------------------

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        log::set_clock(light_rp2::now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let p = take(&clocks).expect("the board's peripherals are taken once");
        info!("clocks: sys {} Hz, peri {} Hz; touch i2c0 at {} Hz, imu i2c1 at {} Hz", clocks.sys_hz, clocks.peri_hz, p.touch_bus.actual_hz, p.imu_bus.actual_hz);

        let front: &'static mut [u8] = FRAME_FRONT.take();
        let mut display = Display::new(Axs15231b::new(p.display_bus), front, DISPLAY_WIDTH, DISPLAY_HEIGHT, PixelFormat::Rgb565, light_rp2::now_us);
        //   region buffering: the page slide runs on this one buffer, no 215 KB second frame --
        // reveal off on open, cover back on close
        display.set_region_buffering(true);
        let font = match Font::parse(FONT_BLOB) {
                Ok(f) => f,
                Err(e) => panic!("the embedded font does not parse: {e:?}"),
        };
        //   the IMU has i2c1 to itself on this board, but the RefCell keeps the same shape
        // as the other boards' shared-bus wiring
        static IMU_I2C: StaticCell<RefCell<I2c1>> = StaticCell::new();
        let imu_i2c: &'static RefCell<I2c1> = IMU_I2C.init(RefCell::new(p.imu_bus));
        let touch = Axs15231bTouch::new(p.touch_bus, p.touch_int, TOUCH_MAP, (light_rp2::now_us() / 1000) as u32);
        let imu = Imu::new(Qmi8658::new(imu_i2c));

        //   the one card, shared: the board module's fs/sd console commands and the
        // recorder both borrow it per operation
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
        //   a small frame-aligned read scratch for the playback path; the stall-riding depth is the
        // transport's IRQ-drained prefetch ring
        static STAGE: ConstStaticCell<[u8; 2048]> = ConstStaticCell::new([0; 2048]);
        let mut recorder = Recorder::new(
                Es8311::new(imu_i2c),
                I2sStream::new(p.i2s),
                p.audio_pa,
                SysClock,
                CardStore { card: sd, clock: SysClock },
                RecorderConfig { sample_hz: AUDIO_SAMPLE_HZ, mic_gain: (0x17, 0xE3), volume: 73 },
                RecorderSlots {
                        // SAFETY: the one take of each slot, on the application core
                        rec: unsafe { REC_SLOT.take() },
                        play: unsafe { PLAY_SLOT.take() },
                        play_stage: STAGE.take(),
                },
                &EVENTS,
                RecorderEvents { command_of: recorder_command, is_stats: power_is_stats, status_event: recorder_status },
        );
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
        type BoardDisplayMod = DisplayMod<Axs15231b<PioQspiDisplayBus>, SysClock, Ext, Hook>;
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
                        desc: "AXS15231B over PIO-QSPI, double-buffered",
                        repush: true,
                        draw_over: false,
                        rotation_map,
                        source: lui,
                        backlight_dim: BACKLIGHT_DIM,
                },
                Hook,
        ));
        static TOUCH_MOD: StaticCell<TouchMod<AppEvent, Axs15231bTouch<I2c0, Input>, SysClock>> = StaticCell::new();
        let touch_mod = TOUCH_MOD.init(TouchMod::new(touch, Tracker::new(DISPLAY_WIDTH, DISPLAY_HEIGHT), &EVENTS, SysClock, demo::touch_reads_held));
        let mut console_mod = demo::ConsoleMod::new(&CLI, &EVENTS);

        let mut rt: Runtime<8> = Runtime::new();
        rt.add(&mut power_mod).expect("capacity");
        rt.add(&mut board_mod).expect("capacity");
        rt.add(display_mod).expect("capacity");
        rt.add(touch_mod).expect("capacity");
        rt.add(&mut imu_mod).expect("capacity");
        rt.add(&mut rtc_mod).expect("capacity");
        rt.add(&mut recorder).expect("capacity");
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
