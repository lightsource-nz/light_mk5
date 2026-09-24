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

use light_core::StaticCell;

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
struct Relay;

impl log::Log for Relay {
        fn enabled(&self, m: &log::Metadata) -> bool {
                light_core::log::enabled(level_of(m.level()))
        }

        fn log(&self, record: &log::Record) {
                //   the target is the driver's module path, which is of no use to anyone reading a
                // console -- what matters is that this came from the radio
                light_core::log::push(level_of(record.level()), "radio", *record.args());
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

/// Where the radio task's future lives: an opaque type, so it cannot be named for a static, but it
/// can be written into aligned storage once and driven through `dyn Future` from then on.
const TASK_BYTES: usize = 8 * 1024;
#[repr(C, align(16))]
struct TaskStorage([MaybeUninit<u8>; TASK_BYTES]);
static TASK: StaticCell<TaskStorage> = StaticCell::new();

fn place_task<F: Future<Output = ()> + 'static>(f: F) -> Pin<&'static mut dyn Future<Output = ()>> {
        assert!(core::mem::size_of::<F>() <= TASK_BYTES && core::mem::align_of::<F>() <= 16, "the radio task does not fit its storage");
        let storage = TASK.init(TaskStorage([MaybeUninit::uninit(); TASK_BYTES]));
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

static STATE: StaticCell<cyw43::State> = StaticCell::new();

/// The radio, once it has firmware in it.
pub struct Radio {
        task: Pin<&'static mut dyn Future<Output = ()>>,
        control: cyw43::Control<'static>,
        device: cyw43::NetDriver<'static>,
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
                let state = STATE.init(cyw43::State::new());

                let (device, control, runner) = block_on(cyw43::new(state, PwrPin(pwr), spi, firmware));
                //   the task never returns, and a future whose answer is "never" cannot be named
                // for a static on this compiler -- so it is wrapped in one whose answer is nothing
                let mut radio = Self { task: place_task(async move { runner.run().await }), control, device };

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

        /// One pass of the radio's own work: called from the runtime loop, like every other module.
        pub fn poll(&mut self) {
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                let _ = self.task.as_mut().poll(&mut cx);
        }

        /// The radio's address, which is also the first proof that the image took: it is read out
        /// of the running radio rather than out of the image.
        pub fn address(&self) -> [u8; 6] {
                use embassy_net_driver::Driver as _;
                match self.device.hardware_address() {
                        embassy_net_driver::HardwareAddress::Ethernet(a) => a,
                        _ => [0; 6],
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
                //   an empty passphrase is a network that asks for none, which is a different
                // request rather than the same one with nothing in it
                let options = if password.is_empty() { cyw43::JoinOptions::new_open() } else { cyw43::JoinOptions::new(password.as_bytes()) };
                match self.run_until_or(JOIN_TIMEOUT_US, |c| c.join(ssid, options)) {
                        Some(Ok(())) => Ok(()),
                        Some(Err(_)) => Err(JoinError::Refused),
                        None => Err(JoinError::NoAnswer),
                }
        }
}

/// How long a join is given before it is abandoned. Associating and exchanging keys is seconds;
/// this is long enough that a slow network is not cut off, and short enough that a network which
/// is not there does not hold the application indefinitely.
const JOIN_TIMEOUT_US: u64 = 20_000_000;

/// Why a network was not joined.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JoinError {
        /// The radio reported failure: the wrong passphrase, usually.
        Refused,
        /// The radio reported nothing at all in the time allowed -- no such network within
        /// range, most often. Worth telling apart from a refusal, because the thing to check
        /// is different.
        NoAnswer,
}

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
