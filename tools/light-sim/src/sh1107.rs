//! A simulated SH1107 display controller on the SPI display bus: the panel the real driver
//! talks to, decoded rather than bypassed.
//!
//! The framework can render a UI offscreen without any controller (the host GUI's null
//! driver does that for previews). A simulator wants more: it wants the driver to run, with its
//! column and page addressing, its init sequence and its display offset, against a controller
//! that keeps RAM the way the part does -- so that a driver bug shows on the simulated glass
//! the way it would on the real one. This decodes the command set the driver uses: column and
//! page addresses, the addressing mode, the display offset and start line, segment remap and
//! scan direction, inverse and all-on, display on and off. Everything else it accepts and
//! ignores, as the part does with settings that change nothing visible.
//!
//! THE GLASS OFFSET is the one thing this cannot know from the commands: which RAM row a
//! panel's first visible line sits at is a property of how the glass is bonded, and the
//! firmware's display offset exists to meet it. The simulated panel is told the offset the
//! glass wants, and shows the RAM rows the configured offset selects -- so the firmware's
//! correct value gives a correct picture, and a wrong one gives the wrapped picture the real
//! panel would.

use std::sync::{Arc, Mutex};

use light_core::hal::{Clock, SpiDisplayBus};

/// The controller's RAM: 128 segments (columns) by 128 rows, as 16 pages of 8.
pub const RAM_COLUMNS: usize = 128;
pub const RAM_ROWS: usize = 128;
const PAGES: usize = RAM_ROWS / 8;

/// The controller's settings and RAM, shared between the bus the driver drives and the view
/// an interface draws.
struct State {
        ram: [[u8; PAGES]; RAM_COLUMNS],
        column: u8,
        page: u8,
        vertical: bool,
        display_on: bool,
        inverse: bool,
        all_on: bool,
        seg_remap: bool,
        scan_down: bool,
        offset: u8,
        start_line: u8,
        /// The second byte of a two-byte command, when one is pending.
        pending: Option<u8>,
        /// Data bytes written since the view last asked: a frame counter for the curious.
        writes: u64,
}

impl State {
        fn new() -> Self {
                Self { ram: [[0; PAGES]; RAM_COLUMNS], column: 0, page: 0, vertical: false, display_on: false, inverse: false, all_on: false, seg_remap: false, scan_down: true, offset: 0, start_line: 0, pending: None, writes: 0 }
        }

        fn command(&mut self, cmd: u8) {
                if let Some(first) = self.pending.take() {
                        match first {
                                0xD3 => self.offset = cmd & 0x7F,
                                0xDC => self.start_line = cmd & 0x7F,
                                // contrast, mux ratio, clock, charge periods, VCOMH, power mode:
                                // nothing visible here
                                _ => {}
                        }
                        return;
                }
                match cmd {
                        0x00..=0x0F => self.column = (self.column & 0x70) | (cmd & 0x0F),
                        0x10..=0x17 => self.column = (self.column & 0x0F) | ((cmd & 0x07) << 4),
                        0x20 => self.vertical = false,
                        0x21 => self.vertical = true,
                        0xB0..=0xBF => self.page = cmd & 0x0F,
                        0xA0 => self.seg_remap = false,
                        0xA1 => self.seg_remap = true,
                        0xA4 => self.all_on = false,
                        0xA5 => self.all_on = true,
                        0xA6 => self.inverse = false,
                        0xA7 => self.inverse = true,
                        0xAE => self.display_on = false,
                        0xAF => self.display_on = true,
                        0xC0 => self.scan_down = true,
                        0xC8 => self.scan_down = false,
                        0x81 | 0xA8 | 0xAD | 0xD3 | 0xD5 | 0xD9 | 0xDB | 0xDC => self.pending = Some(cmd),
                        _ => {}
                }
        }

        fn data(&mut self, byte: u8) {
                let column = usize::from(self.column) % RAM_COLUMNS;
                let page = usize::from(self.page) % PAGES;
                self.ram[column][page] = byte;
                self.writes += 1;
                //   the address that advances after a byte is the mode's: pages in vertical
                // mode, columns in page mode, each wrapping within its range
                if self.vertical {
                        self.page = ((page + 1) % PAGES) as u8;
                } else {
                        self.column = ((column + 1) % RAM_COLUMNS) as u8;
                }
        }

        /// The pixel at glass position `(x, y)`, for a glass of `width` columns and `height`
        /// rows whose first visible row sits at RAM row `glass_offset`.
        fn pixel(&self, x: usize, y: usize, width: usize, height: usize, glass_offset: u8) -> bool {
                if !self.display_on {
                        return false;
                }
                if self.all_on {
                        return !self.inverse;
                }
                let x = if self.seg_remap { width - 1 - x } else { x };
                let y = if self.scan_down { y } else { height - 1 - y };
                //   the row the glass line shows: the RAM row the configured offset and start
                // line select, relative to where the glass is bonded
                let row = (y + usize::from(self.offset) + usize::from(self.start_line) + RAM_ROWS - usize::from(glass_offset)) % RAM_ROWS;
                let bit = self.ram[x % RAM_COLUMNS][row / 8] & (1 << (row % 8)) != 0;
                bit != self.inverse
        }
}

/// The simulated controller as the driver's bus. Construct one, keep its [`view`](Self::view)
/// for the interface, and hand the bus to the driver.
pub struct Sh1107Panel {
        state: Arc<Mutex<State>>,
}

/// The panel as an interface sees it: the picture on the glass.
#[derive(Clone)]
pub struct PanelView {
        state: Arc<Mutex<State>>,
        width: usize,
        height: usize,
        glass_offset: u8,
}

impl Sh1107Panel {
        /// A panel `width` segments by `height` rows whose glass is bonded at RAM row
        /// `glass_offset`: the display offset a correct firmware configures for it.
        pub fn new(width: u16, height: u16, glass_offset: u8) -> (Self, PanelView) {
                let state = Arc::new(Mutex::new(State::new()));
                let view = PanelView { state: state.clone(), width: usize::from(width), height: usize::from(height), glass_offset };
                (Self { state }, view)
        }
}

impl SpiDisplayBus for Sh1107Panel {
        fn command(&mut self, cmd: u8) {
                self.state.lock().expect("panel").command(cmd);
        }

        fn data(&mut self, bytes: &[u8]) {
                let mut s = self.state.lock().expect("panel");
                for &b in bytes {
                        s.data(b);
                }
        }

        unsafe fn start_data(&mut self, bytes: &[u8]) {
                // nothing is asynchronous here: the bytes land before this returns
                self.data(bytes);
        }

        fn is_complete(&mut self) -> bool {
                true
        }

        fn reset_pulse(&mut self, _clock: &mut dyn Clock) {
                *self.state.lock().expect("panel") = State::new();
        }
}

impl PanelView {
        pub fn width(&self) -> usize {
                self.width
        }

        pub fn height(&self) -> usize {
                self.height
        }

        /// The picture, row-major, one `bool` per pixel: what the glass shows right now.
        pub fn pixels(&self) -> Vec<bool> {
                let s = self.state.lock().expect("panel");
                let mut out = Vec::with_capacity(self.width * self.height);
                for y in 0..self.height {
                        for x in 0..self.width {
                                out.push(s.pixel(x, y, self.width, self.height, self.glass_offset));
                        }
                }
                out
        }

        pub fn pixel(&self, x: usize, y: usize) -> bool {
                self.state.lock().expect("panel").pixel(x, y, self.width, self.height, self.glass_offset)
        }

        /// Whether the display is switched on.
        pub fn is_on(&self) -> bool {
                self.state.lock().expect("panel").display_on
        }

        /// The display offset the firmware has configured.
        pub fn configured_offset(&self) -> u8 {
                self.state.lock().expect("panel").offset
        }

        /// Data bytes written since power-up.
        pub fn writes(&self) -> u64 {
                self.state.lock().expect("panel").writes
        }
}

#[cfg(test)]
mod tests {
        use super::*;

        /// The driver's own way of putting one column on the glass: both column nibbles, the
        /// page, then the pages' bytes in vertical mode.
        fn write_column(bus: &mut Sh1107Panel, column: u8, pages: &[u8]) {
                bus.command(0x00 | (column & 0x0F));
                bus.command(0x10 | (column >> 4));
                bus.command(0xB0);
                bus.data(pages);
        }

        fn on(bus: &mut Sh1107Panel, offset: u8) {
                bus.command(0x21);
                bus.command(0xD3);
                bus.command(offset);
                bus.command(0xAF);
        }

        #[test]
        fn a_column_written_in_vertical_mode_lands_as_a_vertical_run_of_pixels() {
                let (mut bus, view) = Sh1107Panel::new(64, 128, 0);
                on(&mut bus, 0);
                // column 5: the first byte lights rows 0 and 3, the second byte lights row 8
                write_column(&mut bus, 5, &[0b0000_1001, 0b0000_0001]);
                assert!(view.pixel(5, 0));
                assert!(!view.pixel(5, 1));
                assert!(view.pixel(5, 3));
                assert!(view.pixel(5, 8));
                assert!(!view.pixel(4, 0));
                assert_eq!(view.writes(), 2);
        }

        #[test]
        fn the_glass_shows_nothing_until_the_display_is_on() {
                let (mut bus, view) = Sh1107Panel::new(64, 128, 0);
                bus.command(0x21);
                write_column(&mut bus, 0, &[0xFF]);
                assert!(!view.pixel(0, 0));
                assert!(!view.is_on());
                bus.command(0xAF);
                assert!(view.pixel(0, 0));
        }

        #[test]
        fn the_right_offset_puts_the_picture_where_the_glass_is_and_a_wrong_one_wraps_it() {
                let (mut bus, view) = Sh1107Panel::new(64, 128, 96);
                on(&mut bus, 96);
                write_column(&mut bus, 0, &[0x01]);
                assert!(view.pixel(0, 0), "the configured offset meets the glass: RAM row 0 is glass row 0");
                assert_eq!(view.configured_offset(), 96);
                // a firmware that forgot the offset shows RAM row 0 ninety-six rows down the
                // glass: the picture wrapped, as the real panel wraps it
                bus.command(0xD3);
                bus.command(0);
                assert!(!view.pixel(0, 0));
                assert!(view.pixel(0, 96));
        }

        #[test]
        fn inverse_and_all_on_and_remap_behave_as_the_part_does() {
                let (mut bus, view) = Sh1107Panel::new(64, 128, 0);
                on(&mut bus, 0);
                write_column(&mut bus, 1, &[0x01]);
                bus.command(0xA7);
                assert!(!view.pixel(1, 0) && view.pixel(1, 1), "inverse");
                bus.command(0xA6);
                bus.command(0xA5);
                assert!(view.pixel(63, 127), "all on");
                bus.command(0xA4);
                bus.command(0xA1);
                assert!(view.pixel(62, 0), "segment remap mirrors x");
                bus.command(0xA0);
                bus.command(0xC8);
                assert!(view.pixel(1, 127), "scan direction mirrors y");
        }

        #[test]
        fn page_mode_advances_the_column_and_a_reset_clears_everything() {
                let (mut bus, view) = Sh1107Panel::new(64, 128, 0);
                bus.command(0x20);
                bus.command(0xAF);
                bus.command(0x00);
                bus.command(0x10);
                bus.command(0xB2);
                bus.data(&[0x80, 0x80]);
                assert!(view.pixel(0, 23) && view.pixel(1, 23));
                bus.reset_pulse(&mut crate::SimClock);
                assert!(!view.is_on());
                assert!(view.pixels().iter().all(|p| !p));
        }
}
