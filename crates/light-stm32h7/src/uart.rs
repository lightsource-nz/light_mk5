//! USART1 as a console transport: PA9/PA10, the pins every board on this chip brings out for a
//! serial adapter. The H7's USART is the ISR/ICR generation -- separate status, clear, receive
//! and transmit registers -- with its FIFO left off, so the transmit register is one byte deep
//! like the F4's. Non-blocking both ways, so the console loop it serves never waits on a wire
//! nothing is listening to.

use light_shell_cmsis::Transport;

use crate::gpio::Pin;
use crate::reg;

const USART1_BASE: usize = 0x4001_1000;
const CR1: usize = USART1_BASE + 0x00;
const BRR: usize = USART1_BASE + 0x0C;
const ISR: usize = USART1_BASE + 0x1C;
const ICR: usize = USART1_BASE + 0x20;
const RDR: usize = USART1_BASE + 0x24;
const TDR: usize = USART1_BASE + 0x28;

const CR1_UE: u32 = 1 << 0;
const CR1_RE: u32 = 1 << 2;
const CR1_TE: u32 = 1 << 3;
const ISR_FE: u32 = 1 << 1;
const ISR_ORE: u32 = 1 << 3;
const ISR_RXNE: u32 = 1 << 5;
const ISR_TXE: u32 = 1 << 7;
const ICR_FECF: u32 = 1 << 1;
const ICR_ORECF: u32 = 1 << 3;

/// USART1 on APB2; its kernel clock is APB2 by reset default.
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
                // 16x oversampling: BRR is the whole ratio
                reg::write(BRR, (apb2_hz + baud / 2) / baud);
                reg::write(CR1, CR1_UE | CR1_TE | CR1_RE);
                Self { _private: () }
        }
}

impl Transport for Usart1 {
        fn write(&mut self, bytes: &[u8]) -> usize {
                //   one byte per call at most -- the transmit register is one deep with the FIFO
                // off -- and the console's queue feeds the rest over the following passes
                match bytes.first() {
                        Some(&b) if reg::read(ISR) & ISR_TXE != 0 => {
                                reg::write(TDR, u32::from(b));
                                1
                        }
                        _ => 0,
                }
        }

        fn read(&mut self) -> Option<u8> {
                let isr = reg::read(ISR);
                if isr & ISR_RXNE != 0 {
                        return Some((reg::read(RDR) & 0xFF) as u8);
                }
                // an overrun or framing error latches and stops reception until cleared
                if isr & (ISR_ORE | ISR_FE) != 0 {
                        reg::write(ICR, ICR_ORECF | ICR_FECF);
                }
                None
        }
}
