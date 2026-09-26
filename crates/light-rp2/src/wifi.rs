//! The bus a radio part is driven over, and the clock such a part's driver asks for. These are the
//! two things this chip owes a radio, and the whole of what it owes one.
//!
//! THE DRIVER IS NOT HERE, and that is the point of the split: a radio part outlives the chip it is
//! wired beside, so its driver belongs to the part and lives in its own crate above
//! [`light_core::hal::RadioBus`]. Any chip that can turn a data line around can link it. What stays
//! behind is only what is genuinely this chip's.
//!
//! WHY THE BUS IS BUILT OUT OF PIO. The radio speaks a serial protocol that is SPI in shape but
//! HALF DUPLEX ON ONE WIRE: the host clocks a command out on the data line, then turns the line
//! around and clocks the answer back in on the same wire. No SPI peripheral does that, so the bus
//! is eight instructions of PIO -- shift out X+1 bits, reverse the pin, shift in Y+1 bits -- with
//! the two counts loaded per transfer. That program is the vendor's and the embedded Rust
//! ecosystem's alike; what differs here is that it is assembled by hand and the FIFOs are filled
//! by the processor rather than by DMA, because a transfer stalls a state machine harmlessly (the
//! clock simply pauses mid-word, which a synchronous slave cannot tell from a slow master) and one
//! fewer moving part is worth more here than the throughput.
//!
//! WHY THERE IS A CLOCK DRIVER IN A WIRELESS FILE. Radio stacks are written against the ecosystem's
//! timer facility, which is a pair of free functions the application is expected to supply. This
//! chip already counts microseconds for the framework's own log, so supplying them is four lines --
//! and the second, which exists to wake a sleeping executor, does nothing: there is no executor to
//! wake, every future is polled once a pass by the runtime loop, and a timer future re-reads the
//! clock each time it is polled. That is the whole of the integration, and it is registered here
//! rather than in the driver's crate because the tick is the chip's, not the part's.

use core::task::Waker;

use light_core::hal::RadioBus;

use crate::gpio::{self, Output};
use crate::pac;

// --- the clock the radio stack asks for ---------------------------------------------------------

struct Clock;

impl embassy_time_driver::Driver for Clock {
        fn now(&self) -> u64 {
                crate::now_us()
        }

        //   nothing sleeps, so nothing needs waking: the runtime loop polls every future it owns
        // once a pass, and a timer future compares the clock afresh each time it is polled
        fn schedule_wake(&self, _at: u64, _waker: &Waker) {}
}

embassy_time_driver::time_driver_impl!(static CLOCK: Clock = Clock);

// --- the bus ------------------------------------------------------------------------------------

/// Which state machine of the first block carries the radio.
const SM: usize = 0;
/// What the first block's state machines ask for when their queues want feeding and emptying:
/// the four output queues come first, then the four input ones.
const DREQ_TX: u8 = SM as u8;
const DREQ_RX: u8 = 4 + SM as u8;

/// The bus program, hand-assembled. One side-set bit, which is the clock.
///
/// ```text
/// lp:  out pins, 1      side 0   ; a bit out, clock low
///      jmp x-- lp       side 1   ; clock high -- X+1 bits in all
///      set pindirs, 0   side 0   ; turn the wire around
///      nop              side 1
/// lp2: in pins, 1       side 0   ; a bit in
///      jmp y-- lp2      side 1   ; Y+1 bits in all
///      wait 1 pin 0     side 0   ; the radio raises the wire when it has something to say
///      irq 0            side 0
/// ```
///
/// The turnaround in the middle is why this cannot be an SPI peripheral, and the two counts are
/// why they are loaded into X and Y per transfer rather than being part of the program.
/// Above this state-machine rate, the turnaround is written the fast way; at or below it, the
/// slow way. The difference is one clock edge in the middle of a transfer and it is not cosmetic:
/// the wrong one for the rate reads the answer half a bit early, which does not fail cleanly --
/// it corrupts occasional replies and leaves everything else looking like it works.
const FAST_PROGRAM_ABOVE_HZ: u32 = 75_000_000;

const PROGRAM_FAST: [u16; 8] = [
        0x6001, // out pins, 1      side 0
        0x1040, // jmp x-- 0        side 1
        0xe080, // set pindirs, 0   side 0
        0xb042, // nop              side 1
        0x4001, // in pins, 1       side 0
        0x1084, // jmp y-- 4        side 1
        0x20a0, // wait 1 pin 0     side 0
        0xc000, // irq 0            side 0
];

/// The same, for a slower state machine: the turnaround costs a cycle less and the read samples
/// on the other edge.
const PROGRAM_SLOW: [u16; 8] = [
        0x6001, // out pins, 1      side 0
        0x1040, // jmp x-- 0        side 1
        0xe080, // set pindirs, 0   side 0
        0xa042, // nop              side 0
        0x5001, // in pins, 1       side 1
        0x0084, // jmp y-- 4        side 0
        0x20a0, // wait 1 pin 0     side 0
        0xc000, // irq 0            side 0
];

/// `out x, 32` and `out y, 32`, executed to load a transfer's two bit counts from the output FIFO.
const OUT_X: u16 = 0x6020;
const OUT_Y: u16 = 0x6040;
/// `set pindirs, 1` -- the wire back to an output for the next transfer.
const SET_PINDIRS_OUT: u16 = 0xe081;
/// `jmp 0` -- back to the top, which is where a transfer starts.
const JMP_TOP: u16 = 0x0000;

/// How the radio is wired to this chip.
#[derive(Clone, Copy)]
pub struct Pins {
        /// Holds the radio in reset until the host is ready for it.
        pub pwr: usize,
        /// Chip select, driven by the processor rather than the state machine: it frames a whole
        /// transfer, and a transfer is several state-machine runs.
        pub cs: usize,
        /// The one data wire, clocked out and then in.
        pub dio: usize,
        /// The clock, which is the program's side-set pin.
        pub clk: usize,
}

/// The wiring a board of this family uses when the radio is on the board rather than beside it.
pub const ONBOARD: Pins = Pins { pwr: 23, cs: 25, dio: 24, clk: 29 };

/// The fastest the part accepts on this bus, which is what the divider is worked back from.
const MAX_BUS_HZ: u32 = 50_000_000;

//   what the part can complain about in the status word that ends every transfer
//   kept, though nothing reads it: it names the bit the status word uses for "nothing arrived",
// which is what a reader needs when chasing a bus fault by hand -- see where the mask below
// deliberately leaves it out
#[allow(dead_code)]
const STATUS_DATA_NOT_AVAILABLE: u32 = 0x0000_0001;
const STATUS_UNDERFLOW: u32 = 0x0000_0002;
const STATUS_OVERFLOW: u32 = 0x0000_0004;
const STATUS_HOST_CMD_DATA_ERR: u32 = 0x0000_0080;

/// The radio's half-duplex bus.
pub struct PioSpi {
        cs: Output,
        dio: u8,
        clk: u8,
        /// The one channel that carries both directions -- they never overlap.
        dma: usize,
}

impl PioSpi {
        /// Claim the first PIO block's first state machine and the four pins.
        ///
        /// The caller says what the system clock is and the rate is worked out here, because
        /// getting it wrong is not a thing a board should be able to do: the divider must be a
        /// WHOLE number (a fractional one is achieved by alternating long and short cycles, and
        /// the short ones would be over the part's limit), the bus runs at half the state
        /// machine's rate, and the program itself differs above and below a threshold.
        pub fn new(pins: Pins, sys_hz: u32, dma: usize) -> Self {
                //   the smallest whole divider that keeps the bus inside what the part accepts
                let mut divider: u16 = 1;
                while sys_hz / (divider as u32) / 2 > MAX_BUS_HZ {
                        divider += 1;
                }
                let sm_hz = sys_hz / divider as u32;
                let program = if sm_hz >= FAST_PROGRAM_ABOVE_HZ { &PROGRAM_FAST } else { &PROGRAM_SLOW };
                light_core::debug!("radio bus: system {sys_hz} Hz over {divider} is {sm_hz} Hz, {} Hz on the wire", sm_hz / 2);

                let pio = unsafe { &*pac::PIO0::ptr() };

                //   the two pins the state machine drives. Both want the strongest drive and the
                // fastest edges the pad offers -- this is a 37 MHz bus on a board trace -- and the
                // data pin additionally wants its input unsynchronised, because a bit clocked in
                // is read one cycle after it is driven and the synchroniser's two stages do not
                // fit in that
                for pin in [pins.dio, pins.clk] {
                        let pads = unsafe { &*pac::PADS_BANK0::ptr() };
                        pads.gpio(pin).modify(|_, w| unsafe { w.drive().bits(3).slewfast().set_bit().pue().clear_bit().pde().clear_bit() });
                        gpio::set_function(pin, gpio::FUNC_PIO0);
                }
                let pads = unsafe { &*pac::PADS_BANK0::ptr() };
                pads.gpio(pins.dio).modify(|_, w| w.schmitt().set_bit());
                pio.input_sync_bypass().modify(|r, w| unsafe { w.bits(r.bits() | (1 << pins.dio)) });

                for (i, instr) in program.iter().enumerate() {
                        pio.instr_mem(i).write(|w| unsafe { w.bits(u32::from(*instr)) });
                }

                let sm = pio.sm(SM);
                sm.sm_clkdiv().write(|w| unsafe { w.int().bits(divider).frac().bits(0) });
                //   the program wraps from its last instruction back to its first, and the whole
                // of it is loaded at zero
                sm.sm_execctrl().write(|w| unsafe { w.wrap_bottom().bits(0).wrap_top().bits(program.len() as u8 - 1) });
                //   both directions shift left -- the bus is most-significant-bit first -- and
                // both refill and drain themselves at the word, so the program never pulls or
                // pushes explicitly
                sm.sm_shiftctrl().write(|w| unsafe {
                        w.out_shiftdir().clear_bit().in_shiftdir().clear_bit().autopull().set_bit().autopush().set_bit().pull_thresh().bits(0).push_thresh().bits(0)
                });
                sm.sm_pinctrl().write(|w| unsafe {
                        w.out_base()
                                .bits(pins.dio as u8)
                                .out_count()
                                .bits(1)
                                .in_base()
                                .bits(pins.dio as u8)
                                .set_base()
                                .bits(pins.dio as u8)
                                .set_count()
                                .bits(1)
                                .sideset_base()
                                .bits(pins.clk as u8)
                                .sideset_count()
                                .bits(1)
                });

                //   both pins out and low before anything is enabled, so the radio never sees a
                // floating clock
                sm.sm_instr().write(|w| unsafe { w.bits(0xe081) }); // set pindirs, 1 (data)
                sm.sm_pinctrl().modify(|_, w| unsafe { w.set_base().bits(pins.clk as u8) });
                sm.sm_instr().write(|w| unsafe { w.bits(0xe081) }); // set pindirs, 1 (clock)
                sm.sm_pinctrl().modify(|_, w| unsafe { w.set_base().bits(pins.dio as u8) });

                Self { cs: Output::new(pins.cs, true), dio: pins.dio as u8, clk: pins.clk as u8, dma }
        }

        fn pio() -> &'static pac::pio0::RegisterBlock {
                unsafe { &*pac::PIO0::ptr() }
        }

        fn enable(on: bool) {
                let pio = Self::pio();
                pio.ctrl().modify(|r, w| unsafe {
                        let bits = r.sm_enable().bits();
                        w.sm_enable().bits(if on { bits | (1 << SM) } else { bits & !(1 << SM) })
                });
        }

        /// Set up a run: how many bits go out, how many come back, and start at the top.
        ///
        /// EVERY TRANSFER STARTS FROM NOTHING, deliberately. What the previous one left behind is
        /// invisible and cumulative: a word still in the input queue becomes the next transfer's
        /// first word and shifts every word after it by one, and bits left in the output shift
        /// register are taken in place of the bit count loaded below -- after which the run is the
        /// wrong length and the answer is the wrong data, with the bus itself reporting no fault
        /// at all. Emptying the queue and restarting the machine's shift state costs a handful of
        /// cycles per transfer and makes the whole class of fault impossible.
        fn arm(&mut self, write_bits: u32, read_bits: u32) {
                let pio = Self::pio();
                let sm = pio.sm(SM);
                Self::enable(false);
                while pio.fstat().read().rxempty().bits() & (1 << SM) == 0 {
                        let _ = pio.rxf(SM).read().bits();
                }
                pio.ctrl().modify(|_, w| unsafe { w.sm_restart().bits(1 << SM) });
                //   the counts are too large for an immediate, so they arrive the way everything
                // else does -- through the output FIFO, pulled into X and Y by an instruction
                // executed while the machine is stopped
                pio.txf(SM).write(|w| unsafe { w.bits(write_bits) });
                sm.sm_instr().write(|w| unsafe { w.bits(u32::from(OUT_X)) });
                pio.txf(SM).write(|w| unsafe { w.bits(read_bits) });
                sm.sm_instr().write(|w| unsafe { w.bits(u32::from(OUT_Y)) });
                //   the previous run left the wire an input
                sm.sm_instr().write(|w| unsafe { w.bits(u32::from(SET_PINDIRS_OUT)) });
                sm.sm_instr().write(|w| unsafe { w.bits(u32::from(JMP_TOP)) });
                Self::enable(true);
                let _ = (self.dio, self.clk);
        }

        //   ONE DMA TRANSFER, waited out. The processor does not touch the queues at all: it
        // hands the channel a source, a destination and a count, and the channel moves a word
        // only when the state machine asks for one.
        //
        //   WHY NOT FILL THE QUEUES FROM THE PROCESSOR, which is simpler and was how this was
        // first written: because it corrupts the data, and does so quietly. Measured on this
        // part, a processor-fed round trip came back with the LAST BIT OF EVERY WORD AFTER THE
        // EIGHTH cleared -- reproducibly, at the same offsets, at two different addresses, with
        // the bus reporting no error and short transfers unaffected. Nothing above the bus can
        // see that for what it is: what it looks like three layers up is a radio that refuses
        // one command out of hundreds for no reason.
        fn dma(&mut self, from: u32, to: u32, words: u32, treq: u8, incr_read: bool, incr_write: bool) {
                let dma = unsafe { &*pac::DMA::ptr() };
                let ch = dma.ch(self.dma);
                ch.ch_read_addr().write(|w| unsafe { w.bits(from) });
                ch.ch_write_addr().write(|w| unsafe { w.bits(to) });
                ch.ch_trans_count().write(|w| unsafe { w.bits(words) });
                //   chained to itself, which is how a channel says it chains to nothing
                ch.ch_ctrl_trig().write(|w| unsafe {
                        w.data_size()
                                .size_word()
                                .incr_read()
                                .bit(incr_read)
                                .incr_write()
                                .bit(incr_write)
                                .treq_sel()
                                .bits(treq)
                                .chain_to()
                                .bits(self.dma as u8)
                                .en()
                                .set_bit()
                });
                while ch.ch_ctrl_trig().read().busy().bit_is_set() {}
        }

        fn transfer(&mut self, write: &[u32], read: &mut [u32]) -> u32 {
                //   the run is "this many bits out, then this many in", and the machine counts
                // from the value loaded down through zero -- so each count is one less than the
                // number of bits. The answer always carries a trailing status word, which is the
                // extra word in the read count
                let write_bits = write.len() as u32 * 32 - 1;
                let read_bits = (read.len() as u32 + 1) * 32 - 1;
                self.arm(write_bits, read_bits);

                let pio = Self::pio();
                let txf = pio.txf(SM).as_ptr() as u32;
                let rxf = pio.rxf(SM).as_ptr() as u32;

                //   out first and in second, and never both at once: the program writes every
                // bit it was given before it turns the wire around, so nothing arrives until
                // everything has left
                self.dma(write.as_ptr() as u32, txf, write.len() as u32, DREQ_TX, true, false);
                if !read.is_empty() {
                        self.dma(rxf, read.as_mut_ptr() as u32, read.len() as u32, DREQ_RX, false, true);
                }
                //   the answer always ends with a status word of the part's own
                let mut status = 0u32;
                self.dma(rxf, &mut status as *mut u32 as u32, 1, DREQ_RX, false, true);
                //   THE PART REPORTS ON ITS OWN BUS, in the word that ends every transfer, and
                // the complaints it can make are exactly the ones a hand-built bus provokes: a
                // command it could not parse, a transfer that outran a FIFO in either direction.
                // Reading the word and saying nothing would leave those failures to surface much
                // later as an unexplained refusal, which is precisely what they did
                //
                //   The one thing it says that is NOT a complaint is the echo it gives before
                // the bus has been configured at all: the part answers the opening exchanges
                // with a repeated test byte, and two of those bits land inside this mask. A
                // status whose four bytes are identical is that echo, not a fault, and warning
                // about it on every successful start-up would teach a reader to ignore the
                // warning that matters.
                //   "NO DATA" IS AN ANSWER, NOT A COMPLAINT, which is why it is not in this mask
                // even though the part reports it in the same word. A driver that asks whether
                // anything has arrived gets told when nothing has, and the version of it this port
                // now carries asks constantly -- so including that bit meant hundreds of warnings
                // across a bring-up that went on to succeed completely. By the same argument as the
                // echo below: a warning that fires on every successful start-up teaches a reader to
                // ignore the one that matters. The bit is still readable here if a genuine bus
                // fault is ever being chased.
                const TROUBLE: u32 = STATUS_UNDERFLOW | STATUS_OVERFLOW | STATUS_HOST_CMD_DATA_ERR;
                let echo = status.to_le_bytes().windows(2).all(|p| p[0] == p[1]);
                if status & TROUBLE != 0 && !echo {
                        light_core::warn!("radio bus: the part answered {status:#010x} to a transfer of {} out, {} in", write.len(), read.len());
                }
                status
        }
}

//   CHIP SELECT IS FRAMED HERE, not by the state machine: it brackets a whole transfer and a
// transfer is several state-machine runs, so the processor owns it. That is also why this is the
// seam -- the framing is part of what "one transfer on this bus" means, and a driver above should
// not have to know it.
impl RadioBus for PioSpi {
        fn write(&mut self, words: &[u32]) -> u32 {
                self.cs.set(false);
                let status = self.transfer(words, &mut []);
                self.cs.set(true);
                status
        }

        fn read(&mut self, command: u32, read: &mut [u32]) -> u32 {
                self.cs.set(false);
                let status = self.transfer(&[command], read);
                self.cs.set(true);
                status
        }
}

/// The two pieces a radio driver wants from this chip: the bus, and the line that holds the part in
/// reset until the host is ready for it.
///
/// Offered together because they come from one set of pins and getting them separately is an
/// opportunity to pair the wrong ones. The driver that receives them belongs to the part, not to
/// this chip -- see the module note.
pub fn radio_bus(pins: Pins, sys_hz: u32, dma: usize) -> (PioSpi, Output) {
        //   the reset line is taken LOW FIRST, before the bus under it is configured: the part
        // should not see a clock until whoever owns it is ready to talk to it
        let pwr = Output::new(pins.pwr, false);
        (PioSpi::new(pins, sys_hz, dma), pwr)
}

