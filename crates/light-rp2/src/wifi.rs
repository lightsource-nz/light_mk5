//! The wireless part some boards of this family carry, on the bus this chip has to build for it.
//!
//! The radio is a separate chip with no firmware of its own: it is powered, handed a quarter of a
//! megabyte of image over a private serial bus, and only then is there a radio at all. The image
//! is delivered as an ASSET rather than linked into the firmware (see `light-assets`), so what is
//! here is the two things a port must supply -- the bus, and the driving of a stack written for an
//! executor this framework does not have.
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
//! WHY THERE IS A CLOCK DRIVER IN A WIRELESS FILE. The radio stack is written against the
//! ecosystem's timer facility, which is a pair of free functions the application is expected to
//! supply. The chip already counts microseconds for the framework's own log, so supplying them is
//! four lines -- and the second, which exists to wake a sleeping executor, does nothing: there is
//! no executor to wake, every future here is polled once a pass by the runtime loop, and a timer
//! future re-reads the clock each time it is polled. That is the whole of the integration.

use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use light_core::atomic::{AtomicU32, Ordering};
use light_core::{ConstStaticCell, StaticCell};

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

// --- the radio driver's own account of itself ---------------------------------------------------

//   The driver reports through the ecosystem's logging facility, which is a global sink the
// application installs. Forwarding it into the framework's log rather than discarding it is worth
// the twenty lines: a radio refuses ONE command out of a conversation of hundreds, names the
// reason in a log line, and reports nothing whatsoever through its return values -- so without
// this a bring-up is guesswork, and with it the answer is on the console with everything else.
/// The driver's own words for a packet it had nowhere to put. Matched exactly; see the note in
/// `Relay::log` for why this is recognised by its text and what happens if it ever changes.
const NO_ROOM_FOR_PACKET: &str = "failed to push rxd packet to the channel.";

/// How many packets the radio had nowhere to put. Not an error count -- the ones that matter are
/// sent again -- but the measure of how hard the path between the radio and the stack is being
/// pushed, which is exactly what a transfer wants to report.
static DROPPED: AtomicU32 = AtomicU32::new(0);

/// A hash of a line, taken as it is formatted so that the line itself never has to be kept.
fn digest(args: core::fmt::Arguments) -> u32 {
        struct Hash(u32);
        impl core::fmt::Write for Hash {
                fn write_str(&mut self, s: &str) -> core::fmt::Result {
                        for b in s.as_bytes() {
                                self.0 = (self.0 ^ u32::from(*b)).wrapping_mul(0x0100_0193);
                        }
                        Ok(())
                }
        }
        let mut h = Hash(0x811c_9dc5);
        let _ = core::fmt::write(&mut h, args);
        h.0
}


struct Relay;

impl log::Log for Relay {
        fn enabled(&self, m: &log::Metadata) -> bool {
                light_core::log::enabled(level_of(m.level()))
        }

        fn log(&self, record: &log::Record) {
                //   THROWN AWAY BEFORE IT COSTS A PLACE IN THE RATE BELOW. The driver is asked
                // for everything down to its per-command chatter, because the level that decides
                // what is worth seeing is the framework's and not this crate's -- so most of what
                // arrives here is discarded a moment later by that level. Rationing those too
                // spends the ration on lines nobody can see, and what reaches the console is a
                // run of summaries with nothing left to summarise.
                //   NOT EVERY WARNING FROM A DRIVER IS ONE. This part reports a packet it had
                // nowhere to put as a warning, and during a transfer it says so dozens of times
                // -- while the transfer succeeds. A line that appears that often in the normal
                // course of a thing working is not a warning, whatever it was labelled; it is
                // detail, and it belongs at the level where detail lives. Left as it was, it
                // buried the result the transfer was run to produce.
                //
                //   Saying so HERE rather than in the driver, because the level is the driver's
                // and changing it there would mean carrying a copy of the driver for one word.
                // The line is recognised by its text, so if a later version words it differently
                // it simply goes back to being a warning: wrong, but visible, which is the right
                // way round for a guess like this to fail.
                //
                //   It is still counted. How often it happens is the measure of how hard the
                // path is being pushed, and that belongs with the transfer's own report rather
                // than scattered through the log.
                let mut level = level_of(record.level());
                if record.level() == log::Level::Warn && digest(*record.args()) == digest(format_args!("{NO_ROOM_FOR_PACKET}")) {
                        DROPPED.fetch_add(1, Ordering::Relaxed);
                        level = light_core::log::Level::Debug;
                }
                if !light_core::log::enabled(level) {
                        return;
                }

                //   the target is the driver's module path, which is of no use to anyone reading a
                // console -- what matters is that this came from the radio
                light_core::log::push(level, "radio", *record.args());
        }

        fn flush(&self) {}
}

fn level_of(level: log::Level) -> light_core::log::Level {
        match level {
                log::Level::Error => light_core::log::Level::Error,
                log::Level::Warn => light_core::log::Level::Warn,
                log::Level::Info => light_core::log::Level::Info,
                log::Level::Debug => light_core::log::Level::Debug,
                log::Level::Trace => light_core::log::Level::Trace,
        }
}

/// Send the radio driver's logging to the framework's log.
///
/// Installed once, by the first radio brought up. The framework's own level still decides what
/// survives, so this costs nothing until someone turns the level up to look.
fn relay_log() {
        static RELAY: Relay = Relay;
        //   an error here means something else already claimed the sink, which is not this
        // crate's business to argue with
        let _ = log::set_logger(&RELAY);
        //   DEBUG and no further. The driver's trace level is a line per bus transfer, and the
        // start-up alone is tens of thousands of them -- turning it on does not produce a longer
        // account of the problem, it produces a console that never catches up and a ring buffer
        // that drops the one line that mattered. Its debug level is a few dozen lines and says
        // what the radio was asked and what it answered
        log::set_max_level(log::LevelFilter::Debug);
}

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
                const TROUBLE: u32 = STATUS_DATA_NOT_AVAILABLE | STATUS_UNDERFLOW | STATUS_OVERFLOW | STATUS_HOST_CMD_DATA_ERR;
                let echo = status.to_le_bytes().windows(2).all(|p| p[0] == p[1]);
                if status & TROUBLE != 0 && !echo {
                        light_core::warn!("radio bus: the part answered {status:#010x} to a transfer of {} out, {} in", write.len(), read.len());
                }
                status
        }
}

impl cyw43::SpiBusCyw43 for PioSpi {
        async fn cmd_write(&mut self, write: &[u32]) -> u32 {
                self.cs.set(false);
                let status = self.transfer(write, &mut []);
                self.cs.set(true);
                status
        }

        async fn cmd_read(&mut self, write: u32, read: &mut [u32]) -> u32 {
                self.cs.set(false);
                let status = self.transfer(&[write], read);
                self.cs.set(true);
                status
        }
}

// --- driving a stack written for an executor ------------------------------------------------------

/// A waker that does nothing, because nothing sleeps: see the note at the top of the file.
fn noop_waker() -> Waker {
        const VTABLE: RawWakerVTable = RawWakerVTable::new(|_| RawWaker::new(core::ptr::null(), &VTABLE), |_| {}, |_| {}, |_| {});
        // SAFETY: the vtable's functions do nothing with the data pointer
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

/// Where a task's future lives: an opaque type, so it cannot be named for a static, but it can be
/// written into aligned storage once and driven through `dyn Future` from then on.
const TASK_BYTES: usize = 8 * 1024;
#[repr(C, align(16))]
struct TaskStorage([MaybeUninit<u8>; TASK_BYTES]);

/// The radio's own task, and the network's. Two, because they are started at different moments:
/// the radio comes up on its own, and a network exists only once a network has been joined.
static RADIO_TASK: StaticCell<TaskStorage> = StaticCell::new();
static NET_TASK: StaticCell<TaskStorage> = StaticCell::new();

fn place_task<F: Future<Output = ()> + 'static>(cell: &'static StaticCell<TaskStorage>, f: F) -> Pin<&'static mut dyn Future<Output = ()>> {
        assert!(core::mem::size_of::<F>() <= TASK_BYTES && core::mem::align_of::<F>() <= 16, "the task does not fit its storage");
        let storage = cell.init(TaskStorage([MaybeUninit::uninit(); TASK_BYTES]));
        let p = storage.0.as_mut_ptr() as *mut F;
        // SAFETY: sized and aligned for F, written once, and never moved again -- the storage is
        // static and the only reference to it is the pinned one returned
        unsafe {
                p.write(f);
                Pin::new_unchecked(&mut *p)
        }
}

/// Drive one future to its answer, with nothing else running.
///
/// Only safe before the radio's own task exists -- which is exactly the one call that needs it,
/// the power-up that uploads the image.
fn block_on<F: Future>(fut: F) -> F::Output {
        let mut fut = core::pin::pin!(fut);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);
        loop {
                if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                        return v;
                }
        }
}

//   This holds the driver's packet pool -- eight buffers of a network frame each -- so it is far
// larger than the application core's whole stack. See where it is taken for why that matters.
//
//   A const-initialised cell would be the tidiest home for it, but this type is not `Send`: it
// keeps a raw pointer to the buffer an ioctl is using, which is sound here (one radio, one core)
// and not something to assert on the type's behalf from outside the driver.
static STATE: StaticCell<cyw43::State> = StaticCell::new();

/// The radio, once it has firmware in it.
/// Everything a fetch reads and writes into.
///
///   NOT ON THE STACK, AND NOT OPTIONALLY SO. Together these are over nineteen kilobytes, and
/// the application core's stack is a few -- so a frame carrying them does not overrun it by a
/// margin, it overruns it several times over. A stack that has run past its end then takes a
/// fault it cannot stack an exception frame for, which is a locked-up core: no console, no
/// panic message, nothing but a part-drawn display. Whether that happens at all comes down to
/// what memory lies below the stack and whether anything else is using it, which is luck, and
/// it changes with the optimisation level -- the same code was survivable in one build and
/// fatal in the next, because the compiler had inlined two of these frames into one.
///
///   Taken once rather than per fetch, because a cell that hands out its contents twice panics,
/// and a second fetch in one session is an ordinary thing to want.
///
///   THE RECEIVE SIDE IS LARGE ON PURPOSE. It is what decides how much the other end may have
/// in flight before it has to stop and wait, so it sets the ceiling on how fast this can go. It
/// is also what has to hold the arriving image while this side is busy writing the last piece
/// to storage -- seconds of a fetch are spent doing that, and nothing is being read from the
/// network meanwhile. Four kilobytes was measured at 90 KiB/s of network time with the sender
/// stalling; this is four times the room to stall into.
struct Fetch {
        rx: [u8; 16384],
        tx: [u8; 1024],
        /// The request line, built in place.
        request: [u8; HEAD_MAX],
        /// The response head, and whatever body arrived with it.
        response: [u8; HEAD_MAX],
        /// One read's worth of body, on its way to the sink.
        chunk: [u8; 1460],
}

/// Const-initialised, so it is a region of `.bss` that `take` hands a reference to -- never a
/// value built somewhere else and moved in, which for twenty kilobytes would be the very stack
/// copy this exists to avoid.
static FETCH: ConstStaticCell<Fetch> = ConstStaticCell::new(Fetch { rx: [0; 16384], tx: [0; 1024], request: [0; HEAD_MAX], response: [0; HEAD_MAX], chunk: [0; 1460] });

pub struct Radio {
        task: Pin<&'static mut dyn Future<Output = ()>>,
        control: cyw43::Control<'static>,
        /// Taken when the network is built on it, because the stack owns it from then on.
        device: Option<cyw43::NetDriver<'static>>,
        net: Option<Net>,
        /// Whether a network was actually joined, which decides whether there is one to let go
        /// of before joining another.
        joined: bool,
        /// When the idle upkeep in [`Radio::poll`] last actually looked at the radio.
        polled_us: u64,
        /// The fetch buffers, held here so they are taken once and reused -- see [`Fetch`].
        buffers: &'static mut Fetch,
}

/// The network running over the radio, once there is one.
struct Net {
        stack: embassy_net::Stack<'static>,
        task: Pin<&'static mut dyn Future<Output = ()>>,
}

impl Radio {
        /// Power the radio up and upload its image.
        ///
        /// `firmware` is the image, `clm` the regulatory limits that follow it; both come from the
        /// product's asset pack. This does not return until the radio is answering, which on this
        /// part is a fifth of a second -- it is a start-up cost, paid once, and the alternative is
        /// a radio that is half-awake while the rest of the firmware starts around it.
        pub fn new(pins: Pins, sys_hz: u32, dma: usize, firmware: &[u8], clm: &[u8]) -> Self {
                relay_log();
                let pwr = Output::new(pins.pwr, false);
                let spi = PioSpi::new(pins, sys_hz, dma);
                //   BUILT WHERE IT LIVES, not built here and copied there. Handing a cell a value
                // means constructing that value where the caller stands: twelve and a half
                // kilobytes of packet pool on a stack of eight, memcpy'd into the static that was
                // always its home. Given the constructor instead, the cell has somewhere to put
                // the result before it exists, and nothing is ever on the stack.
                //   The give-away for this in a disassembly is a frame far larger than a
                // function's own locals with a memcpy of nearly the same size beside it.
                let state = STATE.init_with(cyw43::State::new);

                let (device, control, runner) = block_on(cyw43::new(state, PwrPin(pwr), spi, firmware));
                //   the task never returns, and a future whose answer is "never" cannot be named
                // for a static on this compiler -- so it is wrapped in one whose answer is nothing
                let mut radio = Self { task: place_task(&RADIO_TASK, async move { runner.run().await }), control, device: Some(device), net: None, joined: false, polled_us: 0, buffers: FETCH.take() };

                //   the regulatory blob is loaded by the control side TALKING TO the task side, so
                // the two have to run together: this is the join the framework does by hand, and
                // the reason the task is placed before the radio is finished being built
                radio.run_until(|c| c.init(clm));
                radio
        }

        /// Run the radio's own task until a piece of control work finishes.
        ///
        /// Control work is a conversation -- a request handed to the task, an answer handed back --
        /// so the task must be polled while it is waiting, or it waits forever. That is what an
        /// executor would do, and this is what it costs without one.
        pub fn run_until<'a, F, Fut>(&'a mut self, f: F) -> Fut::Output
        where
                F: FnOnce(&'a mut cyw43::Control<'static>) -> Fut,
                Fut: Future + 'a,
        {
                self.run_until_or(u64::MAX, f).expect("a wait with no deadline cannot expire")
        }

        /// The same, given up on after `timeout_us`.
        ///
        /// FOR WORK THAT WAITS ON THE AIR rather than on the part. Some of what the radio is
        /// asked to do finishes only when a message arrives -- joining waits for the radio to
        /// report the outcome -- and nothing in that contract promises one ever will. Waiting
        /// for it without a deadline puts the whole application behind a network that may simply
        /// not be there, which is not a failure anyone can diagnose from the outside.
        ///
        /// Giving up abandons the request rather than cancelling it: the radio is left as it was,
        /// and the next thing asked of it works. `None` is "no answer in time", which is a
        /// different thing from an answer of failure and is reported as such.
        pub fn run_until_or<'a, F, Fut>(&'a mut self, timeout_us: u64, f: F) -> Option<Fut::Output>
        where
                F: FnOnce(&'a mut cyw43::Control<'static>) -> Fut,
                Fut: Future + 'a,
        {
                let deadline = crate::now_us().saturating_add(timeout_us);
                let task = &mut self.task;
                let mut fut = core::pin::pin!(f(&mut self.control));
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                loop {
                        let _ = task.as_mut().poll(&mut cx);
                        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
                                return Some(v);
                        }
                        if crate::now_us() >= deadline {
                                return None;
                        }
                }
        }

        /// One pass of the radio's own work, and the network's if there is one: called from the
        /// runtime loop, like every other module.
        ///
        /// The radio first, then the network it feeds, once each.
        ///
        ///   ALTERNATING THE TWO SEVERAL TIMES A PASS WAS TRIED AND DOES NOT HELP, which is worth
        /// recording so that it is not tried again. The buffers between them are four, and when
        /// they run out the radio's side warns and DISCARDS -- but it does not stop there: one
        /// turn of it empties the part completely, discarding the whole remainder of the backlog
        /// before it returns. So the second and later rounds find nothing left to rescue, and all
        /// the extra turns achieve is to ask the radio the same question three more times and
        /// report the same losses more often.
        ///
        ///   What would actually help is more buffers, and they belong to the driver. Until a
        /// transfer exists whose throughput can be measured, there is nothing to weigh that
        /// against, so the simple thing stays.
        /// Keep the radio and its network ticking over between the things asked of it. Call every
        /// pass; what it actually does is paced.
        ///
        ///   LOOKING AT THE RADIO COSTS A BUS TRANSACTION, and asking an idle one whether anything
        /// has happened is most of what this used to do. Measured on a board that is also
        /// forwarding instrument traffic: bringing the radio up -- not joining a network, merely
        /// powering it -- HALVED the application's loop rate, from sixteen thousand passes a
        /// second to eight, with the radio's poll taking a third of every pass. That is a standing
        /// tax on everything else the board does, paid for a part that is usually idle.
        ///
        ///   Nothing needs it that often. Everything that waits on the radio -- joining,
        /// configuring, fetching -- drives its own loop and polls as fast as it can for as long as
        /// it takes. What is left for here is upkeep: lease renewals, address resolution, the
        /// transport's timers. Those are millisecond work, so this looks once a millisecond and
        /// costs a twentieth of what it did.
        pub fn poll(&mut self) {
                let now = crate::now_us();
                if now.wrapping_sub(self.polled_us) < IDLE_POLL_US {
                        return;
                }
                self.polled_us = now;
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                let _ = self.task.as_mut().poll(&mut cx);
                if let Some(net) = self.net.as_mut() {
                        let _ = net.task.as_mut().poll(&mut cx);
                }
        }

        /// The radio's address, which is also the first proof that the image took: it is read out
        /// of the running radio rather than out of the image.
        pub fn address(&mut self) -> [u8; 6] {
                use embassy_net_driver::Driver as _;
                match self.device.as_mut().map(|d| d.hardware_address()) {
                        Some(embassy_net_driver::HardwareAddress::Ethernet(a)) => a,
                        _ => [0; 6],
                }
        }

        /// Ask the network this radio has joined for an address, and wait for one.
        ///
        /// A JOINED RADIO IS NOT YET A NETWORK. Joining is association: the two ends agree that
        /// they may talk. Everything above that -- an address of one's own, somewhere to send
        /// what is not local, a name to resolve by -- is handed out by the network afterwards and
        /// has to be asked for. This does the asking and waits for the answer.
        ///
        /// The stack is built here rather than at start-up because it exists only once there is
        /// something for it to run over, and it takes its memory when it does.
        pub fn configure(&mut self) -> Result<embassy_net::StaticConfigV4, NetError> {
                if self.net.is_none() {
                        let device = self.device.take().ok_or(NetError::AlreadyBuilt)?;
                        static RESOURCES: StaticCell<embassy_net::StackResources<SOCKETS>> = StaticCell::new();
                        let resources = RESOURCES.init(embassy_net::StackResources::new());
                        //   THE SEED IS WEAK, and deliberately so for now: it is what the
                        // connection's opening numbers are derived from, and the chip's own
                        // source of randomness is not wired up here yet. It costs nothing that
                        // matters while the thing being carried is an image the hardware verifies
                        // for itself -- but it is not what a transport carrying trust would use.
                        let seed = crate::now_us().wrapping_mul(0x9E37_79B9_7F4A_7C15);
                        let (stack, mut runner) = embassy_net::new(device, embassy_net::Config::dhcpv4(Default::default()), resources, seed);
                        self.net = Some(Net { stack, task: place_task(&NET_TASK, async move { runner.run().await }) });
                }

                let deadline = crate::now_us() + CONFIGURE_TIMEOUT_US;
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                loop {
                        let _ = self.task.as_mut().poll(&mut cx);
                        let net = self.net.as_mut().expect("just built");
                        let _ = net.task.as_mut().poll(&mut cx);
                        if let Some(config) = net.stack.config_v4() {
                                return Ok(config);
                        }
                        if crate::now_us() >= deadline {
                                return Err(NetError::NoAddress);
                        }
                }
        }

        /// Fetch something over the network, handing the body out as it arrives.
        ///
        ///   NOTHING IS KEPT. The body is passed to `take` in whatever pieces the network
        /// delivers it in and is not held anywhere afterwards, because what is being fetched is a
        /// firmware image and this part has no room to hold one. `take` answers false to stop.
        ///
        ///   THE LENGTH IS REQUIRED, not merely read. Without it there is no telling a truncated
        /// image from a complete one, and an image is not a thing to guess about; a server that
        /// does not say is refused rather than trusted.
        ///
        ///   No name is resolved: the address is given. Resolving one means another service to
        /// depend on and another thing to go wrong at the moment of an update, and an address in
        /// a command is a smaller promise than a name in a configuration.
        pub fn fetch(&mut self, ip: [u8; 4], port: u16, path: &str, sink: &mut dyn FnMut(Incoming) -> bool) -> Result<u32, FetchError> {
                let Radio { net, buffers, .. } = self;
                let stack = net.as_ref().ok_or(FetchError::NoNetwork)?.stack;
                if stack.config_v4().is_none() {
                        return Err(FetchError::NoNetwork);
                }

                let Fetch { rx, tx, request, response: buf, chunk } = &mut **buffers;
                let mut socket = embassy_net::tcp::TcpSocket::new(stack, &mut rx[..], &mut tx[..]);

                //   counted for this fetch alone, so the number reported is this transfer's
                DROPPED.store(0, Ordering::Relaxed);
                let address = embassy_net::IpAddress::v4(ip[0], ip[1], ip[2], ip[3]);
                let mut length = 0u32;
                let work = async {
                        socket.connect((address, port)).await.map_err(|_| FetchError::NoConnection)?;

                        //   the connection is closed by the other end when the body is done,
                        // which is what makes a body readable without trusting its length; the
                        // length is still required, to know the body was all of it
                        let head = {
                                use core::fmt::Write as _;
                                let mut w = Line::new(&mut request[..]);
                                let _ = write!(w, "GET {path} HTTP/1.1\r\nHost: {}.{}.{}.{}\r\nConnection: close\r\n\r\n", ip[0], ip[1], ip[2], ip[3]);
                                w.len
                        };
                        socket.write(&request[..head]).await.map_err(|_| FetchError::Broken)?;

                        //   read until the head is complete, keeping whatever body came with it
                        let mut have = 0usize;
                        let body_at = loop {
                                if have == buf.len() {
                                        return Err(FetchError::NotAnAnswer);
                                }
                                let n = socket.read(&mut buf[have..]).await.map_err(|_| FetchError::Broken)?;
                                if n == 0 {
                                        return Err(FetchError::NotAnAnswer);
                                }
                                have += n;
                                if let Some(at) = find(&buf[..have], b"\r\n\r\n") {
                                        break at + 4;
                                }
                        };

                        let status = status_of(&buf[..body_at]).ok_or(FetchError::NotAnAnswer)?;
                        if status != 200 {
                                return Err(FetchError::Refused(status));
                        }
                        length = content_length(&buf[..body_at]).ok_or(FetchError::NoLength)?;

                        //   WHERE THE TIME GOES, counted rather than reasoned about: what is
                        // spent handing the body on -- which on this board means writing it to
                        // storage -- against what is spent waiting for the network to deliver
                        // more. One of those is worth attacking and the other is not, and they
                        // are indistinguishable from the outside.
                        let began = crate::now_us();
                        let mut sunk_us = 0u64;
                        let mut reads = 0u32;
                        let mut taken = 0u32;

                        //   INSIDE THE ACCOUNTING, because this is where the receiver makes room
                        // and on a board that means erasing a megabyte of storage -- seconds of
                        // it. Timed outside, as it was at first, that shows up nowhere and the
                        // two columns quietly fail to add up to the time the whole thing took.
                        let at = crate::now_us();
                        let ok = sink(Incoming::Length(length));
                        sunk_us += crate::now_us() - at;
                        if !ok {
                                return Err(FetchError::Rejected);
                        }

                        let first = &buf[body_at..have];
                        if !first.is_empty() {
                                let at = crate::now_us();
                                let ok = sink(Incoming::Body(first));
                                sunk_us += crate::now_us() - at;
                                if !ok {
                                        return Err(FetchError::Rejected);
                                }
                                taken += first.len() as u32;
                        }

                        while taken < length {
                                let n = socket.read(&mut chunk[..]).await.map_err(|_| FetchError::Broken)?;
                                if n == 0 {
                                        return Err(FetchError::Short);
                                }
                                reads += 1;
                                let at = crate::now_us();
                                let ok = sink(Incoming::Body(&chunk[..n]));
                                sunk_us += crate::now_us() - at;
                                if !ok {
                                        return Err(FetchError::Rejected);
                                }
                                taken += n as u32;
                        }

                        let total_us = crate::now_us() - began;
                        light_core::info!(
                                "fetch: {reads} reads averaging {} bytes; {} ms writing it down, {} ms waiting for more; {} packets had nowhere to go",
                                if reads > 0 { taken / reads } else { 0 },
                                sunk_us / 1000,
                                total_us.saturating_sub(sunk_us) / 1000,
                                DROPPED.load(Ordering::Relaxed)
                        );
                        Ok(taken)
                };

                let mut work = core::pin::pin!(work);
                let deadline = crate::now_us() + FETCH_TIMEOUT_US;
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                loop {
                        let _ = self.task.as_mut().poll(&mut cx);
                        if let Some(net) = self.net.as_mut() {
                                let _ = net.task.as_mut().poll(&mut cx);
                        }
                        if let Poll::Ready(v) = work.as_mut().poll(&mut cx) {
                                let _ = length;
                                return v;
                        }
                        if crate::now_us() >= deadline {
                                return Err(FetchError::TooSlow);
                        }
                }
        }

        /// The radio's own pins -- the indicator on boards that put one there.
        pub fn set_gpio(&mut self, pin: u8, on: bool) {
                self.run_until(|c| c.gpio_set(pin, on));
        }

        /// Join a network, and say plainly whether it worked.
        ///
        /// Blocking, and it can take seconds: the radio is scanning, associating and running a
        /// key exchange, and there is nothing for the rest of the firmware to do in the meantime
        /// that this would not have to be woken up for anyway.
        pub fn join(&mut self, ssid: &str, password: &str) -> Result<(), JoinError> {
                //   ONCE ONLY, AND THIS IS NOT FUSSINESS. Joining assumes the part is idle: it
                // issues commands that the part accepts only while its interface is DOWN, and
                // answers with a refusal when it is already up and associated. The driver treats
                // any refusal as fatal, so a second attempt does not fail -- it stops the board.
                //
                //   Letting go of the network first is not enough, which is what makes this worth
                // saying. Disassociating leaves the interface up, so the commands are refused
                // just the same; taking it properly down and up again is not something the driver
                // offers from here. So the honest answer is to say no and explain, rather than to
                // try something that reads as tidy and takes the device out.
                if self.joined {
                        return Err(JoinError::AlreadyJoined);
                }

                //   an empty passphrase is a network that asks for none, which is a different
                // request rather than the same one with nothing in it
                let options = if password.is_empty() { cyw43::JoinOptions::new_open() } else { cyw43::JoinOptions::new(password.as_bytes()) };
                match self.run_until_or(JOIN_TIMEOUT_US, |c| c.join(ssid, options)) {
                        Some(Ok(())) => {
                                self.joined = true;
                                Ok(())
                        }
                        //   the radio's own account of what went wrong, which is the difference
                        // between looking at the name and looking at everything else
                        Some(Err(e)) => Err(match e.status {
                            STATUS_NO_NETWORKS => JoinError::NoSuchNetwork,
                            STATUS_FAIL => JoinError::Rejected,
                            other => JoinError::Refused(other),
                        }),
                        None => Err(JoinError::NoAnswer),
                }
        }
}

/// How long a join is given before it is abandoned. Associating and exchanging keys is seconds;
/// this is long enough that a slow network is not cut off, and short enough that a network which
/// is not there does not hold the application indefinitely.
const JOIN_TIMEOUT_US: u64 = 20_000_000;

//   what the radio says about a join it did not complete. There are more of these than the two
// named here; the rest are carried through as their number rather than guessed at
const STATUS_FAIL: u32 = 1;
const STATUS_NO_NETWORKS: u32 = 3;

/// How many connections the network may have open at once. One to fetch with, and room beside it
/// for whatever the address negotiation and a name lookup want.
const SOCKETS: usize = 4;
/// How long the network is given to hand out an address before the attempt is abandoned.
/// How often [`Radio::poll`] looks at an otherwise idle radio -- see there for why it is paced at
/// all. A millisecond is far below anything the upkeep it drives cares about, and far above the
/// application's loop period, which is what makes it cheap.
const IDLE_POLL_US: u64 = 1_000;

const CONFIGURE_TIMEOUT_US: u64 = 20_000_000;

/// Why there is no network.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetError {
        /// No address was handed out in the time allowed. The radio is joined but the network
        /// either did not answer or refused.
        NoAddress,
        /// The stack was already built on this radio.
        AlreadyBuilt,
}

/// Why a network was not joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinError {
        /// The radio found no network of that name within range.
        NoSuchNetwork,
        /// The network was found and would not have us: the wrong passphrase, usually, but also
        /// a network that is simply busy or unwilling at that moment. Told apart from not finding
        /// it at all, because the two send you to look in completely different places.
        Rejected,
        /// The radio refused for a reason this does not have a name for. The code is its own.
        Refused(u32),
        /// The radio reported nothing at all in the time allowed -- no such network within
        /// range, most often. Worth telling apart from a refusal, because the thing to check
        /// is different.
        NoAnswer,
        /// This radio has already joined a network, and joining a second time is not something
        /// it survives -- see `Radio::join`. Refusing is the point: the alternative is not a
        /// failed join, it is a stopped board.
        AlreadyJoined,
}

/// Somewhere to write a request into, so it can be built with the ordinary formatting machinery
/// rather than by hand. Anything past the end is dropped; the caller sends `len`.
struct Line<'a> {
        buf: &'a mut [u8],
        len: usize,
}

impl<'a> Line<'a> {
        fn new(buf: &'a mut [u8]) -> Self {
                Self { buf, len: 0 }
        }
}

impl core::fmt::Write for Line<'_> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let room = self.buf.len() - self.len;
                let take = core::cmp::min(room, s.len());
                self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
                self.len += take;
                Ok(())
        }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack.windows(needle.len()).position(|w| w == needle)
}

/// The number in `HTTP/1.1 200 OK`.
fn status_of(head: &[u8]) -> Option<u16> {
        let line = &head[..find(head, b"\r\n")?];
        let mut parts = line.split(|b| *b == b' ');
        let _version = parts.next()?;
        let code = parts.next()?;
        core::str::from_utf8(code).ok()?.parse().ok()
}

/// How long the body says it is. The name is matched without regard to case, because the
/// standard allows any and servers use several.
fn content_length(head: &[u8]) -> Option<u32> {
        const NAME: &[u8] = b"content-length:";
        let mut at = 0;
        while at < head.len() {
                let end = at + find(&head[at..], b"\r\n")?;
                let line = &head[at..end];
                if line.len() > NAME.len() && line[..NAME.len()].eq_ignore_ascii_case(NAME) {
                        let value = core::str::from_utf8(&line[NAME.len()..]).ok()?;
                        return value.trim().parse().ok();
                }
                at = end + 2;
        }
        None
}

/// What a fetch hands out, in the order it becomes known.
pub enum Incoming<'a> {
        /// How long the body is, told ONCE and before any of it. What is being fetched has to be
        /// given its size before its first byte -- storage is erased to fit -- so this is not a
        /// convenience, it is the reason the length is demanded of the server.
        Length(u32),
        /// A piece of the body, in the order it arrived. Not kept here afterwards.
        Body(&'a [u8]),
}

/// Why a fetch did not produce an image.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FetchError {
        /// There is no network to fetch over -- nothing joined, or no address taken.
        NoNetwork,
        /// The other end refused the connection or never answered it.
        NoConnection,
        /// The connection broke part-way.
        Broken,
        /// The answer was not one this understands: not a reply at all, or a reply whose head
        /// is longer than there is room to read it in.
        NotAnAnswer,
        /// The server answered, and its answer was a refusal. The code is carried so that a
        /// missing file and a broken server are told apart.
        Refused(u16),
        /// The answer did not say how long it was. Without that there is no way to know a
        /// truncated image from a complete one, and an image is not a thing to guess about.
        NoLength,
        /// The body stopped before the length promised.
        Short,
        /// Whatever was being written to could not take it.
        Rejected,
        /// It did not finish in the time allowed.
        TooSlow,
}

/// How long a whole fetch is given, connection and all.
const FETCH_TIMEOUT_US: u64 = 120_000_000;
/// Room for the answer's head -- the status line and the headers before the body.
const HEAD_MAX: usize = 1024;

/// The reset line, as the radio stack wants to hold it.
struct PwrPin(Output);

impl embedded_hal_1::digital::ErrorType for PwrPin {
        type Error = core::convert::Infallible;
}

impl embedded_hal_1::digital::OutputPin for PwrPin {
        fn set_low(&mut self) -> Result<(), Self::Error> {
                self.0.set(false);
                Ok(())
        }

        fn set_high(&mut self) -> Result<(), Self::Error> {
                self.0.set(true);
                Ok(())
        }
}
