//! FocalTech FT6336U: a two-point self-capacitive touch controller with 8-bit registers, at
//! address 0x38. One read from `TD_STATUS` (0x02) carries the touch count and the first point:
//! `[count, XH, XL, YH, YL]`, where the top two bits of XH are the event flag (0 press down,
//! 1 lift up, 2 contact) and the low nibbles of XH/YH are coordinate bits 11..8.
//!
//! Reports are EXPLICIT in both directions -- a lift reads back as a zero count -- so this
//! driver trusts them, with the report-silence timeout kept only as a stall backstop. Only the
//! first point is tracked; the framework's tracker classifies. The part has a gesture register,
//! but it is a separate mode that suppresses coordinates, so this driver declines hardware
//! gestures and leaves classification to the software tracker.
//!
//! In its default mode the part holds INT low for as long as a finger is down, rather than
//! pulsing it, which is what the shared poll discipline's "INT says a read is allowed" wants.

use light_core::hal::{Clock, I2cBus, I2cError, InputPin, OutputPin};

pub use crate::TouchSample as Event;

pub const I2C_ADDR: u8 = 0x38;

const REG_TD_STATUS: u8 = 0x02;
const REG_CHIP_ID: u8 = 0xA3;
/// The chip ids this driver speaks to: the FT6336U, and the FT6236/FT6206 siblings that share
/// the register map.
const CHIP_IDS: [u8; 3] = [0x64, 0x36, 0x06];

const EVENT_LIFT_UP: u8 = 1;
/// More than this many points is a report the part makes while idle (an 0x0F count), not a
/// touch.
const MAX_POINTS: u8 = 2;

const POLL_INTERVAL_MS: u32 = 10;
const INT_READ_FLOOR_MS: u32 = 4;
const BACKOFF_MAX_MS: u32 = 160;
const QUIET_AFTER_FAILS: u8 = 4;
/// The stall backstop only: releases normally arrive as explicit zero-count reports.
const RELEASE_TIMEOUT_MS: u32 = 200;
const RELEASE_MIN_POLLS: u32 = 8;

/// Reset line timing: held low, then the part's boot time before it answers the bus.
const RESET_HOLD_MS: u32 = 10;
const RESET_BOOT_MS: u32 = 300;

/// How raw panel coordinates map onto the display's frame, measured on bring-up.
#[derive(Clone, Copy, Debug)]
pub struct CoordMap {
        pub x_max: u16,
        pub y_max: u16,
        pub invert_x: bool,
        pub invert_y: bool,
        /// Applied AFTER the inversions: x and y exchange.
        pub swap_xy: bool,
}

pub struct Ft6336<B: I2cBus, I: InputPin, R: OutputPin> {
        bus: B,
        int: I,
        reset: R,
        map: CoordMap,
        pub active: bool,
        pub x: u16,
        pub y: u16,
        last_active_ms: u32,
        last_attempt_ms: u32,
        idle_polls: u32,
        unanswered: u8,
        pub failures: u32,
        pub nacks: u32,
        pub timeouts: u32,
        pub bus_errors: u32,
}

impl<B: I2cBus, I: InputPin, R: OutputPin> crate::touch::TouchController for Ft6336<B, I, R> {
        fn reset(&mut self, clock: &mut dyn Clock) {
                self.reset_blocking(clock);
        }
        fn probe(&mut self) -> Result<(), I2cError> {
                self.probe().map(|_| ())
        }
        fn poll(&mut self, now_ms: u32) -> Option<Event> {
                self.poll(now_ms)
        }
        fn diagnostics(&self) -> crate::touch::TouchDiagnostics {
                crate::touch::TouchDiagnostics { failures: self.failures, nacks: self.nacks, timeouts: self.timeouts, bus_errors: self.bus_errors }
        }
}

impl<B: I2cBus, I: InputPin, R: OutputPin> crate::touch::HardwareGestures for Ft6336<B, I, R> {
        fn read_gesture(&mut self) -> Option<crate::touch::Swipe> {
                None
        }
}

impl<B: I2cBus, I: InputPin, R: OutputPin> Ft6336<B, I, R> {
        pub fn new(bus: B, int: I, reset: R, map: CoordMap, now_ms: u32) -> Self {
                Self {
                        bus,
                        int,
                        reset,
                        map,
                        active: false,
                        x: 0,
                        y: 0,
                        last_active_ms: now_ms,
                        last_attempt_ms: now_ms,
                        idle_polls: 0,
                        unanswered: 0,
                        failures: 0,
                        nacks: 0,
                        timeouts: 0,
                        bus_errors: 0,
                }
        }

        /// Pulse the reset line and wait out the boot. For init only, where nothing is
        /// rendering and blocking is correct.
        pub fn reset_blocking(&mut self, clock: &mut dyn Clock) {
                self.reset.set(true);
                clock.delay_ms(RESET_HOLD_MS);
                self.reset.set(false);
                clock.delay_ms(RESET_HOLD_MS);
                self.reset.set(true);
                clock.delay_ms(RESET_BOOT_MS);
        }

        /// The chip id, `Some` when it is one this driver speaks to.
        pub fn probe(&mut self) -> Result<Option<u8>, I2cError> {
                let mut id = [0u8];
                self.bus.read_register(I2C_ADDR, REG_CHIP_ID, &mut id)?;
                Ok(if CHIP_IDS.contains(&id[0]) { Some(id[0]) } else { None })
        }

        fn interval_ms(&self) -> u32 {
                (POLL_INTERVAL_MS << self.unanswered).min(BACKOFF_MAX_MS)
        }

        fn infer_release(&mut self, now_ms: u32) -> Option<Event> {
                if !self.active || self.idle_polls < RELEASE_MIN_POLLS {
                        return None;
                }
                if now_ms.wrapping_sub(self.last_active_ms) < RELEASE_TIMEOUT_MS {
                        return None;
                }
                self.active = false;
                Some(Event::Up)
        }

        fn idle(&mut self, now_ms: u32) -> Option<Event> {
                self.idle_polls = self.idle_polls.saturating_add(1);
                self.infer_release(now_ms)
        }

        fn fail(&mut self, e: I2cError, now_ms: u32) -> Option<Event> {
                self.failures = self.failures.wrapping_add(1);
                match e {
                        I2cError::Nack => self.nacks += 1,
                        I2cError::Timeout => self.timeouts += 1,
                        I2cError::Bus => self.bus_errors += 1,
                }
                if self.unanswered < QUIET_AFTER_FAILS {
                        self.unanswered += 1;
                }
                self.idle(now_ms)
        }

        fn release(&mut self) -> Option<Event> {
                let was_active = self.active;
                self.active = false;
                if was_active { Some(Event::Up) } else { None }
        }

        pub fn poll(&mut self, now_ms: u32) -> Option<Event> {
                let int_asserted = self.int.is_low();
                let quiet = self.unanswered >= QUIET_AFTER_FAILS;
                let allowed = int_asserted || !quiet;
                let since_attempt = now_ms.wrapping_sub(self.last_attempt_ms);
                let due = (int_asserted && self.unanswered == 0 && since_attempt >= INT_READ_FLOOR_MS) || since_attempt >= self.interval_ms();
                if !allowed || !due {
                        return self.idle(now_ms);
                }
                self.last_attempt_ms = now_ms;

                let mut r = [0u8; 5];
                if let Err(e) = self.bus.read_register(I2C_ADDR, REG_TD_STATUS, &mut r) {
                        return self.fail(e, now_ms);
                }
                self.unanswered = 0;
                self.idle_polls = 0;

                let points = r[0] & 0x0F;
                if points == 0 || points > MAX_POINTS || r[1] >> 6 == EVENT_LIFT_UP {
                        //   the explicit release report
                        return self.release();
                }
                let mut x = ((u16::from(r[1] & 0x0F) << 8) | u16::from(r[2])).min(self.map.x_max);
                let mut y = ((u16::from(r[3] & 0x0F) << 8) | u16::from(r[4])).min(self.map.y_max);
                if self.map.invert_x {
                        x = self.map.x_max - x;
                }
                if self.map.invert_y {
                        y = self.map.y_max - y;
                }
                if self.map.swap_xy {
                        core::mem::swap(&mut x, &mut y);
                }
                let was_active = self.active;
                self.last_active_ms = now_ms;
                self.active = true;
                self.x = x;
                self.y = y;
                //   a decoded finger touch is user activity: feed the standard beacon a power
                // manager watches, whatever app or board this is
                light_core::note_activity();
                if was_active { Some(Event::Move { x, y }) } else { Some(Event::Down { x, y }) }
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::{Cell, RefCell};
        use std::rc::Rc;

        extern crate std;

        struct MockBus {
                frame: Rc<RefCell<[u8; 5]>>,
                id: u8,
                fail: Rc<Cell<bool>>,
        }
        impl I2cBus for MockBus {
                fn read_register(&mut self, addr: u8, reg: u8, out: &mut [u8]) -> Result<(), I2cError> {
                        assert_eq!(addr, I2C_ADDR);
                        if self.fail.get() {
                                return Err(I2cError::Nack);
                        }
                        match reg {
                                REG_TD_STATUS => out.copy_from_slice(&self.frame.borrow()[..out.len()]),
                                REG_CHIP_ID => out[0] = self.id,
                                _ => panic!("unexpected register {reg:#x}"),
                        }
                        Ok(())
                }
                fn write_register_byte(&mut self, _: u8, _: u8, _: u8) -> Result<(), I2cError> {
                        panic!("the driver writes nothing");
                }
        }
        struct MockInt(Rc<Cell<bool>>);
        impl InputPin for MockInt {
                fn is_low(&self) -> bool {
                        self.0.get()
                }
        }
        struct MockReset;
        impl OutputPin for MockReset {
                fn set(&mut self, _: bool) {}
        }

        const MAP: CoordMap = CoordMap { x_max: 319, y_max: 479, invert_x: false, invert_y: false, swap_xy: false };

        struct Rig {
                frame: Rc<RefCell<[u8; 5]>>,
                fail: Rc<Cell<bool>>,
                t: Ft6336<MockBus, MockInt, MockReset>,
        }

        fn rig(map: CoordMap, id: u8) -> Rig {
                let frame = Rc::new(RefCell::new([0u8; 5]));
                let fail = Rc::new(Cell::new(false));
                let int = Rc::new(Cell::new(true));
                let t = Ft6336::new(MockBus { frame: frame.clone(), id, fail: fail.clone() }, MockInt(int), MockReset, map, 0);
                Rig { frame, fail, t }
        }

        /// A one-point report: event flag 2 (contact) with the 12-bit packing.
        fn point(x: u16, y: u16) -> [u8; 5] {
                [1, 0x80 | (x >> 8) as u8, x as u8, (y >> 8) as u8, y as u8]
        }

        #[test]
        fn the_probe_accepts_the_family_and_refuses_anything_else() {
                assert_eq!(rig(MAP, 0x64).t.probe(), Ok(Some(0x64)));
                assert_eq!(rig(MAP, 0x36).t.probe(), Ok(Some(0x36)));
                assert_eq!(rig(MAP, 0xB5).t.probe(), Ok(None));
        }

        #[test]
        fn a_report_parses_moves_and_the_release_is_explicit() {
                let mut r = rig(MAP, 0x64);
                *r.frame.borrow_mut() = point(300, 460);
                assert_eq!(r.t.poll(POLL_INTERVAL_MS), Some(Event::Down { x: 300, y: 460 }));
                *r.frame.borrow_mut() = point(290, 470);
                assert_eq!(r.t.poll(POLL_INTERVAL_MS * 2), Some(Event::Move { x: 290, y: 470 }));
                //   a zero count ends the touch
                *r.frame.borrow_mut() = [0, 0xFF, 0xFF, 0xFF, 0xFF];
                assert_eq!(r.t.poll(POLL_INTERVAL_MS * 3), Some(Event::Up));
                assert!(!r.t.active);
                //   and silence after it is silence, not a second release
                assert_eq!(r.t.poll(POLL_INTERVAL_MS * 4), None);
        }

        #[test]
        fn a_lift_up_flag_and_an_idle_count_both_release() {
                let mut r = rig(MAP, 0x64);
                *r.frame.borrow_mut() = point(10, 20);
                assert_eq!(r.t.poll(POLL_INTERVAL_MS), Some(Event::Down { x: 10, y: 20 }));
                *r.frame.borrow_mut() = [1, 0x40, 10, 0, 20];
                assert_eq!(r.t.poll(POLL_INTERVAL_MS * 2), Some(Event::Up));
                *r.frame.borrow_mut() = point(10, 20);
                assert_eq!(r.t.poll(POLL_INTERVAL_MS * 3), Some(Event::Down { x: 10, y: 20 }));
                *r.frame.borrow_mut() = [0x0F, 0xFF, 0xFF, 0xFF, 0xFF];
                assert_eq!(r.t.poll(POLL_INTERVAL_MS * 4), Some(Event::Up));
        }

        #[test]
        fn the_map_applies_inversions_then_the_swap() {
                let mut r = rig(CoordMap { x_max: 319, y_max: 479, invert_x: true, invert_y: false, swap_xy: true }, 0x64);
                *r.frame.borrow_mut() = point(19, 30);
                //   x raw 19 -> inverted 300; then the swap: x=30, y=300
                assert_eq!(r.t.poll(POLL_INTERVAL_MS), Some(Event::Down { x: 30, y: 300 }));
        }

        #[test]
        fn failures_are_counted_and_back_off() {
                let mut r = rig(MAP, 0x64);
                r.fail.set(true);
                assert_eq!(r.t.poll(POLL_INTERVAL_MS), None);
                assert_eq!(r.t.failures, 1);
                assert_eq!(r.t.nacks, 1);
                //   the next read waits out the doubled interval
                assert_eq!(r.t.poll(POLL_INTERVAL_MS * 2), None);
                assert_eq!(r.t.failures, 1);
                assert_eq!(r.t.poll(POLL_INTERVAL_MS * 3), None);
                assert_eq!(r.t.failures, 2);
        }
}
