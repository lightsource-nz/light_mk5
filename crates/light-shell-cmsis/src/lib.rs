//! The Rust side of the bare-CMSIS shell: the console and the panic path every STM32/CMSIS
//! firmware layers on the C shell, which hands `light_app_main` its resolved clocks and nothing
//! more -- no stdio, no console of its own. That handshake is the same for every board on the
//! bare-CMSIS shell, and chip-independent, so it lives here rather than in each board's crate.
//! (The RP2 shell's analogue is `light_rp2::shell`; it differs because that port is multicore and
//! runs the console on core 1. Here there is one core, and the console is serviced from within
//! the runtime loop.)
//!
//! The console is [`Console`]: the CDC class over the port's USB device controller, the port's
//! USART (any [`Transport`]), and the ITM stimulus port whenever a debugger has enabled it. A board
//! builds it once from the port's `usb::init` and `uart`, [`install`]s it, and its console module
//! calls [`service`] each poll. A board's instantiation crate keeps only the thin
//! `#[no_mangle] light_app_main` entry and the `#[panic_handler]` wrapper that call in.

#![no_std]

use core::cell::RefCell;
use core::ffi::{c_char, CStr};
use core::fmt::Write;

use critical_section::Mutex;
use heapless::Deque;
use light_core::atomic::{AtomicBool, Ordering};
use light_core::{log, usb};
use usb_device::bus::{UsbBus, UsbBusAllocator};
use usb_device::device::{StringDescriptors, UsbDevice, UsbDeviceBuilder, UsbDeviceState, UsbVidPid};
use usbd_serial::{SerialPort, USB_CLASS_CDC};

/// What the C shell hands `light_app_main`: the resolved clock rates, and a static string saying
/// what its clock tree did, for the board to log once the console is up. One struct for every
/// bare-CMSIS board (the C shell defines it once); a chip reads the fields its clock tree needs.
#[repr(C)]
pub struct ShellInfo {
        pub clk_sys_hz: u32,
        pub clk_ahb_hz: u32,
        pub clk_apb2_hz: u32,
        pub clk_tim_hz: u32,
        clock_status: *const c_char,
}

impl ShellInfo {
        /// The shell's account of its clock tree: which clock the core runs on and what fell back.
        pub fn clock_status(&self) -> &str {
                if self.clock_status.is_null() {
                        return "";
                }
                // SAFETY: the shell hands a NUL-terminated string literal of static storage
                unsafe { CStr::from_ptr(self.clock_status) }.to_str().unwrap_or("")
        }
}

/// A byte transport the console drives beside the USB device: the port's USART. Both directions
/// are non-blocking -- `write` takes what it has room for right now and returns how many, `read`
/// hands back a byte only if one is waiting -- so the console loop never waits on a wire.
pub trait Transport {
        fn write(&mut self, bytes: &[u8]) -> usize;
        fn read(&mut self) -> Option<u8>;
}

/// A fixed-capacity `fmt::Write` sink on the stack -- for formatting a log line or a panic message
/// without an allocator. Writes past `N` are dropped.
struct StackString<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> StackString<N> {
        const fn new() -> Self {
                Self { buf: [0; N], len: 0 }
        }
        fn as_bytes(&self) -> &[u8] {
                &self.buf[..self.len]
        }
}

impl<const N: usize> Write for StackString<N> {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
                let room = N - self.len;
                let take = room.min(s.len());
                self.buf[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
                self.len += take;
                Ok(())
        }
}

// --- the ITM port -----------------------------------------------------------------------------

/// How long a byte waits for room in the ITM FIFO. A debugger that enables ITM without draining
/// SWO (OpenOCD does, after flashing) would otherwise stop the firmware dead here.
const ITM_SPINS: u32 = 2000;

/// Write to stimulus port 0 if a debugger has enabled it; bounded, and lossy past the bound.
fn itm_write(bytes: &[u8]) {
        // SAFETY: the ITM block's own registers, read and written as the CMSIS helpers do; the
        // stimulus port is write-only hardware, so the exclusive reference is a formality
        let itm = unsafe { &mut *(cortex_m::peripheral::ITM::PTR as *mut cortex_m::peripheral::itm::RegisterBlock) };
        let enabled = itm.tcr.read() & 1 != 0 && itm.ter[0].read() & 1 != 0;
        if !enabled {
                return;
        }
        let port = &mut itm.stim[0];
        for &b in bytes {
                let mut spins = ITM_SPINS;
                while !port.is_fifo_ready() {
                        spins -= 1;
                        if spins == 0 {
                                return;
                        }
                }
                port.write_u8(b);
        }
}

// --- the console --------------------------------------------------------------------------------

static USB_MOUNTED: AtomicBool = AtomicBool::new(false);

/// Whether the USB device is enumerated and configured.
pub fn usb_mounted() -> bool {
        USB_MOUNTED.load(Ordering::Relaxed)
}

/// How many bytes wait for the USART. A USART with a one-deep transmit register takes one byte
/// per pass, so a burst of log lines lives here until it has gone; past this it is dropped.
const UART_QUEUE: usize = 512;

/// The console: the CDC class over the port's USB device, the port's USART, and the ITM port.
pub struct Console<'a, B: UsbBus, U: Transport> {
        serial: SerialPort<'a, B, [u8; 128], [u8; 512]>,
        dev: UsbDevice<'a, B>,
        uart: Option<U>,
        uart_queue: Deque<u8, UART_QUEUE>,
}

impl<'a, B: UsbBus, U: Transport> Console<'a, B, U> {
        /// Build the console on the port's bus. `serial_number` names the chip family, so two
        /// boards on one host stay distinct devices.
        pub fn new(alloc: &'a UsbBusAllocator<B>, uart: Option<U>, serial_number: &'static str) -> Self {
                //   a 512-byte write store: a burst of log lines while the host is not reading is
                // absorbed here, and what does not fit is dropped -- never waited for
                let serial = SerialPort::new_with_store(alloc, [0u8; 128], [0u8; 512]);
                let dev = UsbDeviceBuilder::new(alloc, UsbVidPid(usb::VID, usb::PID))
                        .strings(&[StringDescriptors::default().manufacturer(usb::MANUFACTURER).product(usb::PRODUCT).serial_number(serial_number)])
                        .expect("descriptor strings")
                        .device_class(USB_CLASS_CDC)
                        .build();
                Self { serial, dev, uart, uart_queue: Deque::new() }
        }

        /// Advance the USB device stack and publish its state.
        fn poll(&mut self) {
                self.dev.poll(&mut [&mut self.serial]);
                USB_MOUNTED.store(self.dev.state() == UsbDeviceState::Configured, Ordering::Relaxed);
        }

        /// A line to every transport. One that has no room drops it -- never waited for.
        fn write(&mut self, bytes: &[u8]) {
                let _ = self.serial.write(bytes);
                itm_write(bytes);
                if self.uart.is_some() {
                        for &b in bytes {
                                if self.uart_queue.push_back(b).is_err() {
                                        break;
                                }
                        }
                }
        }

        /// Hand the USART what it will take right now.
        fn pump_uart(&mut self) {
                let Some(uart) = self.uart.as_mut() else { return };
                while let Some(&b) = self.uart_queue.front() {
                        if uart.write(core::slice::from_ref(&b)) == 0 {
                                break;
                        }
                        self.uart_queue.pop_front();
                }
        }

        /// Input from any transport, to `push`.
        fn read_into(&mut self, push: &mut dyn FnMut(u8)) {
                if let Some(u) = self.uart.as_mut() {
                        for _ in 0..32 {
                                match u.read() {
                                        Some(b) => push(b),
                                        None => break,
                                }
                        }
                }
                let mut buf = [0u8; 32];
                if let Ok(n) = self.serial.read(&mut buf) {
                        for &b in &buf[..n] {
                                push(b);
                        }
                }
        }
}

/// What the installed console does for the free functions below, whatever its port types.
trait Sink: Send {
        fn service(&mut self, push: &mut dyn FnMut(u8)) -> bool;
        fn flush(&mut self, max: usize);
        fn emergency(&mut self, msg: &[u8]);
}

impl<B: UsbBus + 'static, U: Transport + Send + 'static> Sink for Console<'static, B, U> {
        fn service(&mut self, push: &mut dyn FnMut(u8)) -> bool {
                self.poll();
                // the log drain, bounded per pass so a burst of log cannot starve the rest; a line
                // a transport cannot take is dropped, never waited for
                let drained = log::drain(4, |record| {
                        let mut line = StackString::<160>::new();
                        let _ = write!(line, "{record}\r\n");
                        self.write(line.as_bytes());
                });
                self.pump_uart();
                let mut got = false;
                self.read_into(&mut |b| {
                        got = true;
                        push(b);
                });
                drained > 0 || got
        }

        fn flush(&mut self, max: usize) {
                log::drain(max, |record| {
                        let mut line = StackString::<160>::new();
                        let _ = write!(line, "{record}\r\n");
                        self.write(line.as_bytes());
                });
                // keep the transports moving so the last words actually leave
                for _ in 0..200_000 {
                        self.poll();
                        self.pump_uart();
                        if self.uart_queue.is_empty() && self.serial.flush().is_ok() {
                                break;
                        }
                }
        }

        fn emergency(&mut self, msg: &[u8]) {
                self.write(msg);
                for _ in 0..500_000 {
                        self.poll();
                        self.pump_uart();
                }
        }
}

static SINK: Mutex<RefCell<Option<&'static mut dyn Sink>>> = Mutex::new(RefCell::new(None));

/// Install the board's console, built once in `.bss`. The free functions below drive it.
pub fn install<B: UsbBus + 'static, U: Transport + Send + 'static>(console: &'static mut Console<'static, B, U>) {
        critical_section::with(|cs| {
                *SINK.borrow_ref_mut(cs) = Some(console);
        });
}

/// One pass of the console: poll the USB device, drain a bounded number of log lines to every
/// transport, and pump input from any of them to `push`. Returns whether anything moved. On the
/// single core there is no core-1 loop, so a board calls this from its console module each poll.
pub fn service(mut push: impl FnMut(u8)) -> bool {
        critical_section::with(|cs| match SINK.borrow_ref_mut(cs).as_mut() {
                Some(sink) => sink.service(&mut push),
                None => false,
        })
}

/// Drain up to `max` queued log lines and see them out: for a shutdown, before going quiet.
pub fn flush(max: usize) {
        critical_section::with(|cs| {
                if let Some(sink) = SINK.borrow_ref_mut(cs).as_mut() {
                        sink.flush(max);
                }
        });
}

// --- the panic path ------------------------------------------------------------------------------

/// The last panic's message, kept in memory for a debugger whether or not it got out.
static PANIC_MESSAGE: Mutex<RefCell<StackString<256>>> = Mutex::new(RefCell::new(StackString::new()));

/// Format a panic, print it through the console if nothing else holds it, and halt at a
/// breakpoint with the message still in memory. Never returns. Each board's `#[panic_handler]`
/// calls this. There is no bootloader to fall into; the debug probe reflashes a halted part.
pub fn panic_report(info: &core::panic::PanicInfo) -> ! {
        let mut line = StackString::<300>::new();
        let _ = write!(line, "\r\n*** PANIC ***\r\n{info}\r\n");
        critical_section::with(|cs| {
                if let Ok(mut m) = PANIC_MESSAGE.borrow(cs).try_borrow_mut() {
                        *m = StackString::new();
                        let _ = write!(m, "{info}");
                }
                //   a panic raised inside the console's own servicing finds it borrowed: the
                // message stays in memory and reaches the ITM port, which needs no state
                match SINK.borrow(cs).try_borrow_mut() {
                        Ok(mut sink) => match sink.as_mut() {
                                Some(sink) => sink.emergency(line.as_bytes()),
                                None => itm_write(line.as_bytes()),
                        },
                        Err(_) => itm_write(line.as_bytes()),
                }
        });
        cortex_m::asm::bkpt();
        loop {
                core::hint::spin_loop();
        }
}
