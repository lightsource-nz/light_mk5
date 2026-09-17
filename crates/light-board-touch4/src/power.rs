//! This board's [`PowerMechanism`]: just the backlight (inverted, near-linear). The 4" has a
//! battery and charge-status pins but NO power latch, so it cannot cut its own power; leaving
//! `on_external_power` at its `true` default correctly keeps the policy from ever trying an
//! unrecoverable shutdown, and the board just dims and wakes. Battery and charge status stay in the
//! app's board module for the `stats` readout.

use light_power_manager::PowerMechanism;
use light_rp2::pwm::PwmOutput;

use crate::board::{BACKLIGHT_INVERTED, BACKLIGHT_LEVEL_MAX};

pub struct Touch4Power {
        pub backlight: PwmOutput,
}

impl PowerMechanism for Touch4Power {
        fn set_backlight(&mut self, level: u16) {
                //   plain linear (no threshold band measured for this panel), inverted like the 3.49
                let duty = if BACKLIGHT_INVERTED { BACKLIGHT_LEVEL_MAX - level.min(BACKLIGHT_LEVEL_MAX) } else { level };
                self.backlight.set_duty(duty);
        }
}
