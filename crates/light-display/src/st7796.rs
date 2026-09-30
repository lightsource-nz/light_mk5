//! ST7796S controller driver over a 4-wire SPI display bus: a 320x480 MIPI-DCS part, the same
//! command set as the ST7789 for windowing and pixel writes, with its own power, VCOM and gamma
//! registers behind a command-set unlock.
//!
//! The panel tuning below (power control, VCOM, the gamma curves) is the panel vendor's, taken
//! as a block: those registers describe the glass the controller is bonded to, not the
//! controller, and there is nothing to derive them from but the panel's own sheet.

use light_core::hal::{Clock, SpiDisplayBus};
use crate::display::{DisplayDriver, Frame, Region};

pub const CMD_SWRESET: u8 = 0x01;
pub const CMD_SLPOUT: u8 = 0x11;
pub const CMD_INVOFF: u8 = 0x20;
pub const CMD_INVON: u8 = 0x21;
pub const CMD_DISPOFF: u8 = 0x28;
pub const CMD_DISPON: u8 = 0x29;
pub const CMD_CASET: u8 = 0x2A;
pub const CMD_RASET: u8 = 0x2B;
pub const CMD_RAMWR: u8 = 0x2C;
pub const CMD_MADCTL: u8 = 0x36;
pub const CMD_COLMOD: u8 = 0x3A;
pub const COLMOD_16BPP: u8 = 0x55;
/// Command-set control: 0xC3 then 0x96 unlocks the extended registers, 0x3C then 0x69 locks
/// them again.
pub const CMD_CSCON: u8 = 0xF0;

/// MADCTL bits.
pub const MADCTL_MY: u8 = 0x80;
pub const MADCTL_MX: u8 = 0x40;
pub const MADCTL_MV: u8 = 0x20;
pub const MADCTL_BGR: u8 = 0x08;

/// Upper bound on one chunk. Worst case is bus-bound: a full 320x480x2 = 307200-byte frame at
/// 10 MHz is ~246 ms; 500 ms leaves headroom for a slower clock or contention.
const CHUNK_TIMEOUT_MS: u32 = 500;
/// Row chunks to spin through per poll when a region is chunked by row -- each is a short
/// burst, and yielding after every one would cost a whole pass to move almost nothing.
const ROW_CHUNKS_PER_POLL: u16 = 8;

/// The panel's extended-register tuning, sent between the unlock and the lock: (command, data).
const PANEL_TUNING: &[(u8, &[u8])] = &[
        // display inversion control: 1-dot
        (0xB4, &[0x01]),
        // entry mode
        (0xB7, &[0xC6]),
        // power control 1..3
        (0xC0, &[0x80, 0x45]),
        (0xC1, &[0x13]),
        (0xC2, &[0xA7]),
        // VCOM
        (0xC5, &[0x0A]),
        // display output ctrl adjust
        (0xE8, &[0x40, 0x8A, 0x00, 0x00, 0x29, 0x19, 0xA5, 0x33]),
        // positive and negative gamma
        (0xE0, &[0xD0, 0x08, 0x0F, 0x06, 0x06, 0x33, 0x30, 0x33, 0x47, 0x17, 0x13, 0x13, 0x2B, 0x31]),
        (0xE1, &[0xD0, 0x0A, 0x11, 0x0B, 0x09, 0x07, 0x2F, 0x33, 0x47, 0x38, 0x15, 0x16, 0x2C, 0x32]),
];

pub struct St7796<B: SpiDisplayBus> {
        bus: B,
        madctl: u8,
        invert: bool,
        col_offset: u16,
        row_offset: u16,
        width: u16,
        height: u16,
}

impl<B: SpiDisplayBus> St7796<B> {
        /// `madctl` is the memory access control byte -- scan direction and colour order, a fact
        /// about how the glass is bonded; `invert` is whether this panel needs display
        /// inversion on to show true colours (an IPS panel usually does).
        pub fn new(bus: B, madctl: u8, invert: bool) -> Self {
                Self { bus, madctl, invert, col_offset: 0, row_offset: 0, width: 0, height: 0 }
        }

        /// Where the visible glass sits in GDDRAM. Zero for a panel that uses the controller's
        /// full 320x480.
        pub fn set_offset(&mut self, col: u16, row: u16) {
                self.col_offset = col;
                self.row_offset = row;
        }

        /// The bus, for bring-up instrumentation -- re-clocking a panel live to probe its
        /// headroom. Nothing may be in flight: the caller waits the display out first.
        pub fn bus_mut(&mut self) -> &mut B {
                &mut self.bus
        }

        fn set_window(&mut self, r: &Region) {
                let cx0 = r.x0 + self.col_offset;
                let cx1 = r.x1 + self.col_offset;
                let cy0 = r.y0 + self.row_offset;
                let cy1 = r.y1 + self.row_offset;
                self.bus.command(CMD_CASET);
                self.bus.data(&[(cx0 >> 8) as u8, cx0 as u8, (cx1 >> 8) as u8, cx1 as u8]);
                self.bus.command(CMD_RASET);
                self.bus.data(&[(cy0 >> 8) as u8, cy0 as u8, (cy1 >> 8) as u8, cy1 as u8]);
        }

        /// Blocking clear of the whole panel, using the CURRENT offset.
        pub fn clear(&mut self, color: u16) {
                let full = Region::full(self.width, self.height);
                self.set_window(&full);
                self.bus.command(CMD_RAMWR);
                let mut row = [0u8; 480 * 2];
                let n = (self.width as usize).min(480);
                for px in row[..n * 2].chunks_exact_mut(2) {
                        px[0] = (color >> 8) as u8;
                        px[1] = color as u8;
                }
                for _ in 0..self.height {
                        self.bus.data(&row[..n * 2]);
                }
        }

        fn full_width(&self, r: &Region) -> bool {
                r.x0 == 0 && r.x1 == self.width - 1
        }
}

impl<B: SpiDisplayBus> DisplayDriver for St7796<B> {
        fn init(&mut self, clock: &mut dyn Clock, width: u16, height: u16) {
                self.width = width;
                self.height = height;
                self.bus.reset_pulse(clock);
                self.bus.command(CMD_SWRESET);
                clock.delay_ms(150); // datasheet: >=120 ms after SWRESET
                self.bus.command(CMD_SLPOUT);
                clock.delay_ms(120); // datasheet: >=120 ms after SLPOUT
                self.bus.command(CMD_MADCTL);
                self.bus.data(&[self.madctl]);
                self.bus.command(CMD_COLMOD);
                self.bus.data(&[COLMOD_16BPP]);
                self.bus.command(CMD_CSCON);
                self.bus.data(&[0xC3]);
                self.bus.command(CMD_CSCON);
                self.bus.data(&[0x96]);
                for &(cmd, data) in PANEL_TUNING {
                        self.bus.command(cmd);
                        self.bus.data(data);
                }
                self.bus.command(CMD_CSCON);
                self.bus.data(&[0x3C]);
                self.bus.command(CMD_CSCON);
                self.bus.data(&[0x69]);
                clock.delay_ms(120);
                self.bus.command(if self.invert { CMD_INVON } else { CMD_INVOFF });
                self.bus.command(CMD_DISPON);
                clock.delay_ms(20);
        }

        fn chunk_count(&self, r: &Region) -> u16 {
                // a full-width region is one contiguous run in the frame buffer, so it is a
                // single chunk however tall; a narrower one must go a row per burst
                if self.full_width(r) { 1 } else { r.height() }
        }

        fn chunks_per_poll(&self, r: &Region) -> u16 {
                if self.full_width(r) { 0 } else { ROW_CHUNKS_PER_POLL }
        }

        fn kick(&mut self, frame: &Frame<'_>, r: &Region, index: u16) {
                // the window is armed once: RAMWR streams into it and wraps from x1 back to x0
                // on each new row by itself, so every later row is just more data
                if index == 0 {
                        self.set_window(r);
                        self.bus.command(CMD_RAMWR);
                }
                let bytes = if self.full_width(r) { frame.rows(r) } else { frame.row(r, r.y0 + index) };
                // SAFETY: `frame` borrows the display core's buffer, which refuses mutation
                // while an update is in flight, and stays alive for the core's lifetime
                unsafe { self.bus.start_data(bytes) }
        }

        fn chunk_complete(&mut self) -> bool {
                self.bus.is_complete()
        }

        fn chunk_timeout_ms(&self) -> u32 {
                CHUNK_TIMEOUT_MS
        }
}
