//! A UART as a console transport: UART0, polled, never blocking. On a device-role board it is the
//! console's second path -- the one a debug probe's serial adapter carries when the board's own
//! USB port is not cabled -- and on a USB-host board (whose port hosts instruments) it is the only
//! console. The core-1 loop in `shell` drives it beside the USB CDC class: a log line goes to both,
//! input is taken from either.
//!
//! Same silicon block on both chips (the PL011), the same registers, the same GPIO function.

use crate::gpio;
use crate::pac;

/// UART0, 8N1, FIFOs on.
pub struct Uart {
        _private: (),
}

impl Uart {
        /// Take UART0 on `tx`/`rx` at `baud`, clocked from `peri_hz` (the peripheral clock the
        /// shell reports in `ShellInfo`).
        ///
        /// # Safety
        /// Constructs the one owner of UART0; call it once.
        pub unsafe fn new(tx: usize, rx: usize, baud: u32, peri_hz: u32) -> Self {
                let resets = unsafe { &*pac::RESETS::ptr() };
                resets.reset().modify(|_, w| w.uart0().set_bit());
                resets.reset().modify(|_, w| w.uart0().clear_bit());
                while resets.reset_done().read().uart0().bit_is_clear() {}

                let uart = unsafe { &*pac::UART0::ptr() };
                //   the PL011's baud divisor: a 16.6 fixed-point value of peri / (16 * baud). The
                // integer part is uartibrd, the fraction's top six bits uartfbrd, computed the way
                // the SDK does (8 * peri / baud, then split) so a given baud lands on the same
                // divisor as an SDK console
                let div = (8 * u64::from(peri_hz) / u64::from(baud)) as u32;
                let mut ibrd = div >> 7;
                let mut fbrd = ((div & 0x7f) + 1) / 2;
                if ibrd == 0 {
                        ibrd = 1;
                        fbrd = 0;
                } else if ibrd >= 65535 {
                        ibrd = 65535;
                        fbrd = 0;
                }
                uart.uartibrd().write(|w| unsafe { w.baud_divint().bits(ibrd as u16) });
                uart.uartfbrd().write(|w| unsafe { w.baud_divfrac().bits(fbrd as u8) });
                // 8 data bits, no parity, one stop bit, FIFOs on. The line-control write is also
                // what latches the divisor
                uart.uartlcr_h().write(|w| unsafe { w.wlen().bits(3).fen().set_bit() });
                uart.uartcr().write(|w| w.uarten().set_bit().txe().set_bit().rxe().set_bit());

                gpio::set_function(tx, gpio::FUNC_UART);
                gpio::set_function(rx, gpio::FUNC_UART);
                Self { _private: () }
        }

        /// Queue as many of `bytes` as the transmit FIFO has room for, and return how many that
        /// was. Never waits: what does not fit is the caller's to drop, which is what the console
        /// does -- logging never blocks the loop.
        pub fn write(&mut self, bytes: &[u8]) -> usize {
                let uart = unsafe { &*pac::UART0::ptr() };
                let mut n = 0;
                for &b in bytes {
                        if uart.uartfr().read().txff().bit_is_set() {
                                break;
                        }
                        uart.uartdr().write(|w| unsafe { w.data().bits(b) });
                        n += 1;
                }
                n
        }

        /// One received byte, or `None` when the receive FIFO is empty.
        pub fn read(&mut self) -> Option<u8> {
                let uart = unsafe { &*pac::UART0::ptr() };
                if uart.uartfr().read().rxfe().bit_is_set() {
                        return None;
                }
                Some(uart.uartdr().read().data().bits())
        }
}
