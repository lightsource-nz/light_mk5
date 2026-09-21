//! The USB device controller as a `usb_device::bus::UsbBus`, polled. The port owns the
//! controller from the register up; the class layer above it (the CDC console, say) is the
//! `usb-device` ecosystem's, so nothing here knows what the endpoints carry.
//!
//! POLLED, NEVER INTERRUPT-DRIVEN: the controller's interrupt stays masked and the stack advances
//! only when `poll` is called -- from the console loop on core 1. That is what lets USB live on one
//! core with no per-core interrupt-enable question, and what keeps a busy application core from
//! ever touching it. The cost is that `poll` must be called often enough to service the host's
//! transfers, which a dedicated core's loop does trivially.
//!
//! The controller is the same IP on the RP2040 and the RP2350; only the pac's peripheral names
//! differ, aliased below. Its shape: a 4 KB dual-port RAM holds the setup packet (8 bytes at 0),
//! the per-endpoint control words (`ep_control`, endpoints 1-15 in each direction), the
//! per-endpoint buffer-control words (`ep_buffer_control`, every endpoint in each direction), and
//! then the data buffers. Endpoint 0 has no control word and a fixed 64-byte buffer at 0x100 that
//! its IN and OUT halves share (a control transfer is half-duplex, so they never collide); the
//! other endpoints' buffers are allocated from 0x180 up as classes ask for them.
//!
//! A transfer is driven through the buffer-control word: for an IN, the data is copied into the
//! endpoint's buffer and the word is written with the length, FULL, the data PID and LAST, then
//! AVAILABLE is set in a SECOND write after a short delay -- the controller samples the other
//! fields when AVAILABLE rises, and the datasheet asks for the gap. For an OUT, the word is armed
//! (length = max packet, not full, AVAILABLE) and the controller sets FULL when data lands; the
//! read copies it out and re-arms. Each direction of each endpoint tracks its next data PID, reset
//! to DATA0 on a bus reset and (for endpoint 0) forced to DATA1 after a SETUP, as the protocol
//! requires.
//!
//! Endpoint 0 OUT is the one endpoint armed LAZILY. Its expected PID lives in the armed word, and
//! a control transfer restarts the toggle at every SETUP; a word left armed from the previous
//! transfer therefore carries the wrong PID for the next one, and the controller acknowledges a
//! wrong-PID packet as a duplicate and discards it -- the data stage of a SET_LINE_CODING vanishes
//! and the host's port-open never returns. So a SETUP disarms the word, and it is armed with DATA1
//! only when the setup is read and says what will follow; until then the host's OUT is NAKed and
//! retried, which is harmless.

use core::cell::RefCell;

use critical_section::Mutex;
use usb_device::bus::PollResult;
use usb_device::endpoint::{EndpointAddress, EndpointType};
use usb_device::{Result as UsbResult, UsbDirection, UsbError};

use crate::pac;

#[cfg(feature = "rp2040")]
use pac::{USBCTRL_DPRAM as Dpram, USBCTRL_REGS as Regs};
#[cfg(feature = "rp2350")]
use pac::{USB as Regs, USB_DPRAM as Dpram};

/// The dual-port RAM is 4 KB.
const DPRAM_LEN: usize = 4096;
/// Endpoint 0's fixed buffer: 64 bytes at 0x100, shared by its IN and OUT halves.
const EP0_BUF: u16 = 0x100;
const EP0_MAX_PACKET: u16 = 64;
/// Where the other endpoints' buffers are allocated from, 64-byte aligned.
const DATA_BUF_START: u16 = 0x180;
const ENDPOINTS: usize = 16;

/// One direction of one endpoint.
#[derive(Clone, Copy)]
struct Ep {
        allocated: bool,
        max_packet: u16,
        /// Offset of its data buffer in the dual-port RAM.
        buf: u16,
        /// The data PID the next transfer carries (false = DATA0).
        next_pid: bool,
}

const EP_NONE: Ep = Ep { allocated: false, max_packet: 0, buf: 0, next_pid: false };

struct Inner {
        ep_in: [Ep; ENDPOINTS],
        ep_out: [Ep; ENDPOINTS],
        next_buf: u16,
        /// A SETUP packet has arrived and not yet been read.
        setup_pending: bool,
}

/// The device controller, as `usb_device` sees it. Construct once; hand it to a
/// `UsbBusAllocator` and build the device and its classes on that.
pub struct UsbBus {
        inner: Mutex<RefCell<Inner>>,
}

impl UsbBus {
        /// Take the controller: reset the block, clear its RAM, put it in device mode with the
        /// on-chip transceiver, and leave the pull-up off until [`UsbBus::enable`] (which
        /// `usb_device` calls once the classes have allocated their endpoints).
        ///
        /// # Safety
        /// Constructs the one owner of the USB controller; call it once.
        pub unsafe fn new() -> Self {
                crate::reset_cycle(true, |w| w.usbctrl().set_bit(), |w| w.usbctrl().clear_bit(), |r| r.usbctrl().bit_is_set());

                // a clean RAM: no stale control words from before the reset
                let dpram = Dpram::ptr() as *mut u8;
                for i in 0..DPRAM_LEN {
                        // SAFETY: the whole dual-port RAM, which this crate now owns
                        unsafe { core::ptr::write_volatile(dpram.add(i), 0) };
                }

                let regs = unsafe { &*Regs::ptr() };
                // the on-chip transceiver, with the mux forced to it (the SDK does the same)
                regs.usb_muxing().write(|w| w.to_phy().set_bit().softcon().set_bit());
                // pretend VBUS is present: a device that waits for real VBUS detection cannot
                // enumerate on a board that powers the chip from the same port
                regs.usb_pwr().write(|w| w.vbus_detect().set_bit().vbus_detect_override_en().set_bit());
                // device mode, controller on. The RP2350 additionally isolates the PHY after
                // power-up (MAIN_CTRL.PHY_ISO, set at reset): it must be cleared once the
                // controller is configured, or the pull-up never reaches the bus and the device
                // is invisible to the host
                #[cfg(feature = "rp2040")]
                regs.main_ctrl().write(|w| w.controller_en().set_bit().host_ndevice().clear_bit());
                #[cfg(feature = "rp2350")]
                regs.main_ctrl().write(|w| w.controller_en().set_bit().host_ndevice().clear_bit().phy_iso().clear_bit());
                // endpoint 0 raises a buffer-status bit per buffer, like the others
                regs.sie_ctrl().write(|w| w.ep0_int_1buf().set_bit());
                // polled: no interrupts
                regs.inte().write(|w| unsafe { w.bits(0) });

                Self {
                        inner: Mutex::new(RefCell::new(Inner {
                                ep_in: [EP_NONE; ENDPOINTS],
                                ep_out: [EP_NONE; ENDPOINTS],
                                next_buf: DATA_BUF_START,
                                setup_pending: false,
                        })),
                }
        }
}

#[cfg(feature = "rp2040")]
type RegsBlock = pac::usbctrl_regs::RegisterBlock;
#[cfg(feature = "rp2350")]
type RegsBlock = pac::usb::RegisterBlock;
#[cfg(feature = "rp2040")]
type DpramBlock = pac::usbctrl_dpram::RegisterBlock;
#[cfg(feature = "rp2350")]
type DpramBlock = pac::usb_dpram::RegisterBlock;

fn regs() -> &'static RegsBlock {
        // SAFETY: the controller's register block, owned by the one UsbBus
        unsafe { &*Regs::ptr() }
}

fn dpram() -> &'static DpramBlock {
        // SAFETY: the dual-port RAM's register block, owned by the one UsbBus
        unsafe { &*Dpram::ptr() }
}

/// The buffer-control word for one direction of one endpoint: endpoint 0 IN is index 0, its OUT
/// index 1, endpoint 1 IN index 2, and so on.
fn buffer_control_index(ep: u8, dir: UsbDirection) -> usize {
        usize::from(ep) * 2 + if dir == UsbDirection::In { 0 } else { 1 }
}

/// The endpoint-control word, which endpoints 1-15 have: endpoint 1 IN is index 0, its OUT 1.
fn endpoint_control_index(ep: u8, dir: UsbDirection) -> usize {
        (usize::from(ep) - 1) * 2 + if dir == UsbDirection::In { 0 } else { 1 }
}

/// The datasheet's gap between writing a buffer-control word's fields and raising AVAILABLE:
/// the controller samples the fields when AVAILABLE rises, and a few bus cycles must pass. Three
/// volatile reads of the register are comfortably more than that.
fn arm_delay(ep: u8, dir: UsbDirection) {
        let bc = dpram().ep_buffer_control(buffer_control_index(ep, dir));
        for _ in 0..3 {
                let _ = bc.read().bits();
        }
}

/// Copy `data` into the endpoint's buffer in the dual-port RAM.
fn buffer_write(buf: u16, data: &[u8]) {
        let p = Dpram::ptr() as *mut u8;
        for (i, b) in data.iter().enumerate() {
                // SAFETY: inside the allocated buffer, which lies within the 4 KB RAM
                unsafe { core::ptr::write_volatile(p.add(usize::from(buf) + i), *b) };
        }
}

/// Copy `len` bytes out of the endpoint's buffer.
fn buffer_read(buf: u16, out: &mut [u8]) {
        let p = Dpram::ptr() as *const u8;
        for (i, b) in out.iter_mut().enumerate() {
                // SAFETY: inside the allocated buffer, which lies within the 4 KB RAM
                *b = unsafe { core::ptr::read_volatile(p.add(usize::from(buf) + i)) };
        }
}

/// Arm an OUT endpoint to receive one packet: not full, the expected PID, AVAILABLE.
fn arm_out(ep: u8, e: &Ep) {
        let bc = dpram().ep_buffer_control(buffer_control_index(ep, UsbDirection::Out));
        bc.write(|w| unsafe { w.length_0().bits(e.max_packet).full_0().clear_bit().pid_0().bit(e.next_pid).last_0().set_bit() });
        arm_delay(ep, UsbDirection::Out);
        bc.modify(|_, w| w.available_0().set_bit());
}

impl usb_device::bus::UsbBus for UsbBus {
        fn alloc_ep(&mut self, ep_dir: UsbDirection, ep_addr: Option<EndpointAddress>, ep_type: EndpointType, max_packet_size: u16, _interval: u8) -> UsbResult<EndpointAddress> {
                critical_section::with(|cs| {
                        let mut inner = self.inner.borrow_ref_mut(cs);
                        let table = if ep_dir == UsbDirection::In { &mut inner.ep_in } else { &mut inner.ep_out };
                        // endpoint 0 is the control endpoint, with its fixed buffer; anything
                        // else gets the next free index and a buffer from the pool
                        let index = match ep_addr {
                                Some(a) => usize::from(a.index()),
                                None if ep_type == EndpointType::Control => 0,
                                None => (1..ENDPOINTS).find(|&i| !table[i].allocated).ok_or(UsbError::EndpointOverflow)?,
                        };
                        if index >= ENDPOINTS || table[index].allocated {
                                return Err(UsbError::InvalidEndpoint);
                        }
                        let (buf, max) = if index == 0 {
                                (EP0_BUF, EP0_MAX_PACKET)
                        } else {
                                // 64-byte aligned, from the pool; refuse when the RAM is spent
                                let need = (max_packet_size + 63) & !63;
                                let buf = inner.next_buf;
                                if usize::from(buf) + usize::from(need) > DPRAM_LEN {
                                        return Err(UsbError::EndpointMemoryOverflow);
                                }
                                inner.next_buf = buf + need;
                                (buf, max_packet_size)
                        };
                        let table = if ep_dir == UsbDirection::In { &mut inner.ep_in } else { &mut inner.ep_out };
                        table[index] = Ep { allocated: true, max_packet: max, buf, next_pid: false };
                        if index != 0 {
                                // its control word: enabled, one status bit per buffer, the type,
                                // and where its buffer is
                                let ctl = dpram().ep_control(endpoint_control_index(index as u8, ep_dir));
                                ctl.write(|w| unsafe {
                                        let w = w.enable().set_bit().interrupt_per_buff().set_bit().buffer_address().bits(buf);
                                        match ep_type {
                                                EndpointType::Control => w.endpoint_type().control(),
                                                EndpointType::Isochronous { .. } => w.endpoint_type().isochronous(),
                                                EndpointType::Bulk => w.endpoint_type().bulk(),
                                                EndpointType::Interrupt => w.endpoint_type().interrupt(),
                                        }
                                });
                        }
                        Ok(EndpointAddress::from_parts(index, ep_dir))
                })
        }

        fn enable(&mut self) {
                // the pull-up announces the device; everything else was ready at construction.
                // Arm the non-control OUT endpoints first, so the host's first packets to them
                // have somewhere to land. Endpoint 0 OUT is armed lazily (see `read`)
                critical_section::with(|cs| {
                        let inner = self.inner.borrow_ref(cs);
                        for (i, e) in inner.ep_out.iter().enumerate().skip(1) {
                                if e.allocated {
                                        arm_out(i as u8, e);
                                }
                        }
                });
                regs().sie_ctrl().modify(|_, w| w.pullup_en().set_bit());
        }

        fn reset(&self) {
                // a bus reset: back to address 0, every PID to DATA0, every buffer-control word
                // cleared, the non-control OUT endpoints re-armed (endpoint 0 OUT waits for a SETUP)
                regs().addr_endp().write(|w| unsafe { w.address().bits(0) });
                critical_section::with(|cs| {
                        let mut inner = self.inner.borrow_ref_mut(cs);
                        let inner = &mut *inner;
                        inner.setup_pending = false;
                        for e in inner.ep_in.iter_mut().chain(inner.ep_out.iter_mut()) {
                                e.next_pid = false;
                        }
                        for i in 0..ENDPOINTS {
                                dpram().ep_buffer_control(buffer_control_index(i as u8, UsbDirection::In)).write(|w| unsafe { w.bits(0) });
                                dpram().ep_buffer_control(buffer_control_index(i as u8, UsbDirection::Out)).write(|w| unsafe { w.bits(0) });
                                let e = inner.ep_out[i];
                                if i != 0 && e.allocated {
                                        arm_out(i as u8, &e);
                                }
                        }
                });
        }

        fn set_device_address(&self, addr: u8) {
                regs().addr_endp().write(|w| unsafe { w.address().bits(addr) });
        }

        fn write(&self, ep_addr: EndpointAddress, buf: &[u8]) -> UsbResult<usize> {
                if ep_addr.direction() != UsbDirection::In {
                        return Err(UsbError::InvalidEndpoint);
                }
                critical_section::with(|cs| {
                        let mut inner = self.inner.borrow_ref_mut(cs);
                        let index = usize::from(ep_addr.index());
                        let e = inner.ep_in.get(index).copied().filter(|e| e.allocated).ok_or(UsbError::InvalidEndpoint)?;
                        if buf.len() > usize::from(e.max_packet) {
                                return Err(UsbError::BufferOverflow);
                        }
                        let bc = dpram().ep_buffer_control(buffer_control_index(index as u8, UsbDirection::In));
                        // the previous packet is still waiting for the host: not yet
                        if bc.read().available_0().bit_is_set() {
                                return Err(UsbError::WouldBlock);
                        }
                        buffer_write(e.buf, buf);
                        bc.write(|w| unsafe { w.length_0().bits(buf.len() as u16).full_0().set_bit().pid_0().bit(e.next_pid).last_0().set_bit() });
                        arm_delay(index as u8, UsbDirection::In);
                        bc.modify(|_, w| w.available_0().set_bit());
                        inner.ep_in[index].next_pid = !e.next_pid;
                        Ok(buf.len())
                })
        }

        fn read(&self, ep_addr: EndpointAddress, buf: &mut [u8]) -> UsbResult<usize> {
                if ep_addr.direction() != UsbDirection::Out {
                        return Err(UsbError::InvalidEndpoint);
                }
                critical_section::with(|cs| {
                        let mut inner = self.inner.borrow_ref_mut(cs);
                        let index = usize::from(ep_addr.index());
                        // a SETUP packet takes precedence on endpoint 0: eight bytes from the RAM's
                        // setup area, not the endpoint buffer. Reading it is also when endpoint 0
                        // OUT is ARMED for what follows -- with DATA1, and only if something will
                        // come: an OUT data stage (a non-zero length), or the status-stage ZLP that
                        // closes an IN request. Arming here, not at the SETUP itself, is what makes
                        // the sequence race-free: an OUT that arrives before the arming is NAKed
                        // and retried, whereas one that lands on a word still armed with the old
                        // PID is acknowledged and silently discarded as a duplicate
                        if index == 0 && inner.setup_pending {
                                if buf.len() < 8 {
                                        return Err(UsbError::BufferOverflow);
                                }
                                let lo = dpram().setup_packet_low().read().bits().to_le_bytes();
                                let hi = dpram().setup_packet_high().read().bits().to_le_bytes();
                                buf[..4].copy_from_slice(&lo);
                                buf[4..8].copy_from_slice(&hi);
                                inner.setup_pending = false;
                                let is_in = buf[0] & 0x80 != 0;
                                let length = u16::from_le_bytes([buf[6], buf[7]]);
                                inner.ep_out[0].next_pid = true;
                                if is_in || length != 0 {
                                        let e = inner.ep_out[0];
                                        arm_out(0, &e);
                                }
                                return Ok(8);
                        }
                        let e = inner.ep_out.get(index).copied().filter(|e| e.allocated).ok_or(UsbError::InvalidEndpoint)?;
                        let bc = dpram().ep_buffer_control(buffer_control_index(index as u8, UsbDirection::Out));
                        let word = bc.read();
                        if word.full_0().bit_is_clear() {
                                return Err(UsbError::WouldBlock);
                        }
                        let len = usize::from(word.length_0().bits());
                        if len > buf.len() {
                                return Err(UsbError::BufferOverflow);
                        }
                        buffer_read(e.buf, &mut buf[..len]);
                        // the packet consumed: expect the other PID next. A non-control endpoint is
                        // re-armed at once; endpoint 0 only when a full packet says more is coming
                        // -- a short or empty one ends the stage, and it waits for the next SETUP
                        inner.ep_out[index].next_pid = !e.next_pid;
                        let e = inner.ep_out[index];
                        if index != 0 || len == usize::from(e.max_packet) {
                                arm_out(index as u8, &e);
                        }
                        Ok(len)
                })
        }

        fn set_stalled(&self, ep_addr: EndpointAddress, stalled: bool) {
                let index = ep_addr.index();
                let dir = ep_addr.direction();
                if index == 0 {
                        // endpoint 0's stall is armed in the SIE as well as the buffer control; the
                        // controller clears the arm itself on the next SETUP
                        if stalled {
                                regs().ep_stall_arm().modify(|_, w| if dir == UsbDirection::In { w.ep0_in().set_bit() } else { w.ep0_out().set_bit() });
                        }
                }
                let bc = dpram().ep_buffer_control(buffer_control_index(index as u8, dir));
                bc.modify(|_, w| w.stall().bit(stalled));
                if !stalled {
                        // a cleared halt restarts the endpoint at DATA0
                        critical_section::with(|cs| {
                                let mut inner = self.inner.borrow_ref_mut(cs);
                                let table = if dir == UsbDirection::In { &mut inner.ep_in } else { &mut inner.ep_out };
                                table[usize::from(index)].next_pid = false;
                        });
                }
        }

        fn is_stalled(&self, ep_addr: EndpointAddress) -> bool {
                dpram().ep_buffer_control(buffer_control_index(ep_addr.index() as u8, ep_addr.direction())).read().stall().bit_is_set()
        }

        fn suspend(&self) {}

        fn resume(&self) {}

        fn poll(&self) -> PollResult {
                let regs = regs();
                let status = regs.sie_status().read();
                if status.bus_reset().bit_is_set() {
                        regs.sie_status().write(|w| w.bus_reset().clear_bit_by_one());
                        return PollResult::Reset;
                }
                let mut ep_setup: u16 = 0;
                if status.setup_rec().bit_is_set() {
                        regs.sie_status().write(|w| w.setup_rec().clear_bit_by_one());
                        critical_section::with(|cs| {
                                let mut inner = self.inner.borrow_ref_mut(cs);
                                inner.setup_pending = true;
                                // the data stage that follows a SETUP starts at DATA1 in both
                                // directions, and any transfer still queued from before it is
                                // void: the IN word is cleared, and the OUT word is DISARMED --
                                // not armed -- so an OUT the host sends before the setup has been
                                // read and the word armed with the right PID is NAKed and
                                // retried, never acknowledged onto a stale word and lost. The
                                // arming happens in `read`, where the setup packet says what to
                                // expect
                                inner.ep_in[0].next_pid = true;
                                inner.ep_out[0].next_pid = true;
                                dpram().ep_buffer_control(buffer_control_index(0, UsbDirection::In)).write(|w| unsafe { w.bits(0) });
                                dpram().ep_buffer_control(buffer_control_index(0, UsbDirection::Out)).modify(|_, w| w.available_0().clear_bit());
                        });
                        ep_setup = 1;
                }
                // buffer status: bit 2n is endpoint n IN (a packet the host took), bit 2n+1 its
                // OUT (a packet that landed). Acknowledged by writing them back
                let buff = regs.buff_status().read().bits();
                let mut ep_in_complete: u16 = 0;
                let mut ep_out: u16 = 0;
                if buff != 0 {
                        regs.buff_status().write(|w| unsafe { w.bits(buff) });
                        for ep in 0..ENDPOINTS {
                                if buff & (1 << (ep * 2)) != 0 {
                                        ep_in_complete |= 1 << ep;
                                }
                                if buff & (1 << (ep * 2 + 1)) != 0 {
                                        ep_out |= 1 << ep;
                                }
                        }
                }
                if ep_setup != 0 || ep_in_complete != 0 || ep_out != 0 {
                        return PollResult::Data { ep_out, ep_in_complete, ep_setup };
                }
                if status.suspended().bit_is_set() {
                        regs.sie_status().write(|w| w.suspended().clear_bit_by_one());
                        return PollResult::Suspend;
                }
                if status.resume().bit_is_set() {
                        regs.sie_status().write(|w| w.resume().clear_bit_by_one());
                        return PollResult::Resume;
                }
                PollResult::None
        }
}
