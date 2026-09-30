//! This board's [`PowerMechanism`]: the backlight is a transistor low-side switch (high = lit), and
//! everything else is the AXP2101 PMIC on the shared bus -- the battery voltage, external-power
//! presence (the part's own VBUS-good bit, so a charger that enumerates nothing still counts), and
//! powering off (there is no GPIO latch; the PMIC holds the rails). The power key is read as a live
//! level from its GPIO, which reads HIGH while the key is held (measured on the hardware), because
//! the PMIC itself only latches key edges.

use core::cell::RefCell;
use light_core::InputPin;
use light_power::axp2101::Axp2101;
use light_power_manager::PowerMechanism;
use light_rp2::gpio::Input;
use light_rp2::i2c::I2c1;
use light_rp2::pwm::PwmOutput;

use crate::board::BACKLIGHT_LEVEL_MAX;

/// The PMIC over the board's one shared I2C bus.
pub type Pmic = Axp2101<&'static RefCell<I2c1>>;

pub struct Touch35Power {
        pub backlight: PwmOutput,
        pub pmic: Pmic,
        pub power_key: Input,
}

impl PowerMechanism for Touch35Power {
        fn set_backlight(&mut self, level: u16) {
                self.backlight.set_duty(level.min(BACKLIGHT_LEVEL_MAX));
        }

        fn on_external_power(&self) -> bool {
                //   a PMIC that does not answer is no evidence of running on battery: report
                // external, which only ever suppresses the idle power-off
                let mut pmic = self.pmic;
                pmic.status().map(|s| s.vbus_good).unwrap_or(true)
        }

        fn power_off(&mut self) {
                let _ = self.pmic.power_off();
        }

        fn power_button_pressed(&self) -> bool {
                !self.power_key.is_low()
        }

        fn battery_mv(&mut self) -> Option<u32> {
                self.pmic.battery_mv().ok().flatten().map(u32::from)
        }
}
