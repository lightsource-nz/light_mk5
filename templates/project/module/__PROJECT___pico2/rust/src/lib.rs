//! __PROJECT__ on a Raspberry Pi Pico 2: the application crate (`light_app___PROJECT__`), with
//! this board's pins, clock and console around it.
//!
//! The C shell (the framework's `module/light_shell`) brings the platform up, launches core 1
//! into the port's console loop through `light_app_core1_main`, and calls `light_app_main` on
//! core 0 with the clocks it configured. Neither returns.

#![no_std]

mod board;

use light_app___PROJECT__ as app;
use light_rp2::shell::{panic_report, ShellInfo, UART_BAUD, UART_RX, UART_TX};
use light_rp2::uart::Uart;
use light_rp2::{now_us, Breathe, SysClock};

/// Core 1: the port's console loop owns the whole core -- the USB serial console, the UART, the
/// log drain, and the bytes typed into the console, fed to the application one at a time.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_main(info: &ShellInfo) -> ! {
        // SAFETY: core 1's one construction of the console UART
        let uart = unsafe { Uart::new(UART_TX, UART_RX, UART_BAUD, info.clk_peri_hz) };
        light_rp2::shell::core1_main(app::push_console_byte, Some(uart))
}

/// Core 0: take the board's parts and run the application on them, forever.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        light_core::log::set_clock(now_us);
        light_core::info!("clocks: sys {} Hz, peri {} Hz", info.clk_sys_hz, info.clk_peri_hz);
        let p = board::take().expect("the board's peripherals are taken once");
        app::run(p.led, p.button, SysClock, Breathe)
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        panic_report(info)
}
