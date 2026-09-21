//! The USB OTG_FS controller as a `usb_device` bus. The controller driver is the ecosystem's
//! (`synopsys-usb-otg`, which knows this core's revisions and their quirks); this module is the
//! seam it drives -- where the core sits, how it is clocked and reset, which pins carry it -- and
//! the one-time construction of the bus, whose endpoint memory lives in `.bss`.
//!
//! The 48 MHz clock the core needs is the shell's: the PLL's Q output, set up with the rest of the
//! clock tree before Rust runs.

use light_core::StaticCell;
use synopsys_usb_otg::UsbPeripheral;
use usb_device::bus::UsbBusAllocator;

use crate::gpio::Pin;
use crate::reg;

/// OTG_FS on AHB2.
const OTG_FS_BASE: usize = 0x5000_0000;
const AHB2_OTGFS: u32 = 1 << 7;
const AF_OTG_FS: u32 = 10;

pub const PIN_DM: Pin = Pin::new('A', 11);
pub const PIN_DP: Pin = Pin::new('A', 12);

/// The controller, as the driver sees it.
pub struct OtgFs {
        ahb_hz: u32,
}

unsafe impl UsbPeripheral for OtgFs {
        const REGISTERS: *const () = OTG_FS_BASE as *const ();
        const HIGH_SPEED: bool = false;
        /// The core's dedicated FIFO RAM: 1.25 KB.
        const FIFO_DEPTH_WORDS: usize = 320;
        const ENDPOINT_COUNT: usize = 4;

        fn enable() {
                //   the bus clock, then a reset pulse so the core starts from its defaults
                // whatever a previous firmware left in it
                reg::modify(crate::RCC_AHB2ENR, 0, AHB2_OTGFS);
                let _ = reg::read(crate::RCC_AHB2ENR);
                reg::modify(crate::RCC_AHB2RSTR, 0, AHB2_OTGFS);
                reg::modify(crate::RCC_AHB2RSTR, AHB2_OTGFS, 0);
        }

        fn ahb_frequency_hz(&self) -> u32 {
                self.ahb_hz
        }
}

pub type UsbBus = synopsys_usb_otg::UsbBus<OtgFs>;

/// The driver's endpoint memory: the OUT buffers it copies packets into from the receive FIFO.
/// A control endpoint and a CDC set need well under this.
static EP_MEMORY: StaticCell<[u32; 128]> = StaticCell::new();
static ALLOC: StaticCell<UsbBusAllocator<UsbBus>> = StaticCell::new();

/// Bring the controller up and hand back the allocator the class layer builds on. Once.
///
/// # Safety
/// Takes the OTG_FS block and PA11/PA12; nothing else may touch them afterwards.
pub unsafe fn init(ahb_hz: u32) -> &'static UsbBusAllocator<UsbBus> {
        PIN_DM.set_alternate(AF_OTG_FS);
        PIN_DP.set_alternate(AF_OTG_FS);
        let memory: &'static mut [u32] = EP_MEMORY.init([0; 128]);
        ALLOC.init(UsbBus::new(OtgFs { ahb_hz }, memory))
}
