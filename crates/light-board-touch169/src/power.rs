//! This board's [`PowerMechanism`]: just the backlight (a non-inverted, near-linear PWM drive).
//! The 1.69 has no battery, latch or power button, so the trait defaults do the rest -- most
//! importantly `on_external_power` defaults to `true`, which correctly disables the on-battery
//! power-off on a board that only ever runs from USB. Touch and IMU feed the activity beacon through
//! their shared drivers, so the screen still dims on idle and wakes on use.

use light_power_manager::PowerMechanism;
use light_rp2::pwm::PwmOutput;

use crate::board::BACKLIGHT_LEVEL_MAX;

pub struct Touch169Power {
        pub backlight: PwmOutput,
}

impl PowerMechanism for Touch169Power {
        fn set_backlight(&mut self, level: u16) {
                self.backlight.set_duty(level.min(BACKLIGHT_LEVEL_MAX));
        }
}
