//! OmniVision OV5640: a 5 MP image sensor with an on-chip ISP (scaler, AWB, AEC, colour matrix)
//! and a DVP parallel output. 16-bit register addresses over SCCB.
//!
//! The register values are esp32-camera's, which the module vendors' own drivers also carry: a
//! tuning table (AEC/AGC, AWB, colour matrix, gamma, sharpness) taken as a block, then the output
//! window, binning, PLL and pixel format written per configuration. Only the binned 4:3 path is
//! configured here -- the whole sensor read 2x2-binned to 1312x976 and scaled by the ISP to the
//! requested size, which covers every preview size up to 1280x960.
//!
//! The pixel format is RGB565 sent high byte first ({R[4:0],G[5:3]} then {G[2:0],B[4:0]}),
//! the byte order a display bus sends, so captured frames need no swap.

use light_core::hal::{Clock, I2cBus};

use crate::CameraError;

pub const I2C_ADDR: u8 = 0x3C;
pub const CHIP_ID: u16 = 0x5640;

const REG_CHIP_ID: u16 = 0x300A;
const REG_SYSTEM_CTRL0: u16 = 0x3008;
const REG_TIMING_X_START: u16 = 0x3800;
const REG_TIMING_X_END: u16 = 0x3804;
const REG_TIMING_OUTPUT: u16 = 0x3808;
const REG_TIMING_TOTAL: u16 = 0x380C;
const REG_TIMING_OFFSET: u16 = 0x3810;
const REG_X_INCREMENT: u16 = 0x3814;
const REG_Y_INCREMENT: u16 = 0x3815;
const REG_TIMING_TC20: u16 = 0x3820;
const REG_TIMING_TC21: u16 = 0x3821;
const REG_ISP_CTRL01: u16 = 0x5001;
const REG_FORMAT_CTRL00: u16 = 0x4300;
const REG_FORMAT_MUX: u16 = 0x501F;
const REG_PRE_ISP_TEST: u16 = 0x503D;

/// ISP control 01: the scaler.
const ISP_SCALE_EN: u8 = 0x20;
/// FORMAT_CTRL00: RGB565, {R,G} byte first.
const FORMAT_RGB565_BE: u8 = 0x61;
/// FORMAT MUX: ISP RGB.
const MUX_ISP_RGB: u8 = 0x01;
/// Pre-ISP test pattern: the eight-bar colour bar.
const TEST_COLOR_BAR: u8 = 0x80;

/// The binned 4:3 window: the full sensor read 2x2-binned, the ISP scaling to any size up to it.
const WINDOW_X_END: u16 = 2623;
const WINDOW_Y_END: u16 = 1951;
const BINNED_MAX_W: u16 = 1280;
const BINNED_MAX_H: u16 = 960;
const OFFSET_X: u16 = 16;
const OFFSET_Y: u16 = 8;
const TOTAL_Y: u16 = 984;

/// The sensor's PLL, in the terms its registers take. The PCLK it yields is
/// `xclk / pre_div * multiplier / (root_2x ? 2 : 1) / sys_div * 2 / 5 / 2^pclk_root_div / pclk_div`;
/// it must carry a line's bytes inside the line time, and stay within what the port's capture
/// keeps up with. The values depend on the master clock the board feeds, so the board picks them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pll {
        /// 4..=252.
        pub multiplier: u8,
        /// 1..=15.
        pub sys_div: u8,
        /// 1..=8 (the register's encoding: 1, 2, 3, 4, 1.5, 6, 2.5, 8).
        pub pre_div: u8,
        pub root_2x: bool,
        /// 0..=3: divide by 1, 2, 4, 8.
        pub pclk_root_div: u8,
        /// 1..=31.
        pub pclk_div: u8,
}

/// The tuning block, written after a software reset: (register, value), with a pause where a
/// delay is given. The leading power-down and the trailing power-up bracket it.
const DEFAULTS: &[(u16, u8)] = &[
        (REG_SYSTEM_CTRL0, 0x42), // power down while configuring
        (0x3103, 0x13),           // system clock from the PLL
        (0x3017, 0xFF),           // data and sync pins outputs
        (0x3018, 0xFF),
        (0x302C, 0xC3), // drive capability
        (0x4740, 0x21), // sync polarities: VSYNC active high, PCLK sampled on its rising edge
        (0x4713, 0x02), // JPEG mode select
        (REG_ISP_CTRL01, 0x83),
];

const DEFAULTS_AFTER_MCU_RESET: &[(u16, u8)] = &[
        (0x3002, 0x1C),
        (0x3004, 0xFF), // clock enables
        (0x3006, 0xC3),
        (0x5000, 0xA7), // ISP control
        (REG_ISP_CTRL01, 0xA3),
        (0x5003, 0x08),
        (0x370C, 0x02),
        (0x3634, 0x40),
        // AEC/AGC
        (0x3A02, 0x03),
        (0x3A03, 0xD8),
        (0x3A08, 0x01),
        (0x3A09, 0x27),
        (0x3A0A, 0x00),
        (0x3A0B, 0xF6),
        (0x3A0D, 0x04),
        (0x3A0E, 0x03),
        (0x3A0F, 0x30),
        (0x3A10, 0x28),
        (0x3A11, 0x60),
        (0x3A13, 0x43),
        (0x3A14, 0x03),
        (0x3A15, 0xD8),
        (0x3A18, 0x00),
        (0x3A19, 0xF8),
        (0x3A1B, 0x30),
        (0x3A1E, 0x26),
        (0x3A1F, 0x14),
        // VCM debug
        (0x3600, 0x08),
        (0x3601, 0x33),
        // 50/60 Hz detection
        (0x3C01, 0xA4),
        (0x3C04, 0x28),
        (0x3C05, 0x98),
        (0x3C06, 0x00),
        (0x3C07, 0x08),
        (0x3C08, 0x00),
        (0x3C09, 0x1C),
        (0x3C0A, 0x9C),
        (0x3C0B, 0x40),
        (0x460C, 0x22), // no JPEG footer; PCLK divider manual
        // black level calibration
        (0x4001, 0x02),
        (0x4004, 0x02),
        // AWB
        (0x5180, 0xFF),
        (0x5181, 0xF2),
        (0x5182, 0x00),
        (0x5183, 0x14),
        (0x5184, 0x25),
        (0x5185, 0x24),
        (0x5186, 0x09),
        (0x5187, 0x09),
        (0x5188, 0x09),
        (0x5189, 0x75),
        (0x518A, 0x54),
        (0x518B, 0xE0),
        (0x518C, 0xB2),
        (0x518D, 0x42),
        (0x518E, 0x3D),
        (0x518F, 0x56),
        (0x5190, 0x46),
        (0x5191, 0xF8),
        (0x5192, 0x04),
        (0x5193, 0x70),
        (0x5194, 0xF0),
        (0x5195, 0xF0),
        (0x5196, 0x03),
        (0x5197, 0x01),
        (0x5198, 0x04),
        (0x5199, 0x12),
        (0x519A, 0x04),
        (0x519B, 0x00),
        (0x519C, 0x06),
        (0x519D, 0x82),
        (0x519E, 0x38),
        // colour matrix
        (0x5381, 0x1E),
        (0x5382, 0x5B),
        (0x5383, 0x08),
        (0x5384, 0x0A),
        (0x5385, 0x7E),
        (0x5386, 0x88),
        (0x5387, 0x7C),
        (0x5388, 0x6C),
        (0x5389, 0x10),
        (0x538A, 0x01),
        (0x538B, 0x98),
        // sharpness and denoise
        (0x5300, 0x10),
        (0x5301, 0x10),
        (0x5302, 0x18),
        (0x5303, 0x19),
        (0x5304, 0x10),
        (0x5305, 0x10),
        (0x5306, 0x08),
        (0x5307, 0x16),
        (0x5308, 0x40),
        (0x5309, 0x10),
        (0x530A, 0x10),
        (0x530B, 0x04),
        (0x530C, 0x06),
        // gamma
        (0x5480, 0x01),
        (0x5481, 0x00),
        (0x5482, 0x1E),
        (0x5483, 0x3B),
        (0x5484, 0x58),
        (0x5485, 0x66),
        (0x5486, 0x71),
        (0x5487, 0x7D),
        (0x5488, 0x83),
        (0x5489, 0x8F),
        (0x548A, 0x98),
        (0x548B, 0xA6),
        (0x548C, 0xB8),
        (0x548D, 0xCA),
        (0x548E, 0xD7),
        (0x548F, 0xE3),
        (0x5490, 0x1D),
        // special digital effects: brightness, contrast, saturation
        (0x5580, 0x06),
        (0x5583, 0x40),
        (0x5584, 0x10),
        (0x5586, 0x20),
        (0x5587, 0x00),
        (0x5588, 0x00),
        (0x5589, 0x10),
        (0x558A, 0x00),
        (0x558B, 0xF8),
        (0x501D, 0x40),
        (REG_SYSTEM_CTRL0, 0x02), // power up
        (0x3C00, 0x04),           // 50 Hz
];

pub struct Ov5640<B: I2cBus> {
        bus: B,
        mirror: bool,
        flip: bool,
}

impl<B: I2cBus> Ov5640<B> {
        pub fn new(bus: B) -> Self {
                Self { bus, mirror: false, flip: false }
        }

        fn write(&mut self, reg: u16, value: u8) -> Result<(), CameraError> {
                Ok(self.bus.write_register16(I2C_ADDR, reg, &[value])?)
        }

        /// A 16-bit value across two consecutive registers, high byte first.
        fn write16(&mut self, reg: u16, value: u16) -> Result<(), CameraError> {
                self.write(reg, (value >> 8) as u8)?;
                self.write(reg + 1, value as u8)
        }

        /// Two 16-bit values across four consecutive registers: the X/Y pairs the timing block uses.
        fn write_xy(&mut self, reg: u16, x: u16, y: u16) -> Result<(), CameraError> {
                self.write16(reg, x)?;
                self.write16(reg + 2, y)
        }

        pub fn read(&mut self, reg: u16) -> Result<u8, CameraError> {
                let mut v = [0u8];
                self.bus.read_register16(I2C_ADDR, reg, &mut v)?;
                Ok(v[0])
        }

        /// The chip id register pair. The part needs its master clock running to answer at all.
        pub fn chip_id(&mut self) -> Result<u16, CameraError> {
                Ok(u16::from(self.read(REG_CHIP_ID)?) << 8 | u16::from(self.read(REG_CHIP_ID + 1)?))
        }

        /// Reset the part and configure it to stream `width` x `height` RGB565 at the PCLK `pll`
        /// gives. Blocking (about half a second of settling); init only. The size must fit the
        /// binned window, 1280x960; a size off 4:3 is scaled anamorphically.
        pub fn init(&mut self, clock: &mut dyn Clock, width: u16, height: u16, pll: Pll) -> Result<(), CameraError> {
                if width == 0 || height == 0 || width > BINNED_MAX_W || height > BINNED_MAX_H || width % 2 != 0 {
                        return Err(CameraError::Unsupported);
                }
                let id = self.chip_id()?;
                if id != CHIP_ID {
                        return Err(CameraError::WrongChip(id));
                }
                self.write(REG_SYSTEM_CTRL0, 0x82)?; // software reset
                clock.delay_ms(10);
                for &(reg, value) in DEFAULTS {
                        self.write(reg, value)?;
                }
                self.write(0x3000, 0x20)?; // reset the on-chip MCU
                clock.delay_ms(10);
                for &(reg, value) in DEFAULTS_AFTER_MCU_RESET {
                        self.write(reg, value)?;
                }
                clock.delay_ms(300);

                self.write_xy(REG_TIMING_X_START, 0, 0)?;
                self.write_xy(REG_TIMING_X_END, WINDOW_X_END, WINDOW_Y_END)?;
                self.write_xy(REG_TIMING_OUTPUT, width, height)?;
                //   the line length the binned readout needs: the wider outputs take more
                let total_x = if width > 920 { 2644 } else { 2060 };
                self.write_xy(REG_TIMING_TOTAL, total_x, TOTAL_Y)?;
                self.write_xy(REG_TIMING_OFFSET, OFFSET_X, OFFSET_Y)?;
                let scale = !(width == BINNED_MAX_W && height == BINNED_MAX_H);
                let isp = self.read(REG_ISP_CTRL01)?;
                self.write(REG_ISP_CTRL01, if scale { isp | ISP_SCALE_EN } else { isp & !ISP_SCALE_EN })?;
                self.write_orientation()?;
                self.write(0x4520, 0x0B)?;
                self.write(REG_X_INCREMENT, 0x31)?; // odd 3, even 1: 2x2 binning's subsample
                self.write(REG_Y_INCREMENT, 0x31)?;

                self.write(0x3039, 0x00)?; // PLL not bypassed
                self.write(0x3034, 0x1A)?; // 10-bit MIPI mode, which the DVP PLL path is computed in
                self.write(0x3035, 0x01 | (pll.sys_div & 0x0F) << 4)?;
                self.write(0x3036, if pll.multiplier > 127 { pll.multiplier & 0xFE } else { pll.multiplier })?;
                self.write(0x3037, (pll.pre_div & 0x0F) | if pll.root_2x { 0x10 } else { 0 })?;
                self.write(0x3108, (pll.pclk_root_div & 0x03) << 4 | 0x06)?;
                self.write(0x3824, pll.pclk_div & 0x1F)?;
                self.write(0x460C, 0x22)?;
                self.write(0x3103, 0x13)?;

                self.write(REG_FORMAT_CTRL00, FORMAT_RGB565_BE)?;
                self.write(REG_FORMAT_MUX, MUX_ISP_RGB)?;
                Ok(())
        }

        /// Replace the image with the sensor's colour bars (or bring the image back): the bus,
        /// the capture and the display path proven independently of the optics and exposure.
        pub fn set_test_pattern(&mut self, on: bool) -> Result<(), CameraError> {
                self.write(REG_PRE_ISP_TEST, if on { TEST_COLOR_BAR } else { 0 })
        }

        /// Mirror (left-right) and flip (top-bottom) the image: a fact about how the module is
        /// mounted, so the board's to choose.
        pub fn set_orientation(&mut self, mirror: bool, flip: bool) -> Result<(), CameraError> {
                self.mirror = mirror;
                self.flip = flip;
                self.write_orientation()
        }

        fn write_orientation(&mut self) -> Result<(), CameraError> {
                //   bit 0 of each is the binning enable; bits 2:1 the flip and mirror. The 0x4514
                // value per combination is the sensor vendor's, unexplained in its sheet
                let tc20 = 0x01 | if self.flip { 0x06 } else { 0 };
                let tc21 = 0x01 | if self.mirror { 0x06 } else { 0 };
                let r4514 = if self.mirror != self.flip { 0xBB } else { 0xAA };
                self.write(REG_TIMING_TC20, tc20)?;
                self.write(REG_TIMING_TC21, tc21)?;
                self.write(0x4514, r4514)
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::RefCell;
        use light_core::hal::I2cError;
        use std::collections::BTreeMap;
        use std::rc::Rc;
        use std::vec::Vec;

        extern crate std;

        #[derive(Default)]
        struct MockState {
                regs: BTreeMap<u16, u8>,
                writes: Vec<(u16, u8)>,
        }
        struct MockBus(Rc<RefCell<MockState>>);
        impl I2cBus for MockBus {
                fn read_register(&mut self, _: u8, _: u8, _: &mut [u8]) -> Result<(), I2cError> {
                        panic!("16-bit registers only");
                }
                fn write_register_byte(&mut self, _: u8, _: u8, _: u8) -> Result<(), I2cError> {
                        panic!("16-bit registers only");
                }
                fn read_register16(&mut self, addr: u8, reg: u16, out: &mut [u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        out[0] = *self.0.borrow().regs.get(&reg).unwrap_or(&0);
                        Ok(())
                }
                fn write_register16(&mut self, addr: u8, reg: u16, src: &[u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        assert_eq!(src.len(), 1, "one register per transaction");
                        let mut s = self.0.borrow_mut();
                        s.regs.insert(reg, src[0]);
                        s.writes.push((reg, src[0]));
                        Ok(())
                }
        }

        struct NoClock;
        impl Clock for NoClock {
                fn now_us(&self) -> u64 {
                        0
                }
                fn delay_ms(&mut self, _: u32) {}
        }

        const PLL: Pll = Pll { multiplier: 11, sys_div: 1, pre_div: 1, root_2x: false, pclk_root_div: 1, pclk_div: 4 };

        fn sensor() -> (Ov5640<MockBus>, Rc<RefCell<MockState>>) {
                let state = Rc::new(RefCell::new(MockState::default()));
                state.borrow_mut().regs.insert(0x300A, 0x56);
                state.borrow_mut().regs.insert(0x300B, 0x40);
                (Ov5640::new(MockBus(state.clone())), state)
        }

        fn reg(s: &Rc<RefCell<MockState>>, r: u16) -> u8 {
                *s.borrow().regs.get(&r).unwrap()
        }

        #[test]
        fn the_id_reads_high_byte_first() {
                let (mut cam, _) = sensor();
                assert_eq!(cam.chip_id(), Ok(CHIP_ID));
        }

        #[test]
        fn another_part_is_refused_before_anything_is_written() {
                let (mut cam, state) = sensor();
                state.borrow_mut().regs.insert(0x300B, 0x42);
                assert_eq!(cam.init(&mut NoClock, 320, 240, PLL), Err(CameraError::WrongChip(0x5642)));
                assert!(state.borrow().writes.is_empty());
        }

        #[test]
        fn qvga_configures_the_binned_window_scaled_to_size_in_rgb565() {
                let (mut cam, state) = sensor();
                cam.init(&mut NoClock, 320, 240, PLL).unwrap();
                assert_eq!(state.borrow().writes[0], (REG_SYSTEM_CTRL0, 0x82), "a software reset first");
                //   the output size, high byte first, X then Y
                assert_eq!([reg(&state, 0x3808), reg(&state, 0x3809), reg(&state, 0x380A), reg(&state, 0x380B)], [0x01, 0x40, 0x00, 0xF0]);
                assert_eq!([reg(&state, 0x3804), reg(&state, 0x3805)], [0x0A, 0x3F], "the whole sensor width");
                assert_eq!(reg(&state, REG_ISP_CTRL01) & ISP_SCALE_EN, ISP_SCALE_EN, "scaled down from the binned window");
                assert_eq!(reg(&state, REG_TIMING_TC20) & 0x01, 0x01, "binned");
                assert_eq!(reg(&state, REG_FORMAT_CTRL00), FORMAT_RGB565_BE);
                assert_eq!(reg(&state, 0x3036), 11);
                assert_eq!(reg(&state, 0x3108), 0x16);
                assert_eq!(reg(&state, 0x3824), 4);
                assert_eq!(reg(&state, REG_SYSTEM_CTRL0), 0x02, "powered up");
        }

        #[test]
        fn a_size_past_the_binned_window_is_refused() {
                let (mut cam, _) = sensor();
                assert_eq!(cam.init(&mut NoClock, 1600, 1200, PLL), Err(CameraError::Unsupported));
        }

        #[test]
        fn orientation_keeps_binning_and_sets_both_mirror_bits() {
                let (mut cam, state) = sensor();
                cam.set_orientation(true, false).unwrap();
                assert_eq!(reg(&state, REG_TIMING_TC21), 0x07);
                assert_eq!(reg(&state, REG_TIMING_TC20), 0x01);
                assert_eq!(reg(&state, 0x4514), 0xBB);
                cam.set_test_pattern(true).unwrap();
                assert_eq!(reg(&state, REG_PRE_ISP_TEST), TEST_COLOR_BAR);
        }
}
