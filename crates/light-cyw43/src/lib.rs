//! A driver for a combined wireless and short-range radio part: one chip, two radios, one
//! half-duplex bus, and no firmware of its own until the host gives it some.
//!
//! THIS CRATE BELONGS TO THE PART, NOT TO A CHIP. It reaches the world through exactly two seams --
//! [`light_core::hal::RadioBus`] for the bus and [`light_core::hal::OutputPin`] for the line that
//! holds the part in reset -- so any chip able to turn a data line around can link it, and nothing
//! here needs changing when one does. It is target-only rather than portable because the stack it
//! hosts is written for an executor and registers a global timer, neither of which belongs in a
//! host-tested library; the contract it implements is portable and lives in `light-wireless`.
//!
//! WHAT "HOSTING" MEANS HERE. The stack below is written in the ecosystem's asynchronous style and
//! this framework has no executor. So the futures it hands out are driven by hand: one nested loop
//! with a deadline per blocking errand, and one pass of the long-running tasks from the module poll
//! that owns the radio. There is no second scheduler, and nothing in this crate sleeps.

#![no_std]

use core::future::Future;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use light_core::atomic::{AtomicU32, Ordering};
use light_core::hal::{OutputPin, RadioBus};
use light_core::{ConstStaticCell, StaticCell};

//   RE-EXPORTED RATHER THAN RESTATED: these are the contract, identical whatever the part, so a
// consumer of this driver and a consumer of any other name the same types. Taking them from here
// saves an application naming two crates to do one thing.
pub use light_wireless::{CredentialError, Credentials, HciError, HciTransport, JoinError, ScanEntry, Security, Sightings, WirelessError};
pub use light_net::{Config, FetchError, Incoming, NetError};

/// Microseconds since boot, from the framework's own clock.
///
///   TAKEN FROM THE LOG rather than from a `Clock` handed in, because the log's clock is already
/// installed by the time any of this runs -- a radio is brought up on request, long after start-up
/// -- and a second source of time would be a second thing to wire up and get wrong.
fn now_us() -> u64 {
        light_core::log::now_us()
}

// --- the two seams, adapted to what the stack below expects --------------------------------------

//   The bus seam is BLOCKING and the stack below wants an asynchronous one. Adapting upward is
// free: each method already has its answer when it returns, so the future is ready on first poll.
// Adapting the other way -- declaring the seam asynchronous -- would have put this stack's futures
// into the port interface that every other subsystem shares, which is why the seam is the shape it
// is. See `light_core::hal::RadioBus`.
struct Bus<B: RadioBus>(B);

impl<B: RadioBus> cyw43::SpiBusCyw43 for Bus<B> {
        async fn cmd_write(&mut self, write: &[u32]) -> u32 {
                self.0.write(write)
        }

        async fn cmd_read(&mut self, write: u32, read: &mut [u32]) -> u32 {
                self.0.read(write, read)
        }
}

/// The reset line, as the stack below wants to hold it: through the ecosystem's pin trait, whose
/// operations can fail, over the framework's, whose cannot.
struct Pwr<P: OutputPin>(P);

impl<P: OutputPin> embedded_hal_1::digital::ErrorType for Pwr<P> {
        type Error = core::convert::Infallible;
}

impl<P: OutputPin> embedded_hal_1::digital::OutputPin for Pwr<P> {
        fn set_low(&mut self) -> Result<(), Self::Error> {
                self.0.set(false);
                Ok(())
        }

        fn set_high(&mut self) -> Result<(), Self::Error> {
                self.0.set(true);
                Ok(())
        }
}

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
        /// Whether a join has been ATTEMPTED, which is not the same as whether one succeeded --
        /// and it is attempts, not successes, that the part counts. See [`Radio::join`].
        joined: bool,
        /// When the idle upkeep in [`Radio::poll`] last actually looked at the radio.
        polled_us: u64,
        /// The short-range radio's side of the same part: a transport carrying the standard
        /// host-controller protocol. `None` on a build that did not upload its image.
        #[cfg(feature = "bluetooth")]
        bt: Option<cyw43::bluetooth::BtDriver<'static>>,
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
        pub fn new<B: RadioBus + 'static, P: OutputPin + 'static>(bus: B, pwr: P, firmware: &[u8], clm: &[u8], nvram: &[u8]) -> Self {
                relay_log();
                let spi = Bus(bus);
                //   BUILT WHERE IT LIVES, not built here and copied there. Handing a cell a value
                // means constructing that value where the caller stands: twelve and a half
                // kilobytes of packet pool on a stack of eight, memcpy'd into the static that was
                // always its home. Given the constructor instead, the cell has somewhere to put
                // the result before it exists, and nothing is ever on the stack.
                //   The give-away for this in a disassembly is a frame far larger than a
                // function's own locals with a memcpy of nearly the same size beside it.
                let state = STATE.init_with(cyw43::State::new);

                let firmware = aligned(firmware).expect("the radio's image is where the pack puts it, four-byte aligned");
                let nvram = aligned(nvram).expect("the radio's settings are where the pack puts them, four-byte aligned");
                let (device, control, runner) = block_on(cyw43::new(state, Pwr(pwr), spi, firmware, nvram));
                //   the task never returns, and a future whose answer is "never" cannot be named
                // for a static on this compiler -- so it is wrapped in one whose answer is nothing
                let mut radio = Self { task: place_task(&RADIO_TASK, async move { runner.run().await }), control, device: Some(device), net: None, joined: false, polled_us: 0, buffers: FETCH.take(), #[cfg(feature = "bluetooth")] bt: None };

                //   the regulatory blob is loaded by the control side TALKING TO the task side, so
                // the two have to run together: this is the join the framework does by hand, and
                // the reason the task is placed before the radio is finished being built
                radio.run_until(|c| c.init(clm));
                radio
        }

        /// The same, with the part's short-range radio brought up beside its wireless one.
        ///
        ///   ONE PART, ONE BUS, ONE POWER-UP. The two radios are not two devices: they share the
        /// silicon, the bus this crate built out of PIO, and the image uploaded over it -- which
        /// is why this is a second constructor rather than a second driver, and why the vendor
        /// drop the build chooses differs (see the radio firmware helper). The short-range side
        /// still wants a patch of its own on top, which is the extra image here.
        ///
        ///   What comes back for it is a transport, not a stack: it carries the standard
        /// host-controller protocol and nothing above that. Whatever speaks profiles sits on top
        /// of it and is not this crate's business.
        #[cfg(feature = "bluetooth")]
        pub fn new_with_bluetooth<B: RadioBus + 'static, P: OutputPin + 'static>(bus: B, pwr: P, firmware: &[u8], clm: &[u8], nvram: &[u8], bt_firmware: &[u8]) -> Self {
                relay_log();
                let spi = Bus(bus);
                let state = STATE.init_with(cyw43::State::new);

                let firmware = aligned(firmware).expect("the radio's image is where the pack puts it, four-byte aligned");
                let bt_firmware = aligned(bt_firmware).expect("the short-range patch is where the pack puts it, four-byte aligned");
                let nvram = aligned(nvram).expect("the radio's settings are where the pack puts them, four-byte aligned");
                let (device, bt, control, runner) = block_on(cyw43::new_with_bluetooth(state, Pwr(pwr), spi, firmware, bt_firmware, nvram));
                let mut radio = Self {
                        task: place_task(&RADIO_TASK, async move { runner.run().await }),
                        control,
                        device: Some(device),
                        net: None,
                        joined: false,
                        polled_us: 0,
                        buffers: FETCH.take(),
                        bt: Some(bt),
                };
                radio.run_until(|c| c.init(clm));
                radio
        }

        /// The short-range radio's own address, read out of the running controller.
        ///
        ///   THE SAME PROOF THE WIRELESS SIDE GIVES, for the same reason: an address here did not
        /// come from the image, it came back over the bus from a controller that accepted a reset
        /// and answered a question. It is the one cheap thing that distinguishes "the patch was
        /// uploaded and the radio is alive" from "the upload went somewhere and nothing said so".
        ///
        /// `None` if this radio was built without the short-range image, or if the controller does
        /// not answer in time -- which is a fault to report, never a wait to hang on.
        #[cfg(feature = "bluetooth")]
        pub fn bluetooth_address(&mut self) -> Option<[u8; 6]> {
                use bt_hci::cmd::{controller_baseband::Reset, info::ReadBdAddr, Cmd as _};
                use bt_hci::event::{CommandCompleteWithStatus, Event};
                use bt_hci::transport::Transport as _;
                use bt_hci::ControllerToHostPacket;

                //   disjoint borrows: the exchange below needs the transport, and driving it needs
                // the task that carries its packets over the bus
                let Radio { task, bt, .. } = self;
                let bt = bt.as_ref()?;
                let mut buf = [0u8; 259];

                let work = async {
                        //   reset first, which is the protocol's own opening move: the controller
                        // comes up holding whatever the upload left in it
                        bt.write(&Reset::new()).await.ok()?;
                        loop {
                                //   the packet borrows the buffer, so what is wanted from it is
                                // taken out before the next read needs the buffer back
                                let (opcode, address) = {
                                        //   an event arrives as a packet and is then read as one of
                                        // the kinds it might be; only the completion of a command
                                        // concerns this errand
                                        match bt.read(&mut buf).await.ok()? {
                                                ControllerToHostPacket::Event(packet) => match Event::try_from(packet) {
                                                        Ok(Event::CommandComplete(done)) => {
                                                                let opcode = done.cmd_opcode;
                                                                //   the return parameters are only
                                                                // readable once the status in front
                                                                // of them has been accounted for
                                                                let address = if opcode == ReadBdAddr::OPCODE {
                                                                        CommandCompleteWithStatus::try_from(done)
                                                                                .ok()
                                                                                .and_then(|done| done.to_result::<ReadBdAddr>().ok())
                                                                                .map(|a| {
                                                                                        let mut out = [0u8; 6];
                                                                                        out.copy_from_slice(&a.raw()[..6]);
                                                                                        out
                                                                                })
                                                                } else {
                                                                        None
                                                                };
                                                                (Some(opcode), address)
                                                        }
                                                        _ => (None, None),
                                                },
                                                // anything else on the way past is not this errand's
                                                _ => (None, None),
                                        }
                                };
                                if let Some(address) = address {
                                        return Some(address);
                                }
                                if opcode == Some(Reset::OPCODE) {
                                        bt.write(&ReadBdAddr::new()).await.ok()?;
                                }
                        }
                };

                let deadline = crate::now_us().saturating_add(BLUETOOTH_TIMEOUT_US);
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                let mut work = core::pin::pin!(work);
                loop {
                        let _ = task.as_mut().poll(&mut cx);
                        if let Poll::Ready(v) = work.as_mut().poll(&mut cx) {
                                return v;
                        }
                        if crate::now_us() >= deadline {
                                return None;
                        }
                }
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
        pub fn configure(&mut self) -> Result<Config, NetError> {
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
                                //   HANDED BACK IN THE FRAMEWORK'S OWN SHAPE, not the stack's. What
                                // a network gave is the same four numbers whatever carried them, so
                                // a caller should not have to name the stack's types -- nor change
                                // when the stack does
                                return Ok(Config {
                                        address: config.address.address().octets(),
                                        prefix: config.address.prefix_len(),
                                        gateway: config.gateway.map(|g| g.octets()),
                                });
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

                //   HOW FAR IT GOT, KEPT WHERE A FAILURE CAN STILL READ IT. The counters below live
                // inside the errand, and an errand abandoned on its deadline takes its locals with
                // it -- so a fetch that ran out of time used to report the bare fact and nothing
                // else. That is the least useful moment to say nothing: "too slow" with no figure
                // cannot tell a link that carried almost the whole image from one that never
                // carried a byte, and those want completely different things done about them.
                let progress = core::cell::Cell::new(0u32);
                let expected = core::cell::Cell::new(0u32);

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
                        let head = light_net::get_request(&mut request[..], path, ip).ok_or(FetchError::RequestTooLong)?;
                        socket.write(&request[..head]).await.map_err(|_| FetchError::Broken)?;

                        //   READ UNTIL THE HEAD IS COMPLETE, and hand the whole of what has arrived
                        // over each time rather than only the newest piece: the boundary that ends a
                        // head can fall between two reads. What comes back says where the body
                        // begins, which is how the body bytes that arrived WITH the head are kept --
                        // on a fast link that is almost every transfer, not an edge case.
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
                                match light_net::read_head(&buf[..have], have == buf.len()) {
                                        light_net::Head::NeedMore => {}
                                        light_net::Head::Malformed => return Err(FetchError::NotAnAnswer),
                                        light_net::Head::Complete { status, length: declared, body_at } => {
                                                if status != 200 {
                                                        return Err(FetchError::Refused(status));
                                                }
                                                length = declared.ok_or(FetchError::NoLength)?;
                                                break body_at;
                                        }
                                }
                        };

                        //   WHERE THE TIME GOES, counted rather than reasoned about: what is
                        // spent handing the body on -- which on this board means writing it to
                        // storage -- against what is spent waiting for the network to deliver
                        // more. One of those is worth attacking and the other is not, and they
                        // are indistinguishable from the outside.
                        let began = crate::now_us();
                        let mut sunk_us = 0u64;
                        let mut reads = 0u32;

                        //   INSIDE THE ACCOUNTING, because this is where the receiver makes room
                        // and on a board that means erasing a megabyte of storage -- seconds of
                        // it. Timed outside, as it was at first, that shows up nowhere and the
                        // two columns quietly fail to add up to the time the whole thing took.
                        expected.set(length);
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
                                progress.set(progress.get() + first.len() as u32);
                        }

                        while progress.get() < length {
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
                                progress.set(progress.get() + n as u32);
                        }
                        let taken = progress.get();

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
                                //   SAY HOW FAR IT GOT. Zero of anything means it never connected or
                                // the head never arrived; most of the way means a link too slow for
                                // the deadline rather than a path that does not work -- and those
                                // are the two diagnoses this failure has to tell apart
                                light_core::warn!("fetch: gave up after {} of {} bytes in {} ms", progress.get(), expected.get(), FETCH_TIMEOUT_US / 1000);
                                return Err(FetchError::TooSlow);
                        }
                }
        }

        /// The radio's own pins -- the indicator on boards that put one there.
        pub fn set_gpio(&mut self, pin: u8, on: bool) {
                self.run_until(|c| c.gpio_set(pin, on));
        }

        /// Join a network, and say plainly whether it worked.
        /// Every network the radio can see, handed one at a time to `found`.
        ///
        ///   WHAT THE RADIO CAN SEE, WHICH IS A DIFFERENT QUESTION FROM WHETHER IT CAN JOIN. A join
        /// that comes back "no such network" has two quite separate explanations -- the network is
        /// not there, or this radio is not receiving properly -- and they are indistinguishable from
        /// the outside. Worse, a join cannot be tried at all without a passphrase, so the one
        /// diagnostic that needs no secret is the one that tells the two apart.
        ///
        ///   It earns its place beyond that: it is how a person finds out what a board's radio makes
        /// of a room, and it is the first thing to reach for when a radio's configuration is
        /// suspected, since bad regulatory or calibration data shows up here as a short list or an
        /// empty one rather than as an error.
        ///
        /// Blocking, and a second or two: the radio visits every channel it is allowed.
        pub fn scan(&mut self, mut found: impl FnMut(&ScanEntry)) -> usize {
                //   counted in a cell rather than a plain local, so the tally survives the sweep
                // being cut short: the caller has already been handed every network found up to
                // that point, and reporting none of them would be a lie about what was heard
                let seen = core::cell::Cell::new(0usize);
                //   ONE ENTRY PER RADIO HEARD, NOT ONE PER TIME IT WAS HEARD. What arrives here is
                // every beacon the sweep receives, and a sweep visits each channel more than once,
                // so an access point in the room answers several times and used to be listed
                // several times -- the same name and channel with a different signal each time,
                // which reads as several networks that cannot be told apart. Kept apart by the
                // address of the radio that sent the beacon rather than by the name, because two
                // radios serving one name are genuinely two things worth seeing, and one radio
                // answering twice is not.
                //
                //   Bounded, and deliberately generously: past the end nothing can be remembered,
                // so repeats would be listed again. That is the better failure -- a long list with
                // repeats still says what is in range, where dropping the overflow would quietly
                // claim a network is absent.
                let heard: core::cell::RefCell<Sightings<SCAN_MAX>> = core::cell::RefCell::new(Sightings::new());
                //   the borrow dance the rest of this file does: the errand needs the control side,
                // driving it needs the task that carries its traffic
                let Radio { task, control, .. } = self;
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                let work = async {
                        let mut scanner = control.scan(cyw43::ScanOptions::default()).await;
                        while let Some(bss) = scanner.next().await {
                                //   the rule this turns on -- one entry per radio rather than one per
                                // beacon, kept apart by the sending address -- is portable and is
                                // tested on the host; what is left here is offering each sighting
                                if !heard.borrow_mut().offer(&bss.bssid) {
                                        continue;
                                }
                                let len = usize::from(bss.ssid_len).min(bss.ssid.len());
                                //   a name that is not text is reported as empty rather than
                                // guessed at: a hidden network answers with nothing here
                                let name = core::str::from_utf8(&bss.ssid[..len]).unwrap_or("");
                                //   THE SIGNAL IS THE FIRST READING, and it is a sample rather than
                                // a measurement: the radio reports what one beacon arrived at, and
                                // successive beacons from the same place differ by several decibels.
                                // Good for "is it in range", not for comparing two sweeps.
                                //   THE ADDRESS OF THE RADIO IS REPORTED, not just the name, and it
                                // is what makes the list readable: several radios serving one name
                                // is the ordinary shape of a network that covers a building, and
                                // without the address those entries look like the same network
                                // listed twice by mistake.
                                //   the channel number, not the part's own encoding of it: the low
                                // byte of that word is the channel, and decoding it is the driver's
                                // job rather than something to leak through a portable type
                                found(&ScanEntry::new(name, bss.bssid, bss.rssi, (bss.chanspec & 0xff) as u8));
                                seen.set(seen.get() + 1);
                        }
                };
                let deadline = crate::now_us().saturating_add(SCAN_TIMEOUT_US);
                let mut work = core::pin::pin!(work);
                loop {
                        let _ = task.as_mut().poll(&mut cx);
                        if work.as_mut().poll(&mut cx).is_ready() {
                                break;
                        }
                        if crate::now_us() >= deadline {
                                break;
                        }
                }
                seen.get()
        }

        ///
        /// Blocking, and it can take seconds: the radio is scanning, associating and running a
        /// key exchange, and there is nothing for the rest of the firmware to do in the meantime
        /// that this would not have to be woken up for anyway.
        pub fn join(&mut self, credentials: &Credentials) -> Result<(), JoinError> {
                let ssid = credentials.ssid();
                let password = credentials.passphrase();
                //   NOTHING IS VALIDATED HERE, AND THAT IS NOT AN OMISSION. A name or passphrase of
                // a length the protocol forbids is refused by the part outright, and this driver
                // treats a refused command as fatal -- so the check is a safety property rather
                // than a courtesy, and it is held by [`Credentials`] being impossible to build
                // wrong. A driver that takes one cannot be handed a bad one, which is why the check
                // that used to stand here is gone rather than moved: there is nothing left to check.

                //   A SECOND ATTEMPT RE-ARMS THE PART RATHER THAN BEING REFUSED, and what that is
                // worth is the most ordinary thing anyone does at this prompt: mistype a passphrase
                // and try again. This used to answer "reset the board", because the commands a join
                // issues to set up security are accepted only while the interface is DOWN -- once it
                // is up and configured the part refuses them, and a refusal is fatal to the driver.
                //
                //   Letting go of the network is not enough on its own: disassociating leaves the
                // interface up, so the commands are refused just the same. It has to go down and
                // come back, which the driver would not do from outside until the framework's copy
                // of it offered the pair (see lib/cyw43). So the sequence is leave, then relink --
                // leave first, because taking the interface out from under a live association is a
                // larger thing to ask of the part than ending it politely.
                //
                //   IT IS THE ATTEMPT THAT COUNTS, NOT THE SUCCESS, which is why this reads the same
                // flag whichever way the last one went: a join that FAILED has already brought the
                // interface up and configured it, so it needs re-arming exactly as a successful one
                // does. Getting that backwards is what made the retry after a failure the thing that
                // stopped the board.
                if self.joined {
                        light_core::debug!("radio: re-arming the part for another join");
                        self.run_until(|c| c.leave());
                        self.run_until(|c| c.relink());
                        //   THE STACK IS LEFT STANDING, deliberately. It is built on the driver's
                        // packet interface rather than on the association, and that interface
                        // survives this; the address it holds is the stack's to renegotiate when the
                        // link comes back up. Discarding it here would be worse than leaving it: the
                        // packet interface is taken from the driver exactly once, so a stack thrown
                        // away cannot be rebuilt, and asking for an address after a second join
                        // would answer "already built" for the rest of the power-up.
                }
                self.joined = true;

                //   an empty passphrase is a network that asks for none, which is a different
                // request rather than the same one with nothing in it
                //   THE DRIVER'S DEFAULT IS LEFT ALONE, which asks to negotiate as a station that
                // can do either WPA2 or WPA3. Naming WPA2 here instead looks like the safer, more
                // specific choice and is not: it commits the radio to the older key exchange, and
                // an access point offering WPA3 -- including one offering both -- answers that by
                // associating happily and then deauthenticating the moment the keys are exchanged.
                // From above, that is indistinguishable from a wrong passphrase.
                //
                //   Worth knowing how this was got wrong once: a network stopped joining after the
                // driver was upgraded, the default was blamed for having changed, and it was pinned
                // to WPA2 on that theory. The default had not changed -- it is the same in both
                // versions -- so the pin explained nothing and broke the WPA3 case. Check what a
                // dependency actually did before building on what it is assumed to have done.
                let options = if password.is_empty() {
                        cyw43::JoinOptions::new_open()
                } else {
                        cyw43::JoinOptions::new(password.as_bytes())
                };
                match self.run_until_or(JOIN_TIMEOUT_US, |c| c.join(ssid, options)) {
                        // already marked above, because the part counts the attempt
                        Some(Ok(())) => Ok(()),
                        //   the radio's own account of what went wrong, which is the difference
                        // between looking at the name and looking at everything else. The driver
                        // names the two that matter now rather than handing back the raw status it
                        // used to, so these are its words and not a table of numbers here; a status
                        // it does not name still comes through as its number
                        Some(Err(e)) => Err(match e {
                                cyw43::JoinError::NetworkNotFound => JoinError::NotFound,
                                cyw43::JoinError::AuthenticationFailure => JoinError::Refused,
                                cyw43::JoinError::JoinFailure(status) => JoinError::Other(u32::from(status)),
                        }),
                        None => Err(JoinError::NoAnswer),
                }
        }
}

/// How long a join is given before it is abandoned. Associating and exchanging keys is seconds;
/// this is long enough that a slow network is not cut off, and short enough that a network which
/// is not there does not hold the application indefinitely.
const JOIN_TIMEOUT_US: u64 = 20_000_000;

/// How many distinct radios a sweep can remember, so it can tell a repeat from a new one. Beyond
/// this the list grows repeats again rather than losing entries -- see `Sightings`. Six bytes
/// each, so the whole table is small enough to stand on the stack of the call that builds it.
const SCAN_MAX: usize = 64;

/// How long a scan is given. The radio visits every channel it is permitted and reports as it goes,
/// so this is a ceiling on the whole sweep rather than a wait for any one answer.
const SCAN_TIMEOUT_US: u64 = 10_000_000;

/// A blob the driver will take, or `None` because it is not where it must be.
///
///   THE DRIVER ASKS FOR ITS BLOBS FOUR-BYTE ALIGNED, and the pack already lays every entry on a
/// four-byte boundary -- so this is a check that should never fail rather than work to satisfy the
/// requirement. Copying instead is not an option worth considering: the image alone is a quarter of
/// a megabyte and there is nowhere near that much memory going spare.
///
///   Answered rather than asserted, because a pack whose layout ever changed would otherwise take
/// the radio down with an alignment fault and no explanation. This way it is a radio that says it
/// cannot start.
fn aligned(blob: &[u8]) -> Option<&cyw43::Aligned<cyw43::A4, [u8]>> {
        if !(blob.as_ptr() as usize).is_multiple_of(4) {
                return None;
        }
        // SAFETY: the driver's wrapper is `#[repr(C)]` over a zero-sized alignment marker and the
        // value itself, so for a slice it is the same fat pointer with the alignment raised -- and
        // that alignment is what was just checked. Nothing else about the layout differs.
        Some(unsafe { &*(blob as *const [u8] as *const cyw43::Aligned<cyw43::A4, [u8]>) })
}

/// How many connections the network may have open at once. One to fetch with, and room beside it
/// for whatever the address negotiation and a name lookup want.
const SOCKETS: usize = 4;
/// How long the network is given to hand out an address before the attempt is abandoned.
/// How often [`Radio::poll`] looks at an otherwise idle radio -- see there for why it is paced at
/// all. A millisecond is far below anything the upkeep it drives cares about, and far above the
/// application's loop period, which is what makes it cheap.
const IDLE_POLL_US: u64 = 1_000;

/// How long the short-range controller is given to answer a question before it is called absent.
/// Generous: it is a part that has just been handed an image, and the alternative to a deadline
/// here is a board that stops with no console and no reason.
#[cfg(feature = "bluetooth")]
const BLUETOOTH_TIMEOUT_US: u64 = 5_000_000;

/// How long the network is given to hand out an address.
///
///   SIZED FOR THE SLOW CASE, WHICH IS A SECOND JOIN. A first address on a freshly joined network
/// was measured at about ten seconds; one taken after a re-join is slower, because the interface went
/// down and came back and the lease has to be got again from nothing -- measured between twenty and
/// forty seconds on a link at the weak end of usable. At twenty this reported no address and then
/// handed one over instantly when asked a second time, which is a poor thing to ask of an operator:
/// the work had succeeded and only the waiting had not.
const CONFIGURE_TIMEOUT_US: u64 = 45_000_000;

//   WHY A JoinError IS NOT DECLARED HERE. It is the same set of answers whatever the part, so it is
// part of the contract rather than of this driver -- see `light_wireless::JoinError`, re-exported
// at the top of this crate. What was declared here also carried two variants for credentials of the
// wrong length, and those are gone rather than moved: they cannot arise now that a set of
// credentials cannot be built wrong.

/// How long a whole fetch is given, connection and all.
const FETCH_TIMEOUT_US: u64 = 120_000_000;
/// Room for the answer's head -- the status line and the headers before the body.
const HEAD_MAX: usize = 1024;

