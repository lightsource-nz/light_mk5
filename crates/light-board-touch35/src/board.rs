//! Board wiring for the Waveshare RP2350-Touch-LCD-3.5 (an RP2350B): pins from the board's
//! official schematic GPIO table, cross-checked against the vendor's example sources. Where the
//! two disagree the schematic wins -- the vendor sources carry a wrong IMU interrupt pin (14, which
//! is the I2S MCLK) and copy-pasted MISO and battery-ADC pins that are other nets.
//!
//! One I2C bus carries every device: touch, PMIC, IMU, RTC, codec and the camera's SCCB. There
//! is no IO expander; every control line is a plain GPIO.

use light_core::atomic::{AtomicBool, Ordering};
use light_input::drivers::ft6336::CoordMap;
use light_input::imu::{self, AxisMap};
use light_rp2::gpio::{Input, Output};
use light_rp2::i2c::I2c1;
use light_rp2::i2s::PioI2sOut;
use light_rp2::pwm::PwmOutput;
use light_rp2::spi::Spi0Display;
use light_rp2::spi_bus::Spi1Bus;
use light_rp2::Clocks;

// ST7796S panel, SPI 4-wire on SPI0 (GPIO18/19 mux to SPI0 SCK/TX). No MISO is wired.
pub const PIN_DISPLAY_SCK: usize = 18;
pub const PIN_DISPLAY_MOSI: usize = 19;
pub const PIN_DISPLAY_DC: usize = 20;
pub const PIN_DISPLAY_CS: usize = 21;
pub const PIN_DISPLAY_BL: usize = 22;
pub const PIN_DISPLAY_RESET: usize = 23;
/// 320x480 portrait: the ST7796's full native GDDRAM, so there is no offset.
pub const DISPLAY_WIDTH: u16 = 320;
pub const DISPLAY_HEIGHT: u16 = 480;
/// 75 MHz: the highest rate the PL022 can make from a 150 MHz clk_peri. A full-frame push is
/// ~33 ms at this rate. Unmeasured on this panel; the `spi` console command probes it live.
pub const DISPLAY_SPI_HZ: u32 = 75_000_000;
/// Memory access control: MX with BGR colour order -- portrait, and the order the glass is
/// bonded in. Confirmed on the glass: upright, true colours.
pub const DISPLAY_MADCTL: u8 = 0x48;
/// IPS glass: true colours need display inversion on.
pub const DISPLAY_INVERT: bool = true;
/// The panel keeps its own frame, so the RP2350 holds none: the display is BANDED, drawn and
/// pushed this many full-width rows at a time, from two band buffers (one drawn while the other
/// is pushed) -- 2 x 20 KB in place of a 300 KB frame. Each band is one DMA burst.
pub const DISPLAY_BAND_ROWS: u16 = 32;

// FT6336U touch controller on the shared I2C1, with its own reset line.
pub const PIN_I2C_SDA: usize = 34;
pub const PIN_I2C_SCL: usize = 35;
pub const PIN_TOUCH_RST: usize = 24;
pub const PIN_TOUCH_INT: usize = 25;
pub const I2C_HZ: u32 = 400_000;
/// The touch panel's raw coordinates line up with the portrait frame at this MADCTL with no
/// transform -- confirmed on the glass: taps land under the finger.
pub const TOUCH_MAP: CoordMap = CoordMap { x_max: DISPLAY_WIDTH - 1, y_max: DISPLAY_HEIGHT - 1, invert_x: false, invert_y: false, swap_xy: false };

/// How this board mounts the QMI8658, MEASURED with the +1g-points-up convention: upright read
/// chip [-950, +60, -100] (up the screen = -chip X), flat face-up read [+35, +208, -953] (out of
/// the screen = -chip Z), and device X follows by right-handedness (= -chip Y).
pub const IMU_AXIS_MAP: AxisMap = AxisMap { source: [imu::Y, imu::X, imu::Z], sign: [-1, -1, -1] };

/// DMA channel for the display bus, from the top of the range.
pub const DISPLAY_DMA_CH: usize = 15;

/// Backlight: a transistor low-side switch -- driving the GPIO high lights the panel. Levels run
/// `0..=BACKLIGHT_LEVEL_MAX`.
pub const BACKLIGHT_LEVEL_MAX: u16 = 1000;
pub const BACKLIGHT_CARRIER_HZ: u32 = 30_000;

// The interrupt lines from the parts on the shared bus: declared so nothing reuses them, and left
// unused -- every one of those parts is polled
pub const PIN_RTC_INT: usize = 37;
pub const PIN_IMU_INT1: usize = 38;
pub const PIN_PMIC_IRQ: usize = 39;
/// The AXP2101's power key, level-shifted onto a GPIO (the board carries its own pull-up).
pub const PIN_POWER_KEY: usize = 40;
// the ES8311 codec's I2S (the codec is the I2S master; this side makes MCLK and answers its
// BCLK/LRCK), and the NS4150B speaker amplifier's enable (high = on)
pub const PIN_I2S_DOUT: usize = 12;
pub const PIN_I2S_DIN: usize = 13;
pub const PIN_I2S_MCLK: usize = 14;
pub const PIN_I2S_LRCK: usize = 15;
pub const PIN_I2S_BCLK: usize = 16;
pub const PIN_AMP_EN: usize = 17;
/// The codec runs at 256 x Fs from the MCLK this side generates.
pub const AUDIO_SAMPLE_HZ: u32 = 24_000;
pub const AUDIO_MCLK_HZ: u32 = AUDIO_SAMPLE_HZ * 256;
/// DMA channels for the playback ping-pong and the capture ping-pong, below the display's.
pub const AUDIO_DMA_CH: [usize; 2] = [13, 14];
pub const AUDIO_CAP_DMA_CH: [usize; 2] = [11, 12];
// the TF slot, wired for both SPI1 (SCK/MOSI/MISO/CS) and 4-bit SDIO (CLK/CMD/D0..D3); driven in
// SPI mode, with the two SDIO-only data lines left alone. No card-detect line is wired.
pub const PIN_SD_SCK: usize = 26;
pub const PIN_SD_MOSI: usize = 27;
pub const PIN_SD_MISO: usize = 28;
pub const PIN_SD_D1: usize = 29;
pub const PIN_SD_D2: usize = 30;
pub const PIN_SD_CS: usize = 31;
// the camera: D0..D7 on 0..7, VSYNC 8, HREF 9, PCLK 10, XCLK 11, and its power-down
pub const PIN_CAM_PWDN: usize = 36;
// UART0 on the header (TX through the UART aux function on 46)
pub const PIN_UART_TX: usize = 46;
pub const PIN_UART_RX: usize = 45;

pub struct Peripherals {
        pub display_bus: Spi0Display,
        /// PWM-driven; starts dark.
        pub backlight: PwmOutput,
        /// The one I2C bus every device shares.
        pub i2c: I2c1,
        pub touch_int: Input,
        pub touch_reset: Output,
        /// The speaker amplifier, held off until something drives audio.
        pub amp_en: Output,
        /// The codec's I2S: playback out, the microphone's capture in, on PIO1.
        pub i2s: PioI2sOut,
        /// The power key's level, through the PMIC's level shifter: high while held.
        pub power_key: Input,
        /// The TF slot's bus, at the card-initialisation rate until a card is brought up.
        pub sd_spi: Spi1Bus,
        pub sd_cs: Output,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// Configure and hand over the board's peripherals. Once.
pub fn take(clocks: &Clocks) -> Option<Peripherals> {
        if TAKEN.swap(true, Ordering::AcqRel) {
                return None;
        }
        // SAFETY: the flag above makes this the one construction of each peripheral;
        // the shell uses none of them (its USB and timer blocks are not in this set)
        unsafe {
                Some(Peripherals {
                        amp_en: Output::new(PIN_AMP_EN, false),
                        i2s: {
                                let mut i2s = PioI2sOut::new(PIN_I2S_DOUT, PIN_I2S_BCLK, PIN_I2S_LRCK, PIN_I2S_MCLK, clocks.sys_hz, AUDIO_MCLK_HZ, AUDIO_DMA_CH[0], AUDIO_DMA_CH[1]);
                                i2s.attach_capture(PIN_I2S_DIN, AUDIO_CAP_DMA_CH[0], AUDIO_CAP_DMA_CH[1]);
                                i2s
                        },
                        power_key: Input::new_pull_up(PIN_POWER_KEY),
                        sd_spi: Spi1Bus::new(clocks.peri_hz, PIN_SD_SCK, PIN_SD_MOSI, PIN_SD_MISO, 300_000),
                        sd_cs: Output::new(PIN_SD_CS, true),
                        display_bus: Spi0Display::new(
                                clocks.peri_hz,
                                PIN_DISPLAY_SCK,
                                PIN_DISPLAY_MOSI,
                                PIN_DISPLAY_CS,
                                PIN_DISPLAY_DC,
                                Some(PIN_DISPLAY_RESET),
                                DISPLAY_SPI_HZ,
                                DISPLAY_DMA_CH,
                        ),
                        backlight: PwmOutput::new(PIN_DISPLAY_BL, clocks.sys_hz, BACKLIGHT_CARRIER_HZ, BACKLIGHT_LEVEL_MAX),
                        i2c: I2c1::new(clocks.sys_hz, PIN_I2C_SCL, PIN_I2C_SDA, I2C_HZ),
                        touch_int: Input::new_pull_up(PIN_TOUCH_INT),
                        touch_reset: Output::new(PIN_TOUCH_RST, true),
                })
        }
}
