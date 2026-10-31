//! The board's wiring: a Raspberry Pi Pico 2 on its own. Its LED, and its BOOTSEL button as the
//! application's button.
//!
//! This is the one place a pin number appears. The application crate names none, and the port
//! knows the chip but nothing about what is connected to it.

use light_core::atomic::{AtomicBool, Ordering};
use light_core::hal::InputPin;
use light_rp2::gpio::Output;

pub const PIN_LED: usize = 25;

/// The BOOTSEL button. It shares a pin with the flash chip's select line, so the port reads it
/// carefully, with interrupts off for a moment; the application samples it at a modest rate.
pub struct Bootsel;

impl InputPin for Bootsel {
        fn is_low(&self) -> bool {
                //   pressed reads as low, the way a button to ground does
                light_rp2::shell::bootsel()
        }
}

pub struct Peripherals {
        pub led: Output,
        pub button: Bootsel,
}

static TAKEN: AtomicBool = AtomicBool::new(false);

/// Configure and hand over the board's peripherals. Once.
pub fn take() -> Option<Peripherals> {
        if TAKEN.swap(true, Ordering::AcqRel) {
                return None;
        }
        //   the flag above makes this the one construction of each peripheral. Constructors that
        // claim a whole block (SPI, I2C, a DMA channel) are `unsafe` for exactly that reason; say
        // why each call honours it beside the call
        Some(Peripherals { led: Output::new(PIN_LED, false), button: Bootsel })
}
