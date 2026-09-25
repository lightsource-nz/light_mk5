//! The Rust side of the C shell's ABI, shared by every RP2 board and never copied into one. The
//! C shell (pico-sdk) boots the chip, launches core 1, reads the clocks, and hands each core to
//! Rust: core 0 through the board's `light_app_main`, core 1 through its `light_app_core1_main`
//! -- whose body is [`core1_main`] here. Core 1 IS the console: the USB device stack with the CDC
//! class on it (feature `usb-console`, the device role), the UART (always: the debug-probe path,
//! and the host role's only console), the log drain, the input pump, the enumeration state, the
//! bootloader trigger and the panic relay. The shell keeps only what the bootrom does -- entering
//! BOOTSEL, reading the BOOTSEL button -- and hands its own panics here.
//!
//! A board's instantiation crate keeps only the thin `#[no_mangle]` entry points and the
//! `#[panic_handler]` wrapper that call in.

use core::cell::RefCell;
use core::fmt::Write;

use critical_section::Mutex;
use light_core::atomic::{AtomicBool, AtomicU32, Ordering};
use light_core::log;

use crate::pac;
use crate::uart::Uart;

unsafe extern "C" {
        fn light_shell_bootsel() -> bool;
        //   the host role has no console path into the bootloader -- its flash path is the probe
        #[cfg(feature = "usb-console")]
        fn light_shell_reset_to_bootsel() -> !;
        fn light_shell_core1_stack_free() -> usize;
        fn light_shell_core1_stack_size() -> usize;
}

/// Core 1's stack headroom: `(unused, total)` in bytes, from the paint the shell laid down
/// before launching the core. An overrun here lands in whatever `.bss` the linker placed below
/// the stack -- once a scanout engine's DMA control word -- and the console that would report
/// it is the core that overran, so this is read back and logged instead.
pub fn core1_stack_headroom() -> (usize, usize) {
        // SAFETY: two reads of shell-owned statics
        unsafe { (light_shell_core1_stack_free(), light_shell_core1_stack_size()) }
}

/// Is the BOOTSEL button pressed right now? The shell reads it flash-safe (interrupts off, chip
/// select briefly floated), so keep the polling rate modest -- a few times a second is plenty for a
/// button, and each read is a short interrupts-off window that a fast poll loop should not repeat
/// needlessly. A board with no other button uses this as its one input.
pub fn bootsel() -> bool {
        unsafe { light_shell_bootsel() }
}

/// What the C shell hands each core's entry: the resolved clock rates.
#[repr(C)]
pub struct ShellInfo {
        pub clk_sys_hz: u32,
        pub clk_peri_hz: u32,
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

// --- core 0 stack watermark ----------------------------------------------------------------

//   Core 0's stack spans BOTH scratch banks -- the shell hands it the one the platform would
// reserve for core 1, which runs on an array in ordinary RAM instead (a deep core-0 call chain
// once landed on core 1's frames there and killed the console silently). Painting the bottom of
// that stack and reading back what is still painted says how close the deepest call chain came to
// the floor, for a firmware's `stats`. The base is the lower bank's address, which is the floor.
//
//   IT USED TO MEAN SOMETHING WEAKER. When core 0's stack was the upper bank alone, this region
// was the vacant bank beneath it, and reaching into it meant a call chain had already run off the
// end and was living on borrowed ground. Now it is the stack, so a small number here is ordinary
// depth rather than an overrun -- and ZERO is the thing to fear, because there is nothing under
// this address at all.
#[cfg(feature = "rp2350")]
const PAINT_BASE: u32 = 0x2008_0000;
#[cfg(feature = "rp2040")]
const PAINT_BASE: u32 = 0x2004_0000;
/// The bottom 5 KB of core 0's 8 KB stack: the part worth watching. A chain that never reaches
/// here is nowhere near the floor, and one that does has its depth reported.
const PAINT_WORDS: usize = 1280;
const PAINT: u32 = 0xC0DE_55AA;

/// Paint the bottom of core 0's stack. Call FIRST in `light_app_main`, whose own frame sits near
/// the top of the stack, thousands of bytes above the painted region; core 1's stack is elsewhere
/// entirely. Called later, from anywhere deeper, it would paint over live frames.
pub fn stack_paint() {
        let p = PAINT_BASE as *mut u32;
        for i in 0..PAINT_WORDS {
                // SAFETY: the unlived bottom of core 0's stack, far below this frame
                unsafe { core::ptr::write_volatile(p.add(i), PAINT) };
        }
}

/// Untouched painted bytes above the stack's floor: how much room the deepest call chain left.
/// Zero means the paint is gone as far up as it was laid, and nothing lies below this address.
pub fn stack_free() -> u32 {
        let p = PAINT_BASE as *const u32;
        for i in 0..PAINT_WORDS {
                // SAFETY: reads the painted region
                if unsafe { core::ptr::read_volatile(p.add(i)) } != PAINT {
                        return (i * 4) as u32;
                }
        }
        (PAINT_WORDS * 4) as u32
}

// --- core 1: the console -------------------------------------------------------------------

/// Core 1's pulse, counted every pass of [`core1_main`]. The console cannot report its own death
/// (a dead core 1 IS a dead console), so a firmware's `stats` reads this from core 0 to diagnose it.
static CORE1_TICKS: AtomicU32 = AtomicU32::new(0);

/// The core 1 pulse count, for a firmware's `stats`.
pub fn core1_ticks() -> u32 {
        CORE1_TICKS.load(Ordering::Relaxed)
}

static USB_MOUNTED: AtomicBool = AtomicBool::new(false);

/// Whether the USB device is enumerated and configured: the board's only "on external power"
/// signal where it has no VBUS-sense pin. Always false on a board with no USB device stack.
pub fn usb_mounted() -> bool {
        USB_MOUNTED.load(Ordering::Relaxed)
}

/// The console UART's default wiring: UART0 on the pins the SDK's console used, at its rate, so a
/// probe's serial adapter attached the same way keeps working.
pub const UART_TX: usize = 0;
pub const UART_RX: usize = 1;
pub const UART_BAUD: u32 = 115_200;

/// The line coding the flash script sets to ask for the bootloader.
#[cfg(feature = "usb-console")]
const RESET_BAUD: u32 = 1200;
/// How long a panic's raising core waits for core 1 to relay the message.
const PANIC_RELAY_TIMEOUT_US: u64 = 2_000_000;

// --- the panic relay ------------------------------------------------------------------------
//
//   The raising core stores the message and raises PENDING; core 1 -- which owns the console --
// prints it, keeps the transports serviced so the bytes leave, and raises PRINTED; the raising
// core then finishes the panic: into the bootloader on a device-role board, so a halted board still
// takes a reflash; a halt at a breakpoint on a host-role board, whose USB port is a host port
// (BOOTSEL would be invisible and would wipe the message) and which is flashed over SWD. A panic
// raised ON core 1 cannot be relayed to itself (the console is live only inside its loop), so it
// finishes at once with the message left in memory for a debugger.

static PANIC_PENDING: AtomicBool = AtomicBool::new(false);
static PANIC_PRINTED: AtomicBool = AtomicBool::new(false);
static PANIC_MESSAGE: Mutex<RefCell<StackString<256>>> = Mutex::new(RefCell::new(StackString::new()));

fn core_id() -> u32 {
        // SAFETY: a read-only register
        unsafe { &*pac::SIO::ptr() }.cpuid().read().bits()
}

/// How a panic ends once its message is out (or given up on).
fn panic_finish() -> ! {
        #[cfg(feature = "usb-console")]
        unsafe {
                light_shell_reset_to_bootsel()
        }
        #[cfg(not(feature = "usb-console"))]
        {
                #[cfg(target_arch = "arm")]
                cortex_m::asm::bkpt();
                loop {
                        core::hint::spin_loop();
                }
        }
}

/// Hand a formatted panic message to the relay and never return.
pub fn relay_panic(msg: &[u8]) -> ! {
        critical_section::with(|cs| {
                let mut m = PANIC_MESSAGE.borrow_ref_mut(cs);
                *m = StackString::new();
                let _ = m.write_str(core::str::from_utf8(msg).unwrap_or("(panic message not utf-8)"));
        });
        if core_id() == 1 {
                panic_finish()
        }
        PANIC_PENDING.store(true, Ordering::Release);
        // busy-wait, never sleep: a panic raised inside an interrupt handler must not re-enter
        // the SDK's "sleep in exception handler" panic
        let start = crate::now_us();
        while !PANIC_PRINTED.load(Ordering::Acquire) && crate::now_us().wrapping_sub(start) < PANIC_RELAY_TIMEOUT_US {
                core::hint::spin_loop();
        }
        panic_finish()
}

/// The SDK's own panics, formatted by the C shell's `PICO_PANIC_FUNCTION` hook, land here.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_panic(msg: *const u8, len: usize) -> ! {
        // SAFETY: the shell hands a formatted message it owns for the duration
        let msg = unsafe { core::slice::from_raw_parts(msg, len) };
        relay_panic(msg)
}

/// Format a panic and hand it to the relay; never returns. Each app's `#[panic_handler]` calls this.
pub fn panic_report(info: &core::panic::PanicInfo) -> ! {
        let mut msg = StackString::<160>::new();
        let _ = write!(msg, "{info}");
        relay_panic(msg.as_bytes())
}

// --- the loop -------------------------------------------------------------------------------

/// The console's transports, driven together each pass.
struct Transports<'a> {
        uart: Option<Uart>,
        #[cfg(feature = "usb-console")]
        serial: usbd_serial::SerialPort<'a, crate::usb::UsbBus, [u8; 128], [u8; 512]>,
        #[cfg(feature = "usb-console")]
        dev: usb_device::device::UsbDevice<'a, crate::usb::UsbBus>,
        #[cfg(not(feature = "usb-console"))]
        _lt: core::marker::PhantomData<&'a ()>,
}

impl Transports<'_> {
        /// Advance the USB device stack (a no-op without one).
        fn poll(&mut self) {
                //   FIRST, and unconditionally: this is what moves the wire along, and the panic
                // relay's last act is a loop of nothing but this
                if let Some(u) = self.uart.as_mut() {
                        u.service();
                }
                #[cfg(feature = "usb-console")]
                {
                        self.dev.poll(&mut [&mut self.serial]);
                        USB_MOUNTED.store(self.dev.state() == usb_device::device::UsbDeviceState::Configured, Ordering::Relaxed);
                        // the flash script's request for the bootloader
                        if self.serial.line_coding().data_rate() == RESET_BAUD {
                                unsafe { light_shell_reset_to_bootsel() }
                        }
                }
        }

        /// Whether anything is listening: a UART always might be; a USB host once it has raised
        /// DTR on the port.
        fn listening(&self) -> bool {
                #[cfg(feature = "usb-console")]
                if self.serial.dtr() {
                        return true;
                }
                self.uart.is_some()
        }

        /// A line to every transport. One that has no room drops it -- never waited for.
        fn write(&mut self, bytes: &[u8]) {
                if let Some(u) = self.uart.as_mut() {
                        let _ = u.write(bytes);
                }
                #[cfg(feature = "usb-console")]
                {
                        let _ = self.serial.write(bytes);
                }
        }

        /// Input from any transport, to `push`.
        fn read_into(&mut self, push: &mut impl FnMut(u8)) {
                if let Some(u) = self.uart.as_mut() {
                        for _ in 0..32 {
                                match u.read() {
                                        Some(b) => push(b),
                                        None => break,
                                }
                        }
                }
                #[cfg(feature = "usb-console")]
                {
                        let mut buf = [0u8; 32];
                        if let Ok(n) = self.serial.read(&mut buf) {
                                for &b in &buf[..n] {
                                        push(b);
                                }
                        }
                }
        }
}

/// Core 1's whole life, the body of a board's `light_app_core1_main`: bring the console up, then
/// loop forever -- poll the USB device, drain the log to every transport, pump input to `push`,
/// and relay any panic. `uart` is the board's console UART, if it wires one (see [`UART_TX`]).
/// Never returns.
pub fn core1_main(mut push: impl FnMut(u8), uart: Option<Uart>) -> ! {
        #[cfg(feature = "usb-console")]
        let mut t = {
                use light_core::{usb, StaticCell};
                use usb_device::bus::UsbBusAllocator;
                use usb_device::device::{StringDescriptors, UsbDeviceBuilder, UsbVidPid};
                use usbd_serial::{SerialPort, USB_CLASS_CDC};

                // the allocator outlives the device and class built on it, and is the biggest
                // piece, so it lives in .bss rather than on this core's 4 KB stack
                static ALLOC: StaticCell<UsbBusAllocator<crate::usb::UsbBus>> = StaticCell::new();
                // SAFETY: core 1 is the one owner of the controller, constructed once here
                let alloc: &'static UsbBusAllocator<crate::usb::UsbBus> = ALLOC.init(UsbBusAllocator::new(unsafe { crate::usb::UsbBus::new() }));
                //   a 512-byte write store: a burst of log lines while the host is not reading is
                // absorbed here, and what does not fit is dropped -- never waited for
                let serial = SerialPort::new_with_store(alloc, [0u8; 128], [0u8; 512]);
                let dev = UsbDeviceBuilder::new(alloc, UsbVidPid(usb::VID, usb::PID))
                        .strings(&[StringDescriptors::default().manufacturer(usb::MANUFACTURER).product(usb::PRODUCT).serial_number("light-rp2")])
                        .expect("descriptor strings")
                        .device_class(USB_CLASS_CDC)
                        .build();
                Transports { uart, serial, dev }
        };
        #[cfg(not(feature = "usb-console"))]
        let mut t = Transports { uart, _lt: core::marker::PhantomData };

        //   the stack headroom, once the boot's deepest paths have run and someone is listening
        // (a line before that is dropped with the rest of the boot's): the number that sizes the
        // shell's array, and the one thing this core cannot report about itself after the fact
        const HEADROOM_REPORT_US: u64 = 3_000_000;
        let mut headroom_reported = false;
        loop {
                CORE1_TICKS.fetch_add(1, Ordering::Relaxed);
                t.poll();
                if !headroom_reported && crate::now_us() >= HEADROOM_REPORT_US && t.listening() {
                        headroom_reported = true;
                        let (free, total) = core1_stack_headroom();
                        light_core::info!("core 1: {} of {} stack bytes never touched", free, total);
                }
                // the log drain: a line a transport cannot take is dropped, never waited for
                log::drain(4, |record| {
                        let mut line = StackString::<160>::new();
                        let _ = write!(line, "{record}\r\n");
                        t.write(line.as_bytes());
                });
                t.read_into(&mut push);
                if PANIC_PENDING.load(Ordering::Acquire) && !PANIC_PRINTED.load(Ordering::Relaxed) {
                        let mut line = StackString::<300>::new();
                        critical_section::with(|cs| {
                                let m = PANIC_MESSAGE.borrow_ref(cs);
                                let _ = write!(line, "\r\n*** PANIC (core 0) ***\r\n{}\r\n", core::str::from_utf8(m.as_bytes()).unwrap_or(""));
                        });
                        t.write(line.as_bytes());
                        // keep the transports serviced so the message actually leaves
                        let start = crate::now_us();
                        while crate::now_us().wrapping_sub(start) < 1_000_000 {
                                t.poll();
                        }
                        PANIC_PRINTED.store(true, Ordering::Release);
                        loop {
                                t.poll();
                        }
                }
        }
}

// --- what the boot ROM did ------------------------------------------------------------------

unsafe extern "C" {
        fn light_shell_boot_info(out_diagnostic: *mut u32, out_params: *mut u32) -> u32;
        fn light_shell_reboot_diagnosing(partition: u32) -> i32;
}

/// The boot the firmware is running in, as the ROM describes it. On a chip that chooses between
/// image slots this is the only answer to "which image am I?", and when the ROM REFUSED an image
/// it is the only account of why: `diagnostic` is its verdict on the partition it was asked
/// about ([`reboot_diagnosing`]). `None` where the chip's ROM offers no such facility.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BootInfo {
        /// How this boot was entered (the ROM's boot-type code).
        pub boot_type: u8,
        /// The partition booted from, or `None` when the image was not in one.
        pub partition: Option<u8>,
        /// Try-before-you-buy and flash-update flags: a nonzero value means this image is still
        /// on probation and will be discarded unless it commits itself.
        pub tbyb_and_update: u8,
        /// The partition the ROM was asked to diagnose, or `None`.
        pub diagnostic_partition: Option<u8>,
        /// The ROM's verdict on that partition.
        pub diagnostic: u32,
}

pub fn boot_info() -> Option<BootInfo> {
        let mut diagnostic = 0u32;
        let mut params = [0u32; 2];
        // SAFETY: the shell's bootrom call, which writes only the two outputs
        let word = unsafe { light_shell_boot_info(&mut diagnostic, params.as_mut_ptr()) };
        if word == 0 {
                return None;
        }
        let bytes = word.to_le_bytes();
        let signed = |b: u8| (b as i8 >= 0).then_some(b);
        Some(BootInfo {
                diagnostic_partition: signed(bytes[0]),
                boot_type: bytes[1],
                partition: signed(bytes[2]),
                tbyb_and_update: bytes[3],
                diagnostic,
        })
}

/// Reboot, asking the ROM to report on `partition` in the boot that follows: the diagnostic word
/// describes whatever partition it was asked about, and the asking happens at the reboot rather
/// than afterwards. Returns only on failure, with the ROM's error.
pub fn reboot_diagnosing(partition: u8) -> i32 {
        // SAFETY: the shell's bootrom call; it does not return when it succeeds
        unsafe { light_shell_reboot_diagnosing(u32::from(partition)) }
}
