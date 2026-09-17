//! This board's [`PowerMechanism`]: the backlight is an NPN low-side switch (high = lit, linear, no
//! floor), the battery latch powers it off, the side key is the button, and there is no battery
//! gauge. External power is USB enumeration -- this board has no VBUS pin, so enumeration is its "on
//! external power" signal, as on the 3.49.

use light_core::InputPin;
use light_power_manager::PowerMechanism;
use light_rp2::gpio::{Input, Output};
use light_rp2::pwm::PwmOutput;

use crate::board::BACKLIGHT_LEVEL_MAX;

unsafe extern "C" {
        /// True while a USB host has this device enumerated; set on core 1 by the shell.
        fn light_shell_usb_mounted() -> bool;
}

pub struct Touch28Power {
        pub backlight: PwmOutput,
        pub bat_en: Output,
        pub key_bat: Input,
}

impl PowerMechanism for Touch28Power {
        fn set_backlight(&mut self, level: u16) {
                //   an NPN low-side PWM switch is close to linear, so the per-mille level is the
                // duty directly -- no floor band like the 349's RC-filtered drive
                self.backlight.set_duty(level.min(BACKLIGHT_LEVEL_MAX));
        }

        fn on_external_power(&self) -> bool {
                unsafe { light_shell_usb_mounted() }
        }

        fn power_off(&mut self) {
                self.bat_en.set(false);
        }

        fn power_button_pressed(&self) -> bool {
                self.key_bat.is_low()
        }
}
