//! USART1 as a console transport: PA9/PA10, the pins every board on this chip brings out for a
//! serial adapter. The F4's USART is the SR/DR generation -- one status register, one data
//! register both ways -- and an overrun is cleared by reading SR then DR. Non-blocking both ways,
//! so the console loop it serves never waits on a wire nothing is listening to.

use light_shell_cmsis::Transport;

use crate::gpio::Pin;
use crate::reg;

const USART1_BASE: usize = 0x4001_1000;
const SR: usize = USART1_BASE + 0x00;
const DR: usize = USART1_BASE + 0x04;
const BRR: usize = USART1_BASE + 0x08;
const CR1: usize = USART1_BASE + 0x0C;

const SR_ORE: u32 = 1 << 3;
const SR_RXNE: u32 = 1 << 5;
const SR_TXE: u32 = 1 << 7;
const CR1_RE: u32 = 1 << 2;
const CR1_TE: u32 = 1 << 3;
const CR1_UE: u32 = 1 << 13;

/// USART1 on APB2.
const APB2ENR_USART1EN: u32 = 1 << 4;
const AF_USART1: u32 = 7;

pub const PIN_TX: Pin = Pin::new('A', 9);
pub const PIN_RX: Pin = Pin::new('A', 10);
pub const BAUD: u32 = 115_200;

/// The console USART. Construct once.
pub struct Usart1 {
        _private: (),
}

impl Usart1 {
        /// Bring USART1 up on PA9/PA10 at `baud`, clocked from APB2 at `apb2_hz`.
        ///
        /// # Safety
        /// Takes the USART1 block; nothing else may touch it while this lives.
        pub unsafe fn new(baud: u32, apb2_hz: u32) -> Self {
                reg::modify(crate::RCC_APB2ENR, 0, APB2ENR_USART1EN);
                let _ = reg::read(crate::RCC_APB2ENR);
                PIN_TX.set_alternate(AF_USART1);
                PIN_RX.set_alternate(AF_USART1);
                // 16x oversampling: BRR is the 12.4 fixed-point divider, which is the whole
                // ratio written as one integer
                reg::write(BRR, (apb2_hz + baud / 2) / baud);
                reg::write(CR1, CR1_UE | CR1_TE | CR1_RE);
                Self { _private: () }
        }
}

impl Transport for Usart1 {
        fn write(&mut self, bytes: &[u8]) -> usize {
                //   the transmit register is one byte deep on this generation: one byte per call
                // at most, and the console's queue feeds the rest over the following passes
                match bytes.first() {
                        Some(&b) if reg::read(SR) & SR_TXE != 0 => {
                                reg::write(DR, u32::from(b));
                                1
                        }
                        _ => 0,
                }
        }

        fn read(&mut self) -> Option<u8> {
                let sr = reg::read(SR);
                if sr & SR_RXNE != 0 {
                        return Some((reg::read(DR) & 0xFF) as u8);
                }
                // an overrun is cleared by the SR read above followed by a DR read
                if sr & SR_ORE != 0 {
                        let _ = reg::read(DR);
                }
                None
        }
}
