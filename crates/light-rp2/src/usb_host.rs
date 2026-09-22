//! The USB-MIDI host role: the ecosystem's host stack (`cotton-usb-host`, over its controller
//! driver for this chip) behind [`light_midi::Host`], driven from the runtime loop one poll per
//! pass -- no executor and no interrupt. The stack is written for an async executor woken from the
//! USB IRQ, but every one of its futures re-reads the controller when polled, so polling them
//! each pass with a waker that does nothing is a complete driver of it; the IRQ its constructor
//! unmasks is masked straight back.
//!
//! What this owns: the stack's statics and the one task future that runs the bus (in `.bss`,
//! type-erased), the four MIDI device slots, and the packet mailboxes between the async side
//! and the synchronous [`light_midi::Transport`] the application drives. A MIDI IN endpoint is
//! read as a hardware-polled pipe rather than a bulk transfer: on this controller a bulk read
//! holds the one general-purpose pipe until data arrives, which would stall every other transfer
//! on the bus behind a silent instrument; the polled pipes are per endpoint, and the hardware
//! services them without the CPU.

use core::future::{poll_fn, Future};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

#[cfg(feature = "rp2040")]
use cotton_usb_host::host::rp2040::{Rp2040HostController as Controller, UsbShared, UsbStatics};
#[cfg(feature = "rp2350")]
use cotton_usb_host::host::rp2350::{Rp2350HostController as Controller, UsbShared, UsbStatics};
use cotton_usb_host::host_controller::{InterruptPacket, TransferType, UsbError};
use cotton_usb_host::usb_bus::{BulkOut, DeviceEvent, HubState, UsbBus};
use cotton_usb_host::wire::{DescriptorVisitor, EndpointDescriptor, InterfaceDescriptor};
use futures::Stream;
use light_core::{Mailbox, StaticCell};
use light_midi::{BusInfo, Host, MidiEvent, Mount, Packet, Transport};

use crate::pac;

// --- the stack's silence ------------------------------------------------------------------------

//   the stack logs through defmt, which needs a global logger to exist; this one discards
// everything, and the framework's own log carries what matters
#[defmt::global_logger]
struct Silent;

unsafe impl defmt::Logger for Silent {
        fn acquire() {}
        unsafe fn flush() {}
        unsafe fn release() {}
        unsafe fn write(_bytes: &[u8]) {}
}

// the timestamp symbol defmt's linker script would otherwise supply; nothing reads it
defmt::timestamp!("{=u32}", 0);

// --- the statics ----------------------------------------------------------------------------------

type Bus = UsbBus<Controller>;

static SHARED: UsbShared = UsbShared::new();
// (not Sync: it holds the pipe pools' cells, so it is placed once rather than declared)
static STATICS: StaticCell<UsbStatics> = StaticCell::new();
static BUS: StaticCell<Bus> = StaticCell::new();
static HUB: StaticCell<HubState<Controller>> = StaticCell::new();

/// MIDI device slots, indexed the way the application's forwarder indexes them.
pub const SLOTS: usize = 4;

/// Mount events from the bus task to the application. Eight deep: a hub coming up mounts its
/// instruments in one burst.
static EVENTS: Mailbox<MidiEvent, 8> = Mailbox::new();
const RX_MAILBOX: Mailbox<Packet, 64> = Mailbox::new();
const TX_MAILBOX: Mailbox<Packet, 64> = Mailbox::new();
/// Packets in from each slot's instrument, and packets the application wrote for it.
static RX: [Mailbox<Packet, 64>; SLOTS] = [RX_MAILBOX; SLOTS];
static TX: [Mailbox<Packet, 64>; SLOTS] = [TX_MAILBOX; SLOTS];

// --- the task: one type-erased future in .bss -----------------------------------------------------

/// Where the bus task's future lives: it is an opaque type, so it cannot be named for a static,
/// but it can be written into aligned storage and driven through `dyn Future`.
const TASK_BYTES: usize = 12 * 1024;
#[repr(C, align(16))]
struct TaskStorage([MaybeUninit<u8>; TASK_BYTES]);
static TASK: StaticCell<TaskStorage> = StaticCell::new();

fn place_task<F: Future<Output = core::convert::Infallible> + 'static>(f: F) -> Pin<&'static mut dyn Future<Output = core::convert::Infallible>> {
        assert!(core::mem::size_of::<F>() <= TASK_BYTES && core::mem::align_of::<F>() <= 16, "the USB host task does not fit its storage");
        let storage = TASK.init(TaskStorage([MaybeUninit::uninit(); TASK_BYTES]));
        let p = storage.0.as_mut_ptr() as *mut F;
        // SAFETY: sized and aligned for F, written once, and never moved again: the storage is
        // static and the only reference to it is the pinned one returned
        unsafe {
                p.write(f);
                Pin::new_unchecked(&mut *p)
        }
}

/// A waker that does nothing: the futures are polled every pass regardless.
fn noop_waker() -> Waker {
        const VTABLE: RawWakerVTable = RawWakerVTable::new(|_| RawWaker::new(core::ptr::null(), &VTABLE), |_| {}, |_| {}, |_| {});
        // SAFETY: the vtable's functions do nothing with the data pointer
        unsafe { Waker::from_raw(RawWaker::new(core::ptr::null(), &VTABLE)) }
}

/// End this pass: pending once, ready the next time.
async fn yield_now() {
        let mut yielded = false;
        poll_fn(|_cx| {
                if yielded {
                        Poll::Ready(())
                } else {
                        yielded = true;
                        Poll::Pending
                }
        })
        .await
}

/// The delay the stack asks for around port resets: polled against the port's clock.
fn delay_ms(ms: usize) -> impl Future<Output = ()> {
        let until = crate::now_us() + (ms as u64) * 1000;
        poll_fn(move |_cx| if crate::now_us() >= until { Poll::Ready(()) } else { Poll::Pending })
}

/// The stack's errors have no `Debug` outside std; named here for the log.
fn err_name(e: UsbError) -> &'static str {
        match e {
                UsbError::Stall => "stall",
                UsbError::Timeout => "timeout",
                UsbError::Overflow => "overflow",
                UsbError::BitStuffError => "bit-stuff error",
                UsbError::CrcError => "crc error",
                UsbError::DataSeqError => "data sequence error",
                UsbError::BufferTooSmall => "buffer too small",
                UsbError::AllPipesInUse => "all pipes in use",
                UsbError::ProtocolError => "protocol error",
                UsbError::TooManyDevices => "too many devices",
                UsbError::NoSuchEndpoint => "no such endpoint",
                _ => "error",
        }
}

// --- the MIDI class ---------------------------------------------------------------------------------

/// What a MIDI streaming interface's descriptors say: which configuration, which interface, its
/// bulk endpoints, and how many cables each carries (the class-specific endpoint descriptor's
/// jack count).
#[derive(Default)]
struct MidiConfig {
        config_value: u8,
        interface: Option<u8>,
        in_midi_streaming: bool,
        ep_in: Option<u8>,
        ep_out: Option<u8>,
        /// The IN endpoint's wMaxPacketSize: the pipe is opened at exactly that, not a guess.
        in_max_packet: u16,
        rx_cables: u8,
        tx_cables: u8,
        last_ep_in: bool,
        /// Every endpoint the walk passed, MIDI or not: a truncated descriptor set shows here.
        endpoints_seen: u8,
}

const CLASS_AUDIO: u8 = 1;
const SUBCLASS_MIDI_STREAMING: u8 = 3;
const CS_ENDPOINT: u8 = 0x25;
const MS_GENERAL: u8 = 0x01;

impl DescriptorVisitor for MidiConfig {
        fn on_configuration(&mut self, c: &cotton_usb_host::wire::ConfigurationDescriptor) {
                self.config_value = c.bConfigurationValue;
        }
        fn on_interface(&mut self, i: &InterfaceDescriptor) {
                self.in_midi_streaming = self.interface.is_none() && i.bInterfaceClass == CLASS_AUDIO && i.bInterfaceSubClass == SUBCLASS_MIDI_STREAMING && i.bAlternateSetting == 0;
                if self.in_midi_streaming {
                        self.interface = Some(i.bInterfaceNumber);
                }
        }
        fn on_endpoint(&mut self, e: &EndpointDescriptor) {
                self.endpoints_seen += 1;
                if !self.in_midi_streaming || e.bmAttributes & 0x03 != 0x02 {
                        return;
                }
                self.last_ep_in = e.bEndpointAddress & 0x80 != 0;
                if self.last_ep_in {
                        if self.ep_in.is_none() {
                                self.in_max_packet = u16::from_le_bytes(e.wMaxPacketSize);
                        }
                        self.ep_in.get_or_insert(e.bEndpointAddress & 0x0F);
                } else {
                        self.ep_out.get_or_insert(e.bEndpointAddress & 0x0F);
                }
        }
        fn on_other(&mut self, d: &[u8]) {
                // the class-specific endpoint descriptor follows the endpoint it describes
                if self.in_midi_streaming && d.len() >= 4 && d[1] == CS_ENDPOINT && d[2] == MS_GENERAL {
                        if self.last_ep_in {
                                self.rx_cables = d[3].max(self.rx_cables);
                        } else {
                                self.tx_cables = d[3].max(self.tx_cables);
                        }
                }
        }
}

/// Four empty pipe slots of the type `open` returns: the way to name an opaque stream type.
fn no_pipes<S>(_open: &impl Fn(&cotton_usb_host::usb_bus::UsbDevice, u8, u16) -> S) -> [Option<S>; SLOTS] {
        [None, None, None, None]
}

/// A mounted instrument.
struct Slot {
        daddr: u8,
        out: Option<BulkOut>,
}

/// What one pass of the bus task found to do.
enum Step {
        Event(DeviceEvent),
        Packet(usize, InterruptPacket),
        Idle,
}

/// The bus, forever: enumerate what arrives (the stack handles the hubs), mount the MIDI
/// interfaces into slots, read their IN pipes, and write what the application queued.
async fn host_main(bus: &'static Bus, hub: &'static HubState<Controller>) -> core::convert::Infallible {
        let mut events = core::pin::pin!(bus.device_events(hub, delay_ms));
        let mut slots: [Option<Slot>; SLOTS] = [const { None }; SLOTS];
        //   the IN pipes are streams of one opaque type; an array of them is fine as long as
        // the array never moves, which it does not: it lives in this pinned future
        let open_pipe = move |device: &cotton_usb_host::usb_bus::UsbDevice, ep: u8, max_packet: u16| bus.interrupt_endpoint_in(device, ep, max_packet.clamp(8, 64), 1);
        let mut pipes = no_pipes(&open_pipe);
        let mut out_buf = [0u8; 64];

        loop {
                let step = poll_fn(|cx| {
                        if let Poll::Ready(Some(ev)) = events.as_mut().poll_next(cx) {
                                return Poll::Ready(Step::Event(ev));
                        }
                        for (i, pipe) in pipes.iter_mut().enumerate() {
                                if let Some(p) = pipe.as_mut() {
                                        // SAFETY: the array is a local of a pinned future and is never moved
                                        let p = unsafe { Pin::new_unchecked(p) };
                                        if let Poll::Ready(Some(pkt)) = p.poll_next(cx) {
                                                return Poll::Ready(Step::Packet(i, pkt));
                                        }
                                }
                        }
                        Poll::Ready(Step::Idle)
                })
                .await;

                match step {
                        Step::Event(DeviceEvent::Connect(device, info)) => {
                                let daddr = device.address();
                                let mut cfg = MidiConfig::default();
                                if let Err(e) = bus.get_configuration(&device, &mut cfg).await {
                                        light_core::warn!("usb: device {} ({:04x}:{:04x}): configuration unreadable: {}", daddr, info.vid, info.pid, err_name(e));
                                        continue;
                                }
                                let (Some(ep_in), Some(ep_out)) = (cfg.ep_in, cfg.ep_out) else {
                                        light_core::info!("usb: device {} ({:04x}:{:04x}) class {} is not a MIDI device; ignored", daddr, info.vid, info.pid, info.class);
                                        continue;
                                };
                                let Some(idx) = slots.iter().position(|s| s.is_none()) else {
                                        light_core::warn!("usb: device {} ({:04x}:{:04x}): no free MIDI slot", daddr, info.vid, info.pid);
                                        continue;
                                };
                                let mut configured = match bus.configure(device, cfg.config_value).await {
                                        Ok(d) => d,
                                        Err(e) => {
                                                light_core::warn!("usb: device {}: configure failed: {}", daddr, err_name(e));
                                                continue;
                                        }
                                };
                                let out = configured.open_out_endpoint(ep_out).ok();
                                pipes[idx] = Some(open_pipe(&configured, ep_in, cfg.in_max_packet));
                                slots[idx] = Some(Slot { daddr, out });
                                light_core::info!("usb: device {} ({:04x}:{:04x}): MIDI interface {}, in ep {} ({} cables, {} bytes), out ep {} ({} cables), {} endpoints seen", daddr, info.vid, info.pid, cfg.interface.unwrap_or(0), ep_in, cfg.rx_cables, cfg.in_max_packet, ep_out, cfg.tx_cables, cfg.endpoints_seen);
                                let mount = Mount { daddr, rx_cables: cfg.rx_cables.max(1), tx_cables: cfg.tx_cables.max(1) };
                                // where it sits: the hub in front of it and that hub's port, for the display
                                let position = hub.topology().parent_of(daddr).map(|(hub_addr, hub_port)| BusInfo { hub_addr, hub_port });
                                let _ = EVENTS.push(MidiEvent::Mounted { idx: idx as u8, mount, bus: position });
                        }
                        Step::Event(DeviceEvent::HubConnect(h)) => {
                                light_core::info!("usb: hub at address {}", h.address());
                        }
                        Step::Event(DeviceEvent::Disconnect(gone)) => {
                                for (i, slot) in slots.iter_mut().enumerate() {
                                        if slot.as_ref().is_some_and(|s| gone.contains(s.daddr)) {
                                                pipes[i] = None;
                                                *slot = None;
                                                let _ = EVENTS.push(MidiEvent::Unmounted { idx: i as u8 });
                                        }
                                }
                        }
                        Step::Event(DeviceEvent::EnumerationError(hub_addr, port, e)) => {
                                light_core::warn!("usb: enumeration failed behind hub {} port {}: {}", hub_addr, port, err_name(e));
                        }
                        Step::Event(DeviceEvent::None) => {}
                        Step::Packet(i, pkt) => {
                                for chunk in pkt.data[..usize::from(pkt.size)].chunks_exact(4) {
                                        // a padding packet (all zero) carries nothing
                                        if chunk[0] & 0x0F != 0 {
                                                let _ = RX[i].push([chunk[0], chunk[1], chunk[2], chunk[3]]);
                                        }
                                }
                        }
                        Step::Idle => {
                                // what the application wrote, a burst per slot, then let the pass end
                                for (i, slot) in slots.iter().enumerate() {
                                        let Some(out) = slot.as_ref().and_then(|s| s.out.as_ref()) else { continue };
                                        let mut n = 0;
                                        while n + 4 <= out_buf.len() {
                                                match TX[i].pop() {
                                                        Some(p) => {
                                                                out_buf[n..n + 4].copy_from_slice(&p);
                                                                n += 4;
                                                        }
                                                        None => break,
                                                }
                                        }
                                        if n > 0 {
                                                if let Err(e) = bus.bulk_out_transfer(out, &out_buf[..n], TransferType::FixedSize).await {
                                                        light_core::warn!("usb: slot {} write failed: {}", i, err_name(e));
                                                }
                                        }
                                }
                                yield_now().await;
                        }
                }
        }
}

// --- the Host -----------------------------------------------------------------------------------------

/// The host stack, owned by whichever module drives it.
pub struct UsbMidiHost {
        task: Pin<&'static mut dyn Future<Output = core::convert::Infallible>>,
}

impl UsbMidiHost {
        /// Bring the host stack up on this core. Once.
        pub fn init() -> Self {
                // SAFETY: the one construction of the controller; the shell touched none of it
                let mut p = unsafe { pac::Peripherals::steal() };
                #[cfg(feature = "rp2040")]
                let (regs, dpram) = (p.USBCTRL_REGS, p.USBCTRL_DPRAM);
                #[cfg(feature = "rp2350")]
                let (regs, dpram) = (p.USB, p.USB_DPRAM);
                let statics: &'static UsbStatics = STATICS.init(UsbStatics::new());
                //   the stack pulses the controller's reset with a read-modify-write of the reset
                // register; under the cross-core lock, as every reset in this crate is, or core 1's
                // console UART -- constructed at the same moment -- can be put back into reset
                let hc = critical_section::with(|_| {
                        let hc = Controller::new(&mut p.RESETS, regs, dpram, &SHARED, statics);
                        //   polled, never interrupt-driven: the constructor unmasked the IRQ for
                        // an executor this firmware does not have, and the shell has no handler
                        // for it -- masked again before interrupts come back on, since a device
                        // already attached has it pending at once
                        cortex_m::peripheral::NVIC::mask(pac::Interrupt::USBCTRL_IRQ);
                        hc
                });
                let bus: &'static Bus = BUS.init(UsbBus::new(hc));
                let hub: &'static HubState<_> = HUB.init(HubState::default());
                Self { task: place_task(host_main(bus, hub)) }
        }
}

impl Host for UsbMidiHost {
        /// One pass of the bus task.
        fn task(&mut self) {
                let waker = noop_waker();
                let mut cx = Context::from_waker(&waker);
                // the interrupt work, done here instead: buffer-status bookkeeping and wakes
                SHARED.on_irq();
                let _ = self.task.as_mut().poll(&mut cx);
        }

        fn next_event(&mut self) -> Option<MidiEvent> {
                EVENTS.pop()
        }

        fn dropped_events(&self) -> u32 {
                EVENTS.dropped()
        }

}

impl Transport for UsbMidiHost {
        fn read(&mut self, idx: u8) -> Option<Packet> {
                RX.get(usize::from(idx))?.pop()
        }
        fn write(&mut self, idx: u8, packet: &Packet) {
                if let Some(tx) = TX.get(usize::from(idx)) {
                        let _ = tx.push(*packet);
                }
        }
        fn flush(&mut self, _idx: u8) {}
}
