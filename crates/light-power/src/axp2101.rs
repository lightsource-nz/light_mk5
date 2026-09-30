//! X-Powers AXP2101: a power-management IC -- battery charger, fuel gauge, power path and the
//! board's rails -- with 8-bit registers at address 0x34. Not a [`crate::PowerSource`]: it
//! negotiates nothing with the supply. It is what a board's power mechanism reads for the
//! battery, the charger and external-power presence, and writes to power the board off.
//!
//! Register map and scaling per the part's datasheet, cross-checked against a widely used open
//! driver. What this driver deliberately leaves alone: every rail's enable and voltage (a board
//! whose main 3V3 comes from a DCDC here can be browned out by one wrong write), and the charger's
//! current and voltage targets (a fact about the fitted cell, the board's to decide).
//!
//! The driver holds nothing but its bus, so a copy of it is as good as the original -- which is
//! how a caller with only a shared reference (a status query) still reaches the hardware over a
//! shared-bus handle that is itself `Copy`.

use light_core::hal::{I2cBus, I2cError};

pub const I2C_ADDR: u8 = 0x34;

const REG_STATUS1: u8 = 0x00;
const REG_STATUS2: u8 = 0x01;
const REG_CHIP_ID: u8 = 0x03;
const REG_COMMON_CFG: u8 = 0x10;
const REG_ADC_ENABLE: u8 = 0x30;
const REG_VBAT_H: u8 = 0x34;
const REG_VBUS_H: u8 = 0x38;
const REG_VSYS_H: u8 = 0x3A;
const REG_TDIE_H: u8 = 0x3C;
const REG_IRQ_EN1: u8 = 0x40;
const REG_IRQ_EN2: u8 = 0x41;
const REG_IRQ_EN3: u8 = 0x42;
const REG_IRQ_STATUS1: u8 = 0x48;
const REG_IRQ_STATUS2: u8 = 0x49;
const REG_IRQ_STATUS3: u8 = 0x4A;
const REG_BAT_DETECT: u8 = 0x68;
const REG_GAUGE_PERCENT: u8 = 0xA4;

pub const CHIP_ID: u8 = 0x4A;

const STATUS1_VBUS_GOOD: u8 = 1 << 5;
const STATUS1_BATTERY_PRESENT: u8 = 1 << 3;
const COMMON_CFG_SOFT_POWER_OFF: u8 = 1 << 0;
/// The measurement channels: battery, external input, system rail and die temperature.
const ADC_CHANNELS: u8 = (1 << 0) | (1 << 2) | (1 << 3) | (1 << 4);
const BAT_DETECT_ENABLE: u8 = 1 << 0;

/// The power key's interrupt bits, in the second enable and status registers.
pub const KEY_SHORT: u8 = 1 << 3;
pub const KEY_LONG: u8 = 1 << 2;
pub const KEY_PRESS: u8 = 1 << 1;
pub const KEY_RELEASE: u8 = 1 << 0;
const KEY_ALL: u8 = KEY_SHORT | KEY_LONG | KEY_PRESS | KEY_RELEASE;

/// Where the charger is in its cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Charge {
        Trickle,
        Precharge,
        ConstantCurrent,
        ConstantVoltage,
        Done,
        NotCharging,
}

/// One read of the two status registers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Status {
        /// A usable external input is present -- a real presence signal, independent of whether
        /// anything enumerated over it.
        pub vbus_good: bool,
        pub battery_present: bool,
        pub charge: Charge,
}

/// Power-key events latched since the last [`Axp2101::take_key_events`], as `KEY_*` bits.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct KeyEvents(pub u8);

impl KeyEvents {
        pub fn short(self) -> bool {
                self.0 & KEY_SHORT != 0
        }
        pub fn long(self) -> bool {
                self.0 & KEY_LONG != 0
        }
        pub fn pressed(self) -> bool {
                self.0 & KEY_PRESS != 0
        }
        pub fn released(self) -> bool {
                self.0 & KEY_RELEASE != 0
        }
        pub fn any(self) -> bool {
                self.0 != 0
        }
}

#[derive(Clone, Copy)]
pub struct Axp2101<B: I2cBus> {
        bus: B,
}

impl<B: I2cBus> Axp2101<B> {
        pub fn new(bus: B) -> Self {
                Self { bus }
        }

        fn read(&mut self, reg: u8) -> Result<u8, I2cError> {
                let mut v = [0u8];
                self.bus.read_register(I2C_ADDR, reg, &mut v)?;
                Ok(v[0])
        }

        fn write(&mut self, reg: u8, value: u8) -> Result<(), I2cError> {
                self.bus.write_register_byte(I2C_ADDR, reg, value)
        }

        /// Read-modify-write: most registers here mix fields, and some carry reserved or control
        /// bits a whole-byte write would clobber.
        fn modify(&mut self, reg: u8, clear: u8, set: u8) -> Result<(), I2cError> {
                let v = self.read(reg)?;
                self.write(reg, (v & !clear) | set)
        }

        /// A 14-bit measurement: the high byte first (the part latches the pair on that read),
        /// masked because its upper bits are control fields.
        fn read_adc(&mut self, reg_h: u8) -> Result<u16, I2cError> {
                let h = self.read(reg_h)?;
                let l = self.read(reg_h + 1)?;
                Ok((u16::from(h & 0x3F) << 8) | u16::from(l))
        }

        /// `true` when the chip id answers as this part.
        pub fn probe(&mut self) -> Result<bool, I2cError> {
                Ok(self.read(REG_CHIP_ID)? == CHIP_ID)
        }

        /// Turn on the measurements this driver reports and battery detection, and route the power
        /// key's edges and presses to the interrupt status -- masking every other source, so the
        /// IRQ line is not held asserted by events nobody reads. Latched status is cleared.
        pub fn init(&mut self) -> Result<(), I2cError> {
                self.modify(REG_ADC_ENABLE, 0, ADC_CHANNELS)?;
                self.modify(REG_BAT_DETECT, 0, BAT_DETECT_ENABLE)?;
                self.write(REG_IRQ_EN1, 0)?;
                self.write(REG_IRQ_EN2, KEY_ALL)?;
                self.write(REG_IRQ_EN3, 0)?;
                //   the status registers are write-1-to-clear: never read-modify-write them
                self.write(REG_IRQ_STATUS1, 0xFF)?;
                self.write(REG_IRQ_STATUS2, 0xFF)?;
                self.write(REG_IRQ_STATUS3, 0xFF)
        }

        pub fn status(&mut self) -> Result<Status, I2cError> {
                let s1 = self.read(REG_STATUS1)?;
                let s2 = self.read(REG_STATUS2)?;
                let charge = match s2 & 0x07 {
                        0 => Charge::Trickle,
                        1 => Charge::Precharge,
                        2 => Charge::ConstantCurrent,
                        3 => Charge::ConstantVoltage,
                        4 => Charge::Done,
                        _ => Charge::NotCharging,
                };
                Ok(Status { vbus_good: s1 & STATUS1_VBUS_GOOD != 0, battery_present: s1 & STATUS1_BATTERY_PRESENT != 0, charge })
        }

        /// The battery voltage in millivolts, `None` with no battery fitted (the channel then
        /// reads the charger's regulation, not a cell).
        pub fn battery_mv(&mut self) -> Result<Option<u16>, I2cError> {
                if !self.status()?.battery_present {
                        return Ok(None);
                }
                self.read_adc(REG_VBAT_H).map(Some)
        }

        /// The external input's voltage in millivolts.
        pub fn vbus_mv(&mut self) -> Result<u16, I2cError> {
                self.read_adc(REG_VBUS_H)
        }

        /// The system rail's voltage in millivolts.
        pub fn vsys_mv(&mut self) -> Result<u16, I2cError> {
                self.read_adc(REG_VSYS_H)
        }

        /// The die temperature in tenths of a degree Celsius.
        pub fn die_decidegrees(&mut self) -> Result<i32, I2cError> {
                let raw = i32::from(self.read_adc(REG_TDIE_H)?);
                //   22 C + (7274 - raw) / 20, in tenths
                Ok(220 + (7274 - raw) / 2)
        }

        /// The fuel gauge's state of charge, 0..=100, `None` with no battery fitted.
        pub fn battery_percent(&mut self) -> Result<Option<u8>, I2cError> {
                if !self.status()?.battery_present {
                        return Ok(None);
                }
                Ok(Some(self.read(REG_GAUGE_PERCENT)?.min(100)))
        }

        /// Take the power-key events latched since the last call, clearing exactly those.
        pub fn take_key_events(&mut self) -> Result<KeyEvents, I2cError> {
                let latched = self.read(REG_IRQ_STATUS2)? & KEY_ALL;
                if latched != 0 {
                        self.write(REG_IRQ_STATUS2, latched)?;
                }
                Ok(KeyEvents(latched))
        }

        /// Shut every output down but the RTC's. The board goes dark; what brings it back is the
        /// power key, or an external input arriving, per the part's configuration.
        pub fn power_off(&mut self) -> Result<(), I2cError> {
                self.modify(REG_COMMON_CFG, 0, COMMON_CFG_SOFT_POWER_OFF)
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::RefCell;
        use std::vec::Vec;

        extern crate std;

        /// A register file: reads return what is stored, writes store (and are logged). The two
        /// IRQ status registers clear the bits written as 1, like the part.
        struct Regs {
                r: [u8; 256],
                writes: Vec<(u8, u8)>,
        }
        impl Regs {
                fn new() -> RefCell<Self> {
                        let mut r = [0u8; 256];
                        r[REG_CHIP_ID as usize] = CHIP_ID;
                        RefCell::new(Self { r, writes: Vec::new() })
                }
        }
        struct Bus<'a>(&'a RefCell<Regs>);
        impl I2cBus for Bus<'_> {
                fn read_register(&mut self, addr: u8, reg: u8, out: &mut [u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        out[0] = self.0.borrow().r[reg as usize];
                        Ok(())
                }
                fn write_register_byte(&mut self, addr: u8, reg: u8, value: u8) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        let mut s = self.0.borrow_mut();
                        s.writes.push((reg, value));
                        match reg {
                                REG_IRQ_STATUS1 | REG_IRQ_STATUS2 | REG_IRQ_STATUS3 => s.r[reg as usize] &= !value,
                                _ => s.r[reg as usize] = value,
                        }
                        Ok(())
                }
        }

        #[test]
        fn the_probe_checks_the_chip_id() {
                let regs = Regs::new();
                assert_eq!(Axp2101::new(Bus(&regs)).probe(), Ok(true));
                regs.borrow_mut().r[REG_CHIP_ID as usize] = 0x47;
                assert_eq!(Axp2101::new(Bus(&regs)).probe(), Ok(false));
        }

        #[test]
        fn init_preserves_neighbouring_bits_and_touches_no_rail() {
                let regs = Regs::new();
                //   the TS channel (bit 1) is on, and a control bit sits beside battery detection
                regs.borrow_mut().r[REG_ADC_ENABLE as usize] = 0b0000_0010;
                regs.borrow_mut().r[REG_BAT_DETECT as usize] = 0b1000_0000;
                Axp2101::new(Bus(&regs)).init().unwrap();
                let s = regs.borrow();
                assert_eq!(s.r[REG_ADC_ENABLE as usize], 0b0001_1111);
                assert_eq!(s.r[REG_BAT_DETECT as usize], 0b1000_0001);
                assert_eq!(s.r[REG_IRQ_EN2 as usize], KEY_ALL);
                //   nothing at or above the rail registers is written
                assert!(s.writes.iter().all(|&(reg, _)| reg < 0x80));
        }

        #[test]
        fn status_decodes_presence_and_the_charge_cycle() {
                let regs = Regs::new();
                regs.borrow_mut().r[REG_STATUS1 as usize] = STATUS1_VBUS_GOOD | STATUS1_BATTERY_PRESENT;
                regs.borrow_mut().r[REG_STATUS2 as usize] = 0b0010_0010; // charging, constant current
                let s = Axp2101::new(Bus(&regs)).status().unwrap();
                assert_eq!(s, Status { vbus_good: true, battery_present: true, charge: Charge::ConstantCurrent });
                regs.borrow_mut().r[REG_STATUS2 as usize] = 0b0000_0100;
                assert_eq!(Axp2101::new(Bus(&regs)).status().unwrap().charge, Charge::Done);
        }

        #[test]
        fn battery_readings_mask_the_control_bits_and_need_a_cell() {
                let regs = Regs::new();
                //   4012 mV = 0x0FAC, with a control bit set above the 14-bit value
                regs.borrow_mut().r[REG_VBAT_H as usize] = 0x80 | 0x0F;
                regs.borrow_mut().r[REG_VBAT_H as usize + 1] = 0xAC;
                regs.borrow_mut().r[REG_GAUGE_PERCENT as usize] = 87;
                let mut p = Axp2101::new(Bus(&regs));
                assert_eq!(p.battery_mv(), Ok(None));
                assert_eq!(p.battery_percent(), Ok(None));
                regs.borrow_mut().r[REG_STATUS1 as usize] = STATUS1_BATTERY_PRESENT;
                assert_eq!(p.battery_mv(), Ok(Some(4012)));
                assert_eq!(p.battery_percent(), Ok(Some(87)));
        }

        #[test]
        fn key_events_are_taken_once() {
                let regs = Regs::new();
                regs.borrow_mut().r[REG_IRQ_STATUS2 as usize] = KEY_PRESS | KEY_SHORT | 0x80;
                let mut p = Axp2101::new(Bus(&regs));
                let ev = p.take_key_events().unwrap();
                assert!(ev.pressed() && ev.short() && !ev.long() && !ev.released());
                //   only the key bits were cleared; an unrelated source is left for its owner
                assert_eq!(regs.borrow().r[REG_IRQ_STATUS2 as usize], 0x80);
                assert_eq!(p.take_key_events().unwrap(), KeyEvents(0));
        }

        #[test]
        fn power_off_sets_only_the_soft_off_bit() {
                let regs = Regs::new();
                regs.borrow_mut().r[REG_COMMON_CFG as usize] = 0b0011_0000;
                Axp2101::new(Bus(&regs)).power_off().unwrap();
                assert_eq!(regs.borrow().r[REG_COMMON_CFG as usize], 0b0011_0001);
        }

        #[test]
        fn the_die_temperature_follows_the_part_formula() {
                let regs = Regs::new();
                //   raw 7274 is 22.0 C; each 20 counts below is a degree warmer
                regs.borrow_mut().r[REG_TDIE_H as usize] = (7274u16 >> 8) as u8;
                regs.borrow_mut().r[REG_TDIE_H as usize + 1] = 7274u16 as u8;
                assert_eq!(Axp2101::new(Bus(&regs)).die_decidegrees(), Ok(220));
                let raw = 7274u16 - 200;
                regs.borrow_mut().r[REG_TDIE_H as usize] = (raw >> 8) as u8;
                regs.borrow_mut().r[REG_TDIE_H as usize + 1] = raw as u8;
                assert_eq!(Axp2101::new(Bus(&regs)).die_decidegrees(), Ok(320));
        }
}
