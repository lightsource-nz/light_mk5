//! The PIO host role: a full-speed USB host controller with no USB controller in it. The bus
//! ([`crate::usb_pio`]) moves one packet at a time; this module is the frame engine that turns
//! packets into transfers, and the [`HostController`] face the ecosystem's host stack drives --
//! so enumeration, hub handling and the MIDI class above are the same code as the native host
//! role's, and only the thing under them changed.
//!
//! THE ENGINE RUNS FROM A ONE-MILLISECOND TIMER INTERRUPT, not from the runtime loop, and that
//! is a departure from how every other stack in this port is driven. A full-speed host must
//! start a frame every millisecond, and inside a transaction the answers come within sixteen
//! bit times -- about a microsecond. Neither can wait for a runtime pass. So the frame handler
//! owns the wire: on each tick it sends the start-of-frame, runs one transaction for every
//! queued transfer, watches for the device going away, and returns. The futures the host stack
//! polls do no bus work at all; they queue a transfer in a slot, read its outcome, and free the
//! slot -- the handshake between them is a few words in memory, written by one side at a time.
//!
//! One transaction per endpoint per frame, as the reference host does it. That bounds a frame's
//! work to a few hundred microseconds with every slot busy, and bounds the latency of any
//! endpoint to a millisecond -- which is the interval the native controller polls a MIDI
//! endpoint at anyway.
//!
//! What the two sides share is in one static, and the rule is simple because the handler and
//! the futures run on the SAME core: the handler is atomic against everything else on that
//! core, and the futures touch the engine only inside the port's critical section, which keeps
//! the handler out for the few hundred nanoseconds it takes. No flag is checked twice, no
//! transfer is "started" while being cancelled, because nothing interleaves.
//!
//! Full speed only: a low-speed device is reported as present and left alone, and a request to
//! talk through a hub with a preamble is refused. See the bus module for why.

use core::cell::{Cell, UnsafeCell};
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll};

use cotton_usb_host::host_controller::{DataPhase, DeviceStatus, HostController, InterruptPacket, TransferExtras, TransferType, UsbError, UsbSpeed};
use cotton_usb_host::wire::SetupPacket;
use futures::Stream;

use crate::pac;
use crate::usb_pio::{self, LineState, PioUsbBus, ENCODED_MAX, MAX_PACKET, PID_ACK, PID_DATA0, PID_DATA1, PID_IN, PID_NAK, PID_OUT, PID_SETUP, PID_SOF, PID_STALL, SYNC};

/// Transfer slots: every control, bulk and interrupt transfer in flight holds one. Four
/// instruments behind a hub want a control and two data endpoints each plus the hub's own two,
/// which leaves room for a transfer or two in passing.
pub const SLOTS: usize = 16;

/// Frames a non-periodic transfer may be answered NAK before it is given up on: the five
/// seconds the specification allows a device to complete a request.
const NAK_TIMEOUT_FRAMES: u16 = 5_000;
/// Transactions in a row with no sound answer before a transfer fails: the reference host's
/// number.
const MAX_RETRIES: u8 = 3;
/// Packets an interrupt pipe holds between the frame that received them and the poll that
/// collects them: two frames of slack for a runtime pass.
const PIPE_RING: usize = 2;

/// The hardware alarm the frame tick runs on. The SDK's own alarm pool takes the last one;
/// this is the one below it.
const ALARM: usize = 2;
const FRAME_US: u32 = 1_000;
/// How early the frame alarm fires, for the handler to wait out to the exact microsecond: more
/// than the interrupt latency the application's critical sections cause. See `frame_irq`.
const SOF_LEAD_US: u32 = 30;

unsafe extern "C" {
        //   the SDK owns the RAM vector table; the handler is installed through it
        fn irq_set_exclusive_handler(num: u32, handler: extern "C" fn());
        fn irq_set_enabled(num: u32, enabled: bool);
}

//   `In` first: it is the empty slot's value, and an engine whose every field is zero at rest is
// placed in .bss rather than copied out of the image at boot
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Token {
        In,
        Out,
        Setup,
}

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Outcome {
        Pending,
        Complete,
        Stalled,
        /// No sound answer after the retries: the device is gone or broken.
        Error,
        /// Answered NAK for longer than a request may take.
        Timeout,
}

/// One transfer: where it goes, how far it has got, and what became of it.
struct Slot {
        /// Held by a future or a pipe. Nothing in the slot means anything otherwise.
        allocated: bool,
        /// Queued for the frame handler. Cleared by the handler when the transfer ends, and
        /// by the thread side when it is cancelled.
        has_transfer: bool,
        token: Token,
        address: u8,
        endpoint: u8,
        max_packet: u16,
        /// The data toggle: 0 or 1.
        data_id: u8,
        /// The application's buffer -- the future's borrow, valid while the slot is allocated.
        buf: *mut u8,
        total: u16,
        actual: u16,
        /// A zero-length packet still owed after a whole-multiple OUT.
        zlp_pending: bool,
        /// An interrupt pipe: re-armed by the handler after every packet, polled every
        /// `interval` frames, its packets kept in the ring below.
        periodic: bool,
        interval: u8,
        countdown: u8,
        outcome: Outcome,
        failed: u8,
        naks: u16,
        /// The next packet to send, raw and then encoded for the transmitter. The raw copy lives
        /// here rather than on the handler's stack so that building it is a few stores and not
        /// a call into the flash-resident clear routine.
        packet: [u8; MAX_PACKET + 4],
        encoded: [u8; ENCODED_MAX],
        encoded_len: u8,
        ring: [InterruptPacket; PIPE_RING],
        ring_head: u8,
        ring_tail: u8,
        ring_dropped: u32,
}

/// Every field zero: see `Token`. The packet size and the interval are set when a slot is taken.
const EMPTY_SLOT: Slot = Slot {
        allocated: false,
        has_transfer: false,
        token: Token::In,
        address: 0,
        endpoint: 0,
        max_packet: 0,
        data_id: 0,
        buf: core::ptr::null_mut(),
        total: 0,
        actual: 0,
        zlp_pending: false,
        periodic: false,
        interval: 0,
        countdown: 0,
        outcome: Outcome::Pending,
        failed: 0,
        naks: 0,
        packet: [0; MAX_PACKET + 4],
        encoded: [0; ENCODED_MAX],
        encoded_len: 0,
        ring: [InterruptPacket::new(), InterruptPacket::new()],
        ring_head: 0,
        ring_tail: 0,
        ring_dropped: 0,
};

/// The root port: what is attached and whether the bus is running.
struct Root {
        connected: bool,
        low_speed: bool,
        /// No frames while true: from an attach until the first reset, and during one. Means
        /// nothing while nothing is attached, which is why it rests at false.
        suspended: bool,
        /// A connect or disconnect the detect stream has not yet reported.
        detect_pending: bool,
        frame: u32,
        /// The next start-of-frame, encoded.
        sof: [u8; usb_pio::encoded_len(4)],
        sof_len: u8,
}

/// What the handler counts, for the console.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
        pub frames: u32,
        /// The longest a frame handler has run, in microseconds.
        pub worst_frame_us: u32,
        /// The furthest behind its due time a SOF has gone out, in microseconds: nonzero only
        /// when the frame interrupt was taken later than its lead.
        pub worst_late_us: u32,
        /// Transfers that failed for want of an answer, and runs of an interrupt pipe's polls
        /// that went unanswered (the pipe carries on).
        pub errors: u32,
        /// Packets an interrupt pipe lost because nothing collected the ones before.
        pub pipe_dropped: u32,
        /// Transactions a device answered at all, well-formed or not.
        pub answered: u32,
        /// The most recent transaction that went unanswered, as the receiver saw it.
        pub last_miss: Miss,
}

/// What the receiver saw on an unanswered transaction: the token that was sent, whether a
/// reply began inside the turnaround, how many bytes of it arrived, and the first two (SYNC and
/// PID, when they are sound). This is what tells a silent device from a reply this end could
/// not decode.
#[derive(Clone, Copy, Debug, Default)]
pub struct Miss {
        pub token: u8,
        pub started: u8,
        pub bytes: u8,
        pub first: [u8; 2],
}

/// Record a miss from the bus's last receive. Before `end_receive`, which forgets the PID.
#[inline(always)]
fn note_miss(bus: &PioUsbBus, token: u8, stats: &mut Stats) {
        stats.last_miss = Miss { token, started: bus.rx_started, bytes: bus.rx_count, first: [bus.rx[0], bus.rx[1]] };
}

struct Engine {
        bus: Option<PioUsbBus>,
        root: Root,
        slots: [Slot; SLOTS],
        stats: Stats,
        /// The alarm's next target, kept so the tick does not drift.
        alarm_target: u32,
}

/// The one engine, shared between the frame handler and the thread side on one core.
struct Shared(UnsafeCell<Engine>);

// SAFETY: see the module's note -- the handler is the only interrupt-context user and is
// atomic against the thread side, which enters only under the critical section
unsafe impl Sync for Shared {}

//   all zero at rest, so it is .bss: six kilobytes that would otherwise be in the image and
// copied out of it at every boot
static ENGINE: Shared = Shared(UnsafeCell::new(Engine {
        bus: None,
        root: Root { connected: false, low_speed: false, suspended: false, detect_pending: false, frame: 0, sof: [0; usb_pio::encoded_len(4)], sof_len: 0 },
        slots: [EMPTY_SLOT; SLOTS],
        stats: Stats { frames: 0, worst_frame_us: 0, worst_late_us: 0, errors: 0, pipe_dropped: 0, answered: 0, last_miss: Miss { token: 0, started: 0, bytes: 0, first: [0; 2] } },
        alarm_target: 0,
}));

/// The thread side's one door into the engine: a critical section, so the frame handler
/// cannot run while the closure does.
#[inline(always)]
fn with<R>(f: impl FnOnce(&mut Engine) -> R) -> R {
        critical_section::with(|_| {
                // SAFETY: interrupts are off on this core, so the handler is not running, and
                // the other core never touches the engine
                f(unsafe { &mut *ENGINE.0.get() })
        })
}

#[inline(always)]
fn micros() -> u32 {
        let timer = unsafe { &*pac::TIMER0::ptr() };
        timer.timerawl().read().bits()
}

/// End this pass of the host task: pending once, ready the next time it is polled.
async fn yield_now() {
        let mut yielded = false;
        core::future::poll_fn(|_cx| {
                if yielded {
                        Poll::Ready(())
                } else {
                        yielded = true;
                        Poll::Pending
                }
        })
        .await
}

// --- the frame handler ----------------------------------------------------------------------------------

/// The tick: every millisecond, from the alarm.
#[unsafe(link_section = ".time_critical.light_usb_pio_frame")]
extern "C" fn frame_irq() {
        // SAFETY: the handler is the one interrupt-context user; see the module's note
        let e = unsafe { &mut *ENGINE.0.get() };
        let timer = unsafe { &*pac::TIMER0::ptr() };
        timer.intr().write(|w| w.alarm_2().clear_bit_by_one());
        //   THE SOF GOES OUT ON THE MICROSECOND, not whenever the interrupt is taken. A hub times
        // each frame's end from the SOFs it has seen, and a frame that starts late followed by
        // one on time is a SHORT frame to the hub: its end-of-frame point lands a few
        // microseconds into the next frame, where a device behind it is answering -- and a
        // device still transmitting at that point is babble, for which the hub disables its
        // port. On a core that also runs the application the interrupt is taken 2 to 18 us
        // late, varying frame to frame, and an instrument behind the board's hub lost its port
        // within a second. So the alarm fires SOF_LEAD_US early and the handler waits out the
        // rest; latency inside the lead no longer reaches the wire
        while (e.alarm_target.wrapping_sub(micros()) as i32) > 0 {}
        let began = micros();
        //   how far behind its due time the SOF still goes: zero unless the interrupt was
        // taken later than the lead
        let late = began.wrapping_sub(e.alarm_target);
        if late < 0x8000_0000 && late > e.stats.worst_late_us {
                e.stats.worst_late_us = late;
        }
        e.alarm_target = e.alarm_target.wrapping_add(FRAME_US);
        timer.alarm2().write(|w| unsafe { w.bits(e.alarm_target.wrapping_sub(SOF_LEAD_US)) });
        frame(e);
        let took = micros().wrapping_sub(began);
        if took > e.stats.worst_frame_us {
                e.stats.worst_frame_us = took;
        }
        e.stats.frames = e.stats.frames.wrapping_add(1);
}

#[inline(always)]
fn frame(e: &mut Engine) {
        let Engine { bus, root, slots, stats, .. } = e;
        let Some(bus) = bus.as_mut() else { return };
        if root.connected {
                if !root.suspended && !connection_check(bus, root, slots, stats) {
                        return;
                }
                if !root.suspended && !root.low_speed {
                        bus.send(&root.sof[..usize::from(root.sof_len)]);
                        for slot in slots.iter_mut() {
                                if !(slot.allocated && slot.has_transfer) {
                                        continue;
                                }
                                if slot.periodic {
                                        if slot.countdown > 0 {
                                                slot.countdown -= 1;
                                                continue;
                                        }
                                        slot.countdown = slot.interval.saturating_sub(1);
                                }
                                match slot.token {
                                        Token::Setup => setup_transaction(bus, slot, stats),
                                        Token::In => in_transaction(bus, slot, stats),
                                        Token::Out => out_transaction(bus, slot, stats),
                                }
                        }
                }
        } else {
                let state = bus.line_state();
                match state {
                        LineState::FullSpeedIdle | LineState::LowSpeedIdle => {
                                root.low_speed = state == LineState::LowSpeedIdle;
                                root.connected = true;
                                // a bus reset comes before any traffic: the stack asks for it
                                root.suspended = true;
                                root.detect_pending = true;
                        }
                        _ => {}
                }
        }
        root.frame = root.frame.wrapping_add(1);
        let n = (root.frame & 0x7FF) as u16;
        let packet = [SYNC, PID_SOF, (n & 0xFF) as u8, (usb_pio::crc5(n) << 3) | (n >> 8) as u8];
        root.sof_len = usb_pio::encode(&packet, &mut root.sof) as u8;
}

/// Still attached? Two microseconds of SE0 says not: every transfer fails, and the detect
/// stream is told.
#[inline(always)]
fn connection_check(bus: &PioUsbBus, root: &mut Root, slots: &mut [Slot; SLOTS], stats: &mut Stats) -> bool {
        if bus.line_state() != LineState::Se0 {
                return true;
        }
        spin_us(1);
        if bus.line_state() != LineState::Se0 {
                return true;
        }
        spin_us(1);
        root.connected = false;
        root.suspended = true;
        root.detect_pending = true;
        for slot in slots.iter_mut() {
                if slot.allocated && slot.has_transfer {
                        complete(slot, Outcome::Error, stats);
                }
        }
        false
}

#[inline(always)]
fn spin_us(us: u32) {
        let start = micros();
        while micros().wrapping_sub(start) < us + 1 {}
}

#[inline(always)]
fn transaction_len(slot: &Slot) -> u16 {
        let remaining = slot.total.saturating_sub(slot.actual);
        remaining.min(slot.max_packet)
}

/// End a transfer: the outcome for its future, the slot's queue flag cleared.
#[inline(always)]
fn complete(slot: &mut Slot, outcome: Outcome, stats: &mut Stats) {
        slot.has_transfer = false;
        slot.outcome = outcome;
        if matches!(outcome, Outcome::Error | Outcome::Timeout) {
                stats.errors = stats.errors.wrapping_add(1);
        }
}

/// Encode the next data packet of an OUT or SETUP transfer: SYNC, the data PID, the bytes and
/// their CRC.
#[unsafe(link_section = ".time_critical.light_usb_pio_prepare_tx")]
#[inline(never)]
fn prepare_tx_data(slot: &mut Slot) {
        let n = usize::from(transaction_len(slot));
        slot.packet[0] = SYNC;
        slot.packet[1] = if slot.data_id == 1 { PID_DATA1 } else { PID_DATA0 };
        if n > 0 {
                // SAFETY: the future that queued this transfer holds the borrow of `buf`, which
                // is `total` bytes long, for as long as the slot is allocated
                let src = unsafe { core::slice::from_raw_parts(slot.buf.add(usize::from(slot.actual)), n) };
                for (d, s) in slot.packet[2..2 + n].iter_mut().zip(src) {
                        *d = *s;
                }
        }
        let crc = usb_pio::crc16(&slot.packet[2..2 + n]);
        slot.packet[2 + n] = (crc & 0xFF) as u8;
        slot.packet[3 + n] = (crc >> 8) as u8;
        let Slot { packet, encoded, encoded_len, .. } = slot;
        *encoded_len = usb_pio::encode(&packet[..n + 4], encoded) as u8;
}

/// `n` bytes moved: advance, toggle, and decide whether the transfer is done -- a short packet
/// or the whole length ends it, unless a zero-length packet is still owed.
#[inline(always)]
fn advance(slot: &mut Slot, n: u16, stats: &mut Stats) {
        slot.actual = slot.actual.wrapping_add(n);
        slot.data_id ^= 1;
        let whole = slot.actual >= slot.total;
        if whole && slot.zlp_pending && n == slot.max_packet {
                // the last full packet went; one empty one follows
                slot.zlp_pending = false;
                prepare_tx_data(slot);
                return;
        }
        if n < slot.max_packet || whole {
                if slot.periodic {
                        publish_pipe_packet(slot, stats);
                } else {
                        complete(slot, Outcome::Complete, stats);
                }
        } else if slot.token == Token::Out {
                prepare_tx_data(slot);
        }
}

/// An interrupt pipe received a packet into the ring's head entry: hand it over and re-arm.
#[inline(always)]
fn publish_pipe_packet(slot: &mut Slot, stats: &mut Stats) {
        let head = usize::from(slot.ring_head);
        slot.ring[head].size = slot.actual as u8;
        slot.ring[head].address = slot.address;
        slot.ring[head].endpoint = slot.endpoint;
        let next = ((head + 1) % PIPE_RING) as u8;
        if next == slot.ring_tail {
                // nobody collected the last one: this one is lost, counted
                slot.ring_dropped = slot.ring_dropped.wrapping_add(1);
                stats.pipe_dropped = stats.pipe_dropped.wrapping_add(1);
        } else {
                slot.ring_head = next;
        }
        slot.actual = 0;
        slot.has_transfer = true;
}

#[unsafe(link_section = ".time_critical.light_usb_pio_in")]
#[inline(never)]
fn in_transaction(bus: &mut PioUsbBus, slot: &mut Slot, stats: &mut Stats) {
        let expect = if slot.data_id == 1 { PID_DATA1 } else { PID_DATA0 };
        bus.prepare_receive();
        bus.send_token(PID_IN, slot.address, slot.endpoint);
        bus.start_receive();
        let got = bus.receive_packet_and_handshake(PID_ACK);
        let pid = bus.rx[1];
        let mut sound = true;
        match got {
                Some(len) => {
                        if pid == expect {
                                let n = len.min(usize::from(transaction_len(slot)));
                                if slot.periodic {
                                        let head = usize::from(slot.ring_head);
                                        let dst = &mut slot.ring[head].data;
                                        for i in 0..n {
                                                dst[i] = bus.rx[2 + i];
                                        }
                                } else if n > 0 {
                                        // SAFETY: as in prepare_tx_data -- the future's borrow
                                        let dst = unsafe { core::slice::from_raw_parts_mut(slot.buf.add(usize::from(slot.actual)), n) };
                                        for i in 0..n {
                                                dst[i] = bus.rx[2 + i];
                                        }
                                }
                                advance(slot, n as u16, stats);
                        }
                        //   the other toggle: the device re-sent what was already taken, because
                        // its ACK was lost. Acknowledged again above; nothing to keep
                }
                None => {
                        if pid == PID_NAK {
                                nak(slot, stats);
                        } else if pid == PID_STALL {
                                complete(slot, Outcome::Stalled, stats);
                        } else {
                                note_miss(bus, PID_IN, stats);
                                sound = false;
                                slot.failed += 1;
                                if slot.failed >= MAX_RETRIES {
                                        if slot.periodic {
                                                //   A PIPE OUTLIVES ITS MISSES. Nobody is waiting on
                                                // an interrupt pipe's outcome, and nothing re-arms one
                                                // that completes: ending it here stopped an
                                                // instrument's input for good after three silent
                                                // polls, and every note after that was never asked
                                                // for. Counted, and polled again at its interval; a
                                                // device that has really gone is the detach's
                                                // business, which drops the pipe
                                                slot.failed = 0;
                                                stats.errors = stats.errors.wrapping_add(1);
                                        } else {
                                                complete(slot, Outcome::Error, stats);
                                        }
                                }
                        }
                }
        }
        if sound {
                slot.failed = 0;
                stats.answered = stats.answered.wrapping_add(1);
        }
        bus.end_receive();
}

#[unsafe(link_section = ".time_critical.light_usb_pio_out")]
#[inline(never)]
fn out_transaction(bus: &mut PioUsbBus, slot: &mut Slot, stats: &mut Stats) {
        let n = transaction_len(slot);
        bus.prepare_receive();
        bus.send_token(PID_OUT, slot.address, slot.endpoint);
        bus.send(&slot.encoded[..usize::from(slot.encoded_len)]);
        bus.start_receive();
        let handshake = bus.wait_handshake();
        let mut sound = true;
        if handshake == PID_ACK {
                advance(slot, n, stats);
        } else if handshake == PID_NAK {
                nak(slot, stats);
        } else if handshake == PID_STALL {
                complete(slot, Outcome::Stalled, stats);
        } else {
                note_miss(bus, PID_OUT, stats);
                sound = false;
                slot.failed += 1;
                if slot.failed >= MAX_RETRIES {
                        complete(slot, Outcome::Error, stats);
                }
        }
        if sound {
                slot.failed = 0;
                stats.answered = stats.answered.wrapping_add(1);
        }
        bus.end_receive();
}

#[unsafe(link_section = ".time_critical.light_usb_pio_setup")]
#[inline(never)]
fn setup_transaction(bus: &mut PioUsbBus, slot: &mut Slot, stats: &mut Stats) {
        bus.prepare_receive();
        bus.send_token(PID_SETUP, slot.address, 0);
        bus.send(&slot.encoded[..usize::from(slot.encoded_len)]);
        bus.start_receive();
        let handshake = bus.wait_handshake();
        if handshake == PID_ACK {
                slot.actual = 8;
                slot.failed = 0;
                stats.answered = stats.answered.wrapping_add(1);
                complete(slot, Outcome::Complete, stats);
        } else {
                // a setup is never NAKed: anything but an ACK is a retry
                note_miss(bus, PID_SETUP, stats);
                slot.failed += 1;
                if slot.failed >= MAX_RETRIES {
                        complete(slot, Outcome::Error, stats);
                }
        }
        bus.end_receive();
}

#[inline(always)]
fn nak(slot: &mut Slot, stats: &mut Stats) {
        if slot.periodic {
                return;
        }
        slot.naks = slot.naks.saturating_add(1);
        if slot.naks >= NAK_TIMEOUT_FRAMES {
                complete(slot, Outcome::Timeout, stats);
        }
}

// --- the thread side: transfers as futures -------------------------------------------------------------------

/// What a transfer asks for.
#[derive(Clone, Copy)]
struct Request {
        token: Token,
        address: u8,
        endpoint: u8,
        max_packet: u16,
        data_id: u8,
        buf: *mut u8,
        len: u16,
        zlp: bool,
}

/// Queue a transfer in a free slot, or `None` if every slot is held.
fn submit(r: Request) -> Option<usize> {
        with(|e| {
                let idx = e.slots.iter().position(|s| !s.allocated)?;
                let slot = &mut e.slots[idx];
                *slot = Slot { allocated: true, has_transfer: true, token: r.token, address: r.address, endpoint: r.endpoint, max_packet: r.max_packet, data_id: r.data_id, buf: r.buf, total: r.len, zlp_pending: r.zlp && r.len > 0 && r.len % r.max_packet == 0, ..EMPTY_SLOT };
                if matches!(r.token, Token::Out | Token::Setup) {
                        prepare_tx_data(slot);
                }
                Some(idx)
        })
}

/// A transfer in flight: resolves to the bytes moved and the data toggle after it. Dropping
/// it cancels the transfer.
struct Transfer<'a> {
        slot: Option<usize>,
        request: Option<Request>,
        _buf: PhantomData<&'a mut [u8]>,
}

impl<'a> Transfer<'a> {
        fn new(request: Request) -> Self {
                Self { slot: None, request: Some(request), _buf: PhantomData }
        }
}

impl core::future::Future for Transfer<'_> {
        type Output = Result<(usize, u8), UsbError>;

        fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
                // a slot is taken when one is free; until then the request waits here
                if self.slot.is_none() {
                        let Some(r) = self.request else { return Poll::Pending };
                        match submit(r) {
                                Some(idx) => {
                                        self.slot = Some(idx);
                                        self.request = None;
                                }
                                None => return Poll::Pending,
                        }
                }
                let idx = self.slot.expect("a slot");
                let done = with(|e| {
                        let s = &mut e.slots[idx];
                        match s.outcome {
                                Outcome::Pending => None,
                                o => {
                                        let result = (usize::from(s.actual), s.data_id);
                                        s.allocated = false;
                                        Some((o, result))
                                }
                        }
                });
                match done {
                        None => Poll::Pending,
                        Some((outcome, (actual, data_id))) => {
                                self.slot = None;
                                Poll::Ready(match outcome {
                                        Outcome::Complete => Ok((actual, data_id)),
                                        Outcome::Stalled => Err(UsbError::Stall),
                                        Outcome::Timeout => Err(UsbError::Timeout),
                                        // no answer at all: the stack's word for a device that stopped talking
                                        Outcome::Error | Outcome::Pending => Err(UsbError::Timeout),
                                })
                        }
                }
        }
}

impl Drop for Transfer<'_> {
        fn drop(&mut self) {
                if let Some(idx) = self.slot {
                        with(|e| {
                                e.slots[idx].has_transfer = false;
                                e.slots[idx].allocated = false;
                        });
                }
        }
}

// --- the controller --------------------------------------------------------------------------------------------

/// An interrupt IN endpoint polled by the frame engine at its interval; a stream of its packets.
pub struct PioInterruptPipe {
        slot: usize,
}

impl Stream for PioInterruptPipe {
        type Item = InterruptPacket;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                let idx = self.slot;
                let packet = with(|e| {
                        let s = &mut e.slots[idx];
                        if s.ring_tail == s.ring_head {
                                return None;
                        }
                        let tail = usize::from(s.ring_tail);
                        let mut out = InterruptPacket::new();
                        out.address = s.ring[tail].address;
                        out.endpoint = s.ring[tail].endpoint;
                        out.size = s.ring[tail].size;
                        out.data[..usize::from(out.size)].copy_from_slice(&s.ring[tail].data[..usize::from(out.size)]);
                        s.ring_tail = ((tail + 1) % PIPE_RING) as u8;
                        Some(out)
                });
                match packet {
                        Some(p) => Poll::Ready(Some(p)),
                        None => Poll::Pending,
                }
        }
}

impl Drop for PioInterruptPipe {
        fn drop(&mut self) {
                let idx = self.slot;
                with(|e| {
                        e.slots[idx].has_transfer = false;
                        e.slots[idx].allocated = false;
                });
        }
}

/// The root port's attach and detach, as the stack wants them: one item per change.
pub struct PioDeviceDetect;

impl Stream for PioDeviceDetect {
        type Item = DeviceStatus;

        fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
                let change = with(|e| {
                        if !e.root.detect_pending {
                                return None;
                        }
                        e.root.detect_pending = false;
                        Some(if e.root.connected { DeviceStatus::Present(if e.root.low_speed { UsbSpeed::Low1_5 } else { UsbSpeed::Full12 }) } else { DeviceStatus::Absent })
                });
                match change {
                        Some(c) => Poll::Ready(Some(c)),
                        None => Poll::Pending,
                }
        }
}

/// The host controller: PIO0, a DMA channel, two pins and a hardware alarm, on the core that
/// constructs it.
pub struct PioHostController {
        _private: (),
}

impl PioHostController {
        /// Bring the bus up and start the frame tick. Once, on the core that will poll the host
        /// stack: the alarm's interrupt is enabled on this core, and the engine's one rule
        /// (handler and futures on the same core) follows from that.
        ///
        /// # Safety
        ///
        /// Construct once; PIO0, `dma_ch` and the pins `pin_dp` and `pin_dp + 1` belong to it.
        pub unsafe fn new(pin_dp: usize, dma_ch: usize, sys_hz: u32) -> Self {
                // SAFETY: the caller's promise, passed on
                let bus = unsafe { PioUsbBus::new(pin_dp, dma_ch, sys_hz) };
                let timer = unsafe { &*pac::TIMER0::ptr() };
                with(|e| {
                        e.bus = Some(bus);
                        e.alarm_target = micros().wrapping_add(FRAME_US);
                        timer.alarm2().write(|w| unsafe { w.bits(e.alarm_target.wrapping_sub(SOF_LEAD_US)) });
                });
                timer.inte().modify(|_, w| w.alarm_2().set_bit());
                // SAFETY: our handler on the SDK's vector table, our alarm's line
                unsafe {
                        irq_set_exclusive_handler(pac::Interrupt::TIMER0_IRQ_2 as u32, frame_irq);
                        irq_set_enabled(pac::Interrupt::TIMER0_IRQ_2 as u32, true);
                }
                let _ = ALARM;
                Self { _private: () }
        }

        /// What the frame handler has counted.
        pub fn stats() -> Stats {
                with(|e| e.stats)
        }

        /// The frame number, as the bus carries it.
        pub fn frame_number() -> u32 {
                with(|e| e.root.frame)
        }

        /// What is attached: `None` for nothing, or whether it is low speed.
        pub fn attached() -> Option<bool> {
                with(|e| if e.root.connected { Some(e.root.low_speed) } else { None })
        }

        fn refused(extras: TransferExtras) -> Result<(), UsbError> {
                if extras == TransferExtras::WithPreamble || with(|e| e.root.low_speed) {
                        //   a low-speed device, directly or behind a hub: this host does not speak
                        // it, and says so once per attempt rather than pretending to try
                        light_core::warn!("usb: a low-speed device is attached; this host is full speed only");
                        return Err(UsbError::ProtocolError);
                }
                Ok(())
        }
}

impl HostController for PioHostController {
        type InterruptPipe = PioInterruptPipe;
        type DeviceDetect = PioDeviceDetect;

        fn device_detect(&self) -> Self::DeviceDetect {
                PioDeviceDetect
        }

        fn reset_root_port(&self, rst: bool) {
                if rst {
                        with(|e| {
                                e.root.suspended = true;
                                if let Some(bus) = e.bus.as_ref() {
                                        bus.force_se0(true);
                                }
                        });
                } else {
                        with(|e| {
                                if let Some(bus) = e.bus.as_ref() {
                                        bus.force_se0(false);
                                }
                        });
                        // the lines settle back to the device's idle before the frames resume
                        spin_us(100);
                        with(|e| e.root.suspended = false);
                }
        }

        async fn control_transfer(&self, address: u8, transfer_extras: TransferExtras, packet_size: u8, setup: SetupPacket, data_phase: DataPhase<'_>) -> Result<usize, UsbError> {
                Self::refused(transfer_extras)?;
                let max = u16::from(packet_size).clamp(8, MAX_PACKET as u16);
                let mut setup_bytes = [setup.bmRequestType, setup.bRequest, (setup.wValue & 0xFF) as u8, (setup.wValue >> 8) as u8, (setup.wIndex & 0xFF) as u8, (setup.wIndex >> 8) as u8, (setup.wLength & 0xFF) as u8, (setup.wLength >> 8) as u8];
                Transfer::new(Request { token: Token::Setup, address, endpoint: 0, max_packet: 8, data_id: 0, buf: setup_bytes.as_mut_ptr(), len: 8, zlp: false }).await?;
                //   the data stage is as long as the SETUP says, and no longer than the buffer. NOT
                // the buffer's length: the stack reuses one buffer for requests of different
                // sizes, and asking for more than wLength is an IN the device has nothing for --
                // it stalls it, and the request fails after its data arrived
                let requested = usize::from(setup.wLength);
                // the data stage and the status stage both begin at DATA1
                let (moved, status_in) = match data_phase {
                        DataPhase::In(buf) => {
                                let len = buf.len().min(requested) as u16;
                                let (n, _) = Transfer::new(Request { token: Token::In, address, endpoint: 0, max_packet: max, data_id: 1, buf: buf.as_mut_ptr(), len, zlp: false }).await?;
                                (n, false)
                        }
                        DataPhase::Out(buf) => {
                                let len = buf.len().min(requested) as u16;
                                let (n, _) = Transfer::new(Request { token: Token::Out, address, endpoint: 0, max_packet: max, data_id: 1, buf: buf.as_ptr() as *mut u8, len, zlp: false }).await?;
                                (n, true)
                        }
                        DataPhase::None => (0, true),
                };
                let token = if status_in { Token::In } else { Token::Out };
                Transfer::new(Request { token, address, endpoint: 0, max_packet: max, data_id: 1, buf: core::ptr::null_mut(), len: 0, zlp: false }).await?;
                Ok(moved)
        }

        async fn bulk_in_transfer(&self, address: u8, endpoint: u8, packet_size: u16, data: &mut [u8], _transfer_type: TransferType, data_toggle: &Cell<bool>) -> Result<usize, UsbError> {
                Self::refused(TransferExtras::Normal)?;
                let len = data.len().min(u16::MAX as usize) as u16;
                let (n, toggle) = Transfer::new(Request { token: Token::In, address, endpoint: endpoint & 0xF, max_packet: packet_size.clamp(8, MAX_PACKET as u16), data_id: u8::from(data_toggle.get()), buf: data.as_mut_ptr(), len, zlp: false }).await?;
                data_toggle.set(toggle == 1);
                Ok(n)
        }

        async fn bulk_out_transfer(&self, address: u8, endpoint: u8, packet_size: u16, data: &[u8], transfer_type: TransferType, data_toggle: &Cell<bool>) -> Result<usize, UsbError> {
                Self::refused(TransferExtras::Normal)?;
                let len = data.len().min(u16::MAX as usize) as u16;
                let (n, toggle) = Transfer::new(Request { token: Token::Out, address, endpoint: endpoint & 0xF, max_packet: packet_size.clamp(8, MAX_PACKET as u16), data_id: u8::from(data_toggle.get()), buf: data.as_ptr() as *mut u8, len, zlp: transfer_type == TransferType::VariableSize }).await?;
                data_toggle.set(toggle == 1);
                Ok(n)
        }

        async fn alloc_interrupt_pipe(&self, address: u8, transfer_extras: TransferExtras, endpoint: u8, max_packet_size: u16, interval_ms: u8) -> Self::InterruptPipe {
                loop {
                        if let Ok(pipe) = self.try_alloc_interrupt_pipe(address, transfer_extras, endpoint, max_packet_size, interval_ms) {
                                return pipe;
                        }
                        // a slot frees when a transfer ends; polled again next pass
                        yield_now().await;
                }
        }

        fn try_alloc_interrupt_pipe(&self, address: u8, transfer_extras: TransferExtras, endpoint: u8, max_packet_size: u16, interval_ms: u8) -> Result<Self::InterruptPipe, UsbError> {
                Self::refused(transfer_extras)?;
                let max = max_packet_size.clamp(8, MAX_PACKET as u16);
                let idx = with(|e| {
                        let idx = e.slots.iter().position(|s| !s.allocated)?;
                        e.slots[idx] = Slot { allocated: true, has_transfer: true, token: Token::In, address, endpoint: endpoint & 0xF, max_packet: max, data_id: 0, total: max, periodic: true, interval: interval_ms.max(1), ..EMPTY_SLOT };
                        Some(idx)
                });
                match idx {
                        Some(slot) => Ok(PioInterruptPipe { slot }),
                        None => Err(UsbError::AllPipesInUse),
                }
        }
}
