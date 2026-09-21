//! The USB OTG controller as a `usb_device` bus: the chip's second instance (OTG_FS, on the
//! PA11/PA12 pins), which has the internal full-speed PHY and nothing else. The controller
//! driver is the ecosystem's (`synopsys-usb-otg`, in the register layout both of this chip's
//! instances share); this module is the seam it drives -- where the core sits, how it is clocked,
//! reset and powered, which pins carry it -- and the one-time construction of the bus, whose
//! endpoint memory lives in `.bss`.
//!
//! The 48 MHz kernel clock is the shell's: PLL3's Q output, selected for USB with the rest of the
//! clock tree before Rust runs.

use light_core::StaticCell;
use synopsys_usb_otg::UsbPeripheral;
use usb_device::bus::UsbBusAllocator;

use crate::gpio::Pin;
use crate::reg;

/// OTG_FS (the second instance) on AHB1.
const OTG_FS_BASE: usize = 0x4008_0000;
const AHB1_USB2OTG: u32 = 1 << 27;
/// The ULPI clock of the same instance: for an external PHY only. Left enabled in the
/// low-power register it hangs the core in sleep with the internal PHY, so it is cleared.
const AHB1LP_USB2OTG_ULPI: u32 = 1 << 28;
const AF_OTG_FS: u32 = 10;

/// The USB supply's voltage detector: without it the PHY is unpowered and a fully configured
/// controller never enumerates.
const PWR_CR3: usize = 0x5802_4800 + 0x0C;
const PWR_CR3_USB33DEN: u32 = 1 << 24;
const PWR_CR3_USB33RDY: u32 = 1 << 26;

pub const PIN_DM: Pin = Pin::new('A', 11);
pub const PIN_DP: Pin = Pin::new('A', 12);

/// The controller, as the driver sees it.
pub struct OtgFs {
        ahb_hz: u32,
}

unsafe impl UsbPeripheral for OtgFs {
        const REGISTERS: *const () = OTG_FS_BASE as *const ();
        /// The high-speed-layout core: the driver's speed choice then follows the PHY, which is
        /// the internal full-speed one (its default `phy_type`).
        const HIGH_SPEED: bool = true;
        /// The core's dedicated FIFO RAM: 4 KB.
        const FIFO_DEPTH_WORDS: usize = 1024;
        const ENDPOINT_COUNT: usize = 9;

        fn enable() {
                reg::modify(PWR_CR3, 0, PWR_CR3_USB33DEN);
                let mut spins = 1_000_000u32;
                while reg::read(PWR_CR3) & PWR_CR3_USB33RDY == 0 && spins > 0 {
                        spins -= 1;
                }
                //   the bus clock, then a reset pulse so the core starts from its defaults
                // whatever a previous firmware left in it
                reg::modify(crate::RCC_AHB1ENR, 0, AHB1_USB2OTG);
                let _ = reg::read(crate::RCC_AHB1ENR);
                reg::modify(crate::RCC_AHB1LPENR, AHB1LP_USB2OTG_ULPI, 0);
                reg::modify(crate::RCC_AHB1RSTR, 0, AHB1_USB2OTG);
                reg::modify(crate::RCC_AHB1RSTR, AHB1_USB2OTG, 0);
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
