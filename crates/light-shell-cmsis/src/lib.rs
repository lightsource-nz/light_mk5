//! The Rust side of the bare-CMSIS shell ABI: the small helpers every STM32/CMSIS firmware layers
//! on the C shell's `light_shell_*` symbols. The CMSIS shell (`light_shell_cmsis`) hands the
//! Rust staticlib its resolved clocks, takes its log lines and console input, and takes its panic
//! message -- and that handshake is the same for every board on the bare-CMSIS shell, and
//! chip-independent, so it lives here rather than in each board's crate. (The RP2 shell's analogue
//! is `light_rp2::shell`; it differs because that port is multicore and pumps on core 1.) A board's
//! instantiation crate keeps only the thin `#[no_mangle] light_app_main` entry and `#[panic_handler]`
//! wrapper that call in, and drives [`drain_log`]/[`read_console`] from its console module each poll.

#![no_std]

use core::fmt::Write;

use light_core::log;

unsafe extern "C" {
        fn light_shell_panic(msg: *const u8, len: usize) -> !;
        fn light_shell_log(msg: *const u8, len: usize);
        fn light_shell_read_byte() -> i32;
}

/// What the C shell hands `light_app_main`: the resolved clock rates. One struct for every
/// bare-CMSIS board (the C shell defines it once); a chip reads the fields its clock tree needs.
#[repr(C)]
pub struct ShellInfo {
        pub clk_sys_hz: u32,
        pub clk_apb2_hz: u32,
        pub clk_tim_hz: u32,
}

/// A fixed-capacity `fmt::Write` sink on the stack -- for formatting a log line or a panic message
/// without an allocator. Writes past `N` are dropped.
struct StackString<const N: usize> {
        buf: [u8; N],
        len: usize,
}

impl<const N: usize> StackString<N> {
        fn new() -> Self {
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

fn log_line(record: &log::Record) {
        let mut line = StackString::<160>::new();
        let _ = write!(line, "{record}");
        let b = line.as_bytes();
        unsafe { light_shell_log(b.as_ptr(), b.len()) }
}

/// Drain up to `max` queued log lines to the shell; returns how many were written. On the single
/// core there is no core-1 pump, so a board calls this from its console module each poll (and again
/// with a larger bound at shutdown, to flush the last words before going quiet).
pub fn drain_log(max: usize) -> usize {
        log::drain(max, log_line)
}

/// Read up to 32 console bytes from the shell, handing each to `push`. Non-blocking: it stops at the
/// first byte the shell does not have, so it never stalls the poll loop.
pub fn read_console(mut push: impl FnMut(u8)) {
        for _ in 0..32 {
                let b = unsafe { light_shell_read_byte() };
                if b < 0 {
                        break;
                }
                push(b as u8);
        }
}

/// Format a panic and hand it to the shell; never returns. Each board's `#[panic_handler]` calls this.
pub fn panic_report(info: &core::panic::PanicInfo) -> ! {
        let mut msg = StackString::<160>::new();
        let _ = write!(msg, "{info}");
        let b = msg.as_bytes();
        unsafe { light_shell_panic(b.as_ptr(), b.len()) }
}
