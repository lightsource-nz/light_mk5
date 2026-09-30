//! This board's [`PowerMechanism`]: the backlight is a transistor low-side switch (high = lit), and
//! external power is USB enumeration. The board's power is held by its AXP2101 PMIC, not a GPIO
//! latch, so powering off and the battery gauge both go through that part -- which has no driver
//! yet, so both are left at the trait's defaults (no-op, no gauge). The power key reaches a GPIO
//! only through a level shifter whose idle level is unconfirmed, so it is not wired as the button
//! until it has been read on the hardware.

use light_power_manager::PowerMechanism;
use light_rp2::pwm::PwmOutput;

use crate::board::BACKLIGHT_LEVEL_MAX;

pub struct Touch35Power {
        pub backlight: PwmOutput,
}

impl PowerMechanism for Touch35Power {
        fn set_backlight(&mut self, level: u16) {
                self.backlight.set_duty(level.min(BACKLIGHT_LEVEL_MAX));
        }

        fn on_external_power(&self) -> bool {
                // published by the port's console loop on core 1, which owns the USB device stack
                light_rp2::shell::usb_mounted()
        }
}
