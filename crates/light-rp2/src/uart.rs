//! A UART as a console transport: UART0, polled, never blocking. On a device-role board it is the
//! console's second path -- the one a debug probe's serial adapter carries when the board's own
//! USB port is not cabled -- and on a USB-host board (whose port hosts instruments) it is the only
//! console. The core-1 loop in `shell` drives it beside the USB CDC class: a log line goes to both,
//! input is taken from either.
//!
//! Same silicon block on both chips (the PL011), the same registers, the same GPIO function.

use crate::gpio;
use crate::pac;
use light_core::ConstStaticCell;

/// What a write goes into and the transmitter comes out of.
///
///   A KILOBYTE, because the point of it is to absorb a burst. The log queue holds a few dozen
/// records and a record is a line; at the console's rate a line is milliseconds of wire time, so
/// without somewhere to put them the queue is the only elastic there is and a burst of them is
/// lost at the producer. With this, a burst is lost only if it outruns the wire by a kilobyte.
const TX_RING: usize = 1024;

static TX: ConstStaticCell<[u8; TX_RING]> = ConstStaticCell::new([0; TX_RING]);

/// The DMA channel the console's transmitter owns, on every board and on both chips.
///
///   FIXED RATHER THAN PASSED IN, which is the opposite of how every other channel here is
/// chosen. A display's or an audio path's channel is a board's business because the board decides
/// whether it has one at all; the console is on every board there is, so binding it once is the
/// arrangement that cannot be got wrong. Nothing has to choose it, nothing has to thread it
/// through a constructor, and a board author reads one constant to know what is spoken for.
///
///   TEN, because it has to exist on both parts and the smaller one has twelve channels, and
/// because it was the highest that no board had already taken -- the alternative was renumbering
/// three of them, including an audio capture path proven on hardware, for nothing but tidier
/// ordering. Boards take theirs from the top downward, away from the platform allocator that
/// counts up from zero; this sits just under them.
pub const CONSOLE_DMA_CH: usize = 10;

/// UART0, 8N1, FIFOs on.
pub struct Uart {
        /// The bytes waiting to go out, oldest at `tail`. In `.bss`: a kilobyte is more than this
        /// core's whole stack can spare, and the value would be copied on every move besides.
        ring: &'static mut [u8; TX_RING],
        tail: usize,
        len: usize,
        /// Bytes of `len` the transmitter is currently carrying, counted from `tail`; zero when
        /// the channel is idle. They stay counted in `len` so a write can never be handed space
        /// that is being read out from under it.
        inflight: usize,
        /// Bytes a full ring had to refuse, since boot. The console's policy is that a line which
        /// does not fit is dropped and counted, never waited for.
        dropped: u32,
}

impl Uart {
        /// Take UART0 on `tx`/`rx` at `baud`, clocked from `peri_hz` (the peripheral clock the
        /// shell reports in `ShellInfo`).
        ///
        /// # Safety
        /// Constructs the one owner of UART0; call it once.
        pub unsafe fn new(tx: usize, rx: usize, baud: u32, peri_hz: u32) -> Self {
                crate::reset_cycle(true, |w| w.uart0().set_bit(), |w| w.uart0().clear_bit(), |r| r.uart0().bit_is_set());

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
                //   AND THE TRANSMITTER MUST BE TOLD TO ASK. A channel paced by a peripheral's
                // request line moves nothing until that peripheral is enabled to raise it: without
                // this the first transfer is armed, never advances, and every later call finds the
                // channel still busy -- so the ring fills and the console goes silent with no
                // clue as to why, which is the worst way for the one thing that reports faults to
                // fail. Receive is left alone; input is read straight from the FIFO.
                uart.uartdmacr().write(|w| w.txdmae().set_bit());

                gpio::set_function(tx, gpio::FUNC_UART);
                gpio::set_function(rx, gpio::FUNC_UART);
                Self { ring: TX.take(), tail: 0, len: 0, inflight: 0, dropped: 0 }
        }

        /// Queue `bytes` and return how many were taken. Never waits.
        ///
        ///   THE CONSOLE CORE HAS SOMETHING MORE URGENT THAN THE WIRE, which is what this used to
        /// get wrong. It waited for room in the transmit FIFO byte by byte, so a line cost that
        /// core its whole wire time -- about fourteen milliseconds at the console's rate, since
        /// the FIFO is thirty-two bytes and a line is longer. That looked free, because that core
        /// exists to run the console. It was not: the same loop is what takes INPUT, and the
        /// receive FIFO is also thirty-two bytes, which at the same rate is under three
        /// milliseconds of typing. Anything arriving while a line went out was lost past that --
        /// so a command pasted into a board that was busy logging came out with holes in it, and
        /// the heartbeat that says the core is alive stopped for the duration too.
        ///
        ///   So the wire is fed from a ring instead, a few bytes per pass of a loop that now never
        /// stops. Throughput is unchanged -- it was always the wire -- but nothing is blocked
        /// behind it. What does not fit is dropped and counted, which is the same policy the rest
        /// of the console already has for a transport that cannot keep up.
        pub fn write(&mut self, bytes: &[u8]) -> usize {
                let take = (TX_RING - self.len).min(bytes.len());
                let head = (self.tail + self.len) % TX_RING;
                // at most two runs: up to the end of the ring, then from its start
                let first = take.min(TX_RING - head);
                self.ring[head..head + first].copy_from_slice(&bytes[..first]);
                self.ring[..take - first].copy_from_slice(&bytes[first..take]);
                self.len += take;
                self.dropped = self.dropped.saturating_add((bytes.len() - take) as u32);
                self.service();
                take
        }

        /// Retire a finished transfer and start the next. Called by every pass of the console
        /// loop, and by `write` so a line starts on its way without waiting for one.
        ///
        ///   THE TRANSFER IS HANDED OVER, NOT FED. The channel is paced by the transmitter's own
        /// request line, so it delivers a whole run at the wire's rate with nothing watching it:
        /// this core is free between the call that starts one and the call that finds it done, and
        /// output keeps flowing even across a pass that goes long somewhere else -- a device-role
        /// board's console shares this loop with a USB stack poll.
        ///
        ///   One run at a time, and never past the end of the ring: what wraps is simply the next
        /// run. Byte-wide writes into a peripheral register are the thing to be careful of on this
        /// bus -- a narrow write is replicated across the word, which has silently ruined a stream
        /// into a register whose upper bits meant something -- but the transmitter's data register
        /// is data in its low byte and reserved above, so there is nothing there to ruin.
        pub fn service(&mut self) {
                let dma = unsafe { &*pac::DMA::ptr() };
                let ch = dma.ch(CONSOLE_DMA_CH);
                if self.inflight != 0 {
                        if ch.ch_ctrl_trig().read().busy().bit_is_set() {
                                return;
                        }
                        self.tail = (self.tail + self.inflight) % TX_RING;
                        self.len -= self.inflight;
                        self.inflight = 0;
                }
                if self.len == 0 {
                        return;
                }
                let run = self.len.min(TX_RING - self.tail);
                let uart = unsafe { &*pac::UART0::ptr() };
                ch.ch_read_addr().write(|w| unsafe { w.bits(self.ring[self.tail..].as_ptr() as u32) });
                ch.ch_write_addr().write(|w| unsafe { w.bits(uart.uartdr().as_ptr() as u32) });
                ch.ch_trans_count().write(|w| unsafe { w.bits(run as u32) });
                // chained to itself, which means no chaining; writing this register is the start
                ch.ch_ctrl_trig().write(|w| unsafe {
                        w.data_size()
                                .size_byte()
                                .incr_read()
                                .set_bit()
                                .incr_write()
                                .clear_bit()
                                .treq_sel()
                                .uart0_tx()
                                .chain_to()
                                .bits(CONSOLE_DMA_CH as u8)
                                .en()
                                .set_bit()
                });
                self.inflight = run;
        }

        /// Bytes refused because the ring was full, since boot.
        pub fn dropped(&self) -> u32 {
                self.dropped
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
