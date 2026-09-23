//! The USB HOST-ROLE PROBE: the smallest firmware that puts the native port in its host role and
//! says what it finds. It exists so the role is covered by a build in this repository -- the port
//! owns the host stack, and a seam with no consumer here is a seam that breaks quietly in a
//! product repository instead.
//!
//! There is no application: the runtime brings the stack up, polls it, and logs every mount,
//! unmount and enumeration failure to the console. The console is the UART, because the native
//! port is the host port.

#![no_std]

use light_core::{info, log, warn};
use light_midi::{Host, MidiEvent};
use light_rp2::shell::{panic_report, ShellInfo, UART_BAUD, UART_RX, UART_TX};
use light_rp2::uart::Uart;
use light_rp2::usb_host::UsbMidiHost;
use light_rp2::{now_us, Clocks};

/// Core 1: the console on the UART alone. The native port is the host port, so there is no
/// device stack to carry a console here.
#[unsafe(no_mangle)]
pub extern "C" fn light_app_core1_main(info: &ShellInfo) -> ! {
        // SAFETY: core 1's one construction of the console UART
        let uart = unsafe { Uart::new(UART_TX, UART_RX, UART_BAUD, info.clk_peri_hz) };
        light_rp2::shell::core1_main(|_| {}, Some(uart))
}

#[unsafe(no_mangle)]
pub extern "C" fn light_app_main(info: &ShellInfo) -> ! {
        log::set_clock(now_us);
        let clocks = Clocks { sys_hz: info.clk_sys_hz, peri_hz: info.clk_peri_hz };
        let _ = clocks;
        info!("usb host probe: sys {} Hz; host stack on core 0, console on the UART", info.clk_sys_hz);
        if let Some(b) = light_rp2::shell::boot_info() {
                info!("boot: type {}, partition {:?}, probation {:#x}; diagnosing {:?} -> {:#010x}", b.boot_type, b.partition, b.tbyb_and_update, b.diagnostic_partition, b.diagnostic);
        }
        let mut host = UsbMidiHost::init();
        info!("host stack up; waiting for instruments");

        let mut mounted = 0u32;
        loop {
                //   the stack is polled, not interrupt-driven: one pass per loop, exactly as an
                // application's runtime would drive it
                host.task();
                while let Some(event) = host.next_event() {
                        match event {
                                MidiEvent::Mounted { idx, mount, bus } => {
                                        mounted += 1;
                                        match bus {
                                                Some(b) => info!("mounted: slot {} (address {}, {} in / {} out cables) on hub {} port {}", idx, mount.daddr, mount.rx_cables, mount.tx_cables, b.hub_addr, b.hub_port),
                                                None => info!("mounted: slot {} (address {}, {} in / {} out cables), not behind a hub", idx, mount.daddr, mount.rx_cables, mount.tx_cables),
                                        }
                                }
                                MidiEvent::Unmounted { idx } => {
                                        mounted = mounted.saturating_sub(1);
                                        info!("unmounted: slot {}", idx);
                                }
                        }
                }
                let dropped = host.dropped_events();
                if dropped > 0 {
                        warn!("host events dropped: {}", dropped);
                }
                let _ = mounted;
        }
}

#[cfg(target_os = "none")]
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
        panic_report(info)
}
