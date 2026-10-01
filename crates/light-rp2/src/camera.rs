//! A parallel (DVP) camera port on PIO2: an image sensor's 8 data lines, VSYNC, HREF and PCLK
//! in, whole frames out into memory by DMA.
//!
//! One state machine, armed per frame. Each frame starts with a `pull` of the frame's byte
//! count, so the machine re-aligns on every frame's VSYNC edge: a capture that lost a byte (or
//! was stopped half way) can never shift the next frame. The reference design synchronised
//! once at start and then counted bytes across frames forever, so a single missed PCLK would
//! roll every later image.
//!
//! ```text
//!     pull block          ; the frame's byte count - 1
//!     mov x, osr
//!     wait 0 gpio VSYNC   ; the next frame's start: a VSYNC rising edge
//!     wait 1 gpio VSYNC
//! byte:
//!     wait 1 gpio HREF    ; only bytes inside a line; the blanking between lines stalls here
//!     wait 1 gpio PCLK    ; data is stable from the rising edge
//!     in pins, 8
//!     wait 0 gpio PCLK
//!     jmp x--, byte
//!                         ; wrap: back to the pull, parked until the next frame is armed
//! ```
//!
//! Bytes shift in from the RIGHT and autopush every four, so each word lands in memory with
//! its bytes in arrival order -- the sensor's big-endian RGB565 comes out exactly as a display
//! bus sends it, with no swap anywhere.
//!
//! Timing: the loop is five instructions per byte, plus the two-cycle input synchroniser on each
//! wait. At a 200 MHz system clock that keeps up with a PCLK past 25 MHz.

use crate::gpio;
use crate::pac;
use light_core::hal::CameraBus;

const SM: usize = 0;
const ORIGIN: u8 = 0;
const PROGRAM_LEN: u8 = 9;
/// DREQ for PIO2's RX FIFOs starts at 20 (TX at 16).
const DREQ_PIO2_RX0: u8 = 20;

/// `wait` on an absolute GPIO (source 0). `pol` 1 waits for high.
fn wait_gpio(pol: u16, pin: usize) -> u16 {
        0x2000 | pol << 7 | pin as u16
}
const PULL_BLOCK: u16 = 0x80A0;
const MOV_X_OSR: u16 = 0xA027;
const IN_PINS_8: u16 = 0x4008;
const JMP_XDEC: u16 = 0x0040;

pub struct CameraPins {
        /// First of 8 contiguous data pins, D0 lowest.
        pub data0: usize,
        pub vsync: usize,
        pub href: usize,
        pub pclk: usize,
}

pub struct PioCamera {
        pins: CameraPins,
        dma_ch: usize,
        armed: bool,
        /// Frames that have landed since construction.
        pub frames: u32,
}

impl PioCamera {
        /// Claims PIO2 and DMA channel `dma_ch`, and routes the camera's pins to the block. Nothing
        /// is captured until a frame is armed.
        ///
        /// # Safety
        ///
        /// Construct once; nothing else may use PIO2, `dma_ch` or the pins.
        pub unsafe fn new(pins: CameraPins, dma_ch: usize) -> Self {
                crate::reset_cycle(false, |w| w, |w| w.pio2().clear_bit(), |r| r.pio2().bit_is_set());
                let pio = unsafe { &*pac::PIO2::ptr() };
                let program: [u16; PROGRAM_LEN as usize] = [
                        PULL_BLOCK,
                        MOV_X_OSR,
                        wait_gpio(0, pins.vsync),
                        wait_gpio(1, pins.vsync),
                        wait_gpio(1, pins.href),
                        wait_gpio(1, pins.pclk),
                        IN_PINS_8,
                        wait_gpio(0, pins.pclk),
                        JMP_XDEC | (u16::from(ORIGIN) + 4),
                ];
                for (i, ins) in program.iter().enumerate() {
                        pio.instr_mem(usize::from(ORIGIN) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }
                let smr = pio.sm(SM);
                smr.sm_pinctrl().write(|w| unsafe { w.in_base().bits(pins.data0 as u8) });
                smr.sm_execctrl().write(|w| unsafe { w.wrap_bottom().bits(ORIGIN).wrap_top().bits(ORIGIN + PROGRAM_LEN - 1) });
                //   shift right, autopush at 32 (threshold 0 means 32): four bytes per word, the
                // first in the lowest byte. The FIFOs stay UNJOINED: joining RX takes the TX
                // FIFO's storage, and the per-frame byte count arrives through TX -- joined, that
                // write is silently dropped and the machine waits at its pull forever. Four words
                // of RX is ample: a word lands every ~36 system cycles at a 22 MHz PCLK
                smr.sm_shiftctrl().write(|w| unsafe { w.in_shiftdir().set_bit().autopush().set_bit().push_thresh().bits(0) });
                smr.sm_clkdiv().write(|w| unsafe { w.int().bits(1).frac().bits(0) });
                //   every pin an input to the block: set_function enables the pad input and clears
                // its isolation, and pin directions are inputs after the reset above
                for i in 0..8 {
                        gpio::set_function(pins.data0 + i, gpio::FUNC_PIO2);
                }
                for pin in [pins.vsync, pins.href, pins.pclk] {
                        gpio::set_function(pin, gpio::FUNC_PIO2);
                }
                smr.sm_instr().write(|w| unsafe { w.bits(u32::from(ORIGIN)) });
                Self { pins, dma_ch, armed: false, frames: 0 }
        }

        /// The machine disabled, restarted at its origin with both FIFOs emptied, and the channel
        /// aborted. Leaves the block parked.
        fn park(&mut self) {
                let pio = unsafe { &*pac::PIO2::ptr() };
                let dma = unsafe { &*pac::DMA::ptr() };
                pio.ctrl().modify(|r, w| unsafe { w.sm_enable().bits(r.sm_enable().bits() & !(1 << SM)) });
                //   EN off before the abort, and the wait bounded: a channel stalled on a DREQ that
                // will now never come can hold the abort bit up
                dma.ch(self.dma_ch).ch_al1_ctrl().modify(|_, w| w.en().clear_bit());
                dma.chan_abort().write(|w| unsafe { w.bits(1 << self.dma_ch) });
                for _ in 0..1_000_000u32 {
                        if dma.chan_abort().read().bits() == 0 {
                                break;
                        }
                }
                //   changing FJOIN discards both FIFOs' contents: a stale count in TX or stale
                // bytes in RX would otherwise start the next frame wrong
                let smr = pio.sm(SM);
                smr.sm_shiftctrl().modify(|_, w| w.fjoin_rx().set_bit());
                smr.sm_shiftctrl().modify(|_, w| w.fjoin_rx().clear_bit());
                //   a self-clearing strobe; the modify keeps the other machines' enables
                pio.ctrl().modify(|_, w| unsafe { w.sm_restart().bits(1 << SM) });
                smr.sm_instr().write(|w| unsafe { w.bits(u32::from(ORIGIN)) });
                self.armed = false;
        }

        /// Capture the next frame's first `out.len()` words by reading the RX FIFO from the CPU,
        /// with no DMA involved: splits "the machine reads nothing" from "the DMA lands nothing".
        /// Returns how many words arrived before the deadline. Blocking; bring-up only.
        pub fn capture_raw(&mut self, out: &mut [u32], timeout_us: u64, now: fn() -> u64) -> usize {
                self.park();
                let pio = unsafe { &*pac::PIO2::ptr() };
                pio.txf(SM).write(|w| unsafe { w.bits((out.len() * 4) as u32 - 1) });
                pio.ctrl().modify(|r, w| unsafe { w.sm_enable().bits(r.sm_enable().bits() | (1 << SM)) });
                let start = now();
                let mut n = 0;
                while n < out.len() && now() - start < timeout_us {
                        if pio.fstat().read().rxempty().bits() & (1 << SM) == 0 {
                                out[n] = pio.rxf(SM).read().bits();
                                n += 1;
                        }
                }
                self.park();
                n
        }

        /// The sync lines and PCLK sampled `n` times each, counting highs, with the machine's
        /// program counter and the bytes the armed frame still waits for. A bring-up bisect: a
        /// live sensor shows all three lines toggling; a capture that never lands shows where the
        /// machine is parked.
        pub fn probe(&self, n: u32) -> CameraProbe {
                let sio = unsafe { &*pac::SIO::ptr() };
                let mut highs = [0u32; 3];
                let (mut data_or, mut data_and) = (0u8, 0xFFu8);
                for _ in 0..n {
                        let v = sio.gpio_in().read().bits();
                        for (i, pin) in [self.pins.vsync, self.pins.href, self.pins.pclk].iter().enumerate() {
                                if v & (1 << pin) != 0 {
                                        highs[i] += 1;
                                }
                        }
                        if v & (1 << self.pins.href) != 0 {
                                let d = (v >> self.pins.data0) as u8;
                                data_or |= d;
                                data_and &= d;
                        }
                }
                let pio = unsafe { &*pac::PIO2::ptr() };
                let dma = unsafe { &*pac::DMA::ptr() };
                CameraProbe {
                        samples: n,
                        vsync_highs: highs[0],
                        href_highs: highs[1],
                        pclk_highs: highs[2],
                        data_or,
                        data_and,
                        pc: pio.sm(SM).sm_addr().read().bits() as u8,
                        words_left: dma.ch(self.dma_ch).ch_trans_count().read().bits() & 0x0FFF_FFFF,
                        dma_ctrl: dma.ch(self.dma_ch).ch_ctrl_trig().read().bits(),
                        dma_write_addr: dma.ch(self.dma_ch).ch_write_addr().read().bits(),
                        dma_abort: dma.chan_abort().read().bits(),
                        armed: self.armed,
                }
        }
}

/// What [`PioCamera::probe`] saw.
#[derive(Clone, Copy, Debug)]
pub struct CameraProbe {
        pub samples: u32,
        pub vsync_highs: u32,
        pub href_highs: u32,
        pub pclk_highs: u32,
        /// The data lines OR-ed and AND-ed across every sample taken inside a line: a bit set in
        /// the OR and clear in the AND toggled; a bit clear in the OR never went high.
        pub data_or: u8,
        pub data_and: u8,
        pub pc: u8,
        pub words_left: u32,
        /// The channel's control register: bit 31 a bus error, 30 a read error, 29 a write error,
        /// 26 busy.
        pub dma_ctrl: u32,
        pub dma_write_addr: u32,
        /// The DMA's abort register: nonzero means an abort has not completed.
        pub dma_abort: u32,
        pub armed: bool,
}

impl CameraBus for PioCamera {
        unsafe fn start_frame(&mut self, buf: &mut [u8]) {
                //   word transfers: a misaligned buffer is not an error the DMA reports -- the
                // channel runs, busy and advancing, and the buffer stays exactly as it was
                assert!(buf.as_ptr() as usize % 4 == 0, "a camera frame buffer must be word-aligned");
                debug_assert!(buf.len() % 4 == 0 && !buf.is_empty());
                self.park();
                let pio = unsafe { &*pac::PIO2::ptr() };
                let dma = unsafe { &*pac::DMA::ptr() };
                let c = dma.ch(self.dma_ch);
                c.ch_read_addr().write(|w| unsafe { w.bits(pio.rxf(SM).as_ptr() as u32) });
                c.ch_write_addr().write(|w| unsafe { w.bits(buf.as_mut_ptr() as u32) });
                c.ch_trans_count().write(|w| unsafe { w.bits((buf.len() / 4) as u32) });
                c.ch_al1_ctrl().write(|w| unsafe {
                        w.data_size()
                                .size_word()
                                .incr_read()
                                .clear_bit()
                                .incr_write()
                                .set_bit()
                                .treq_sel()
                                .bits(DREQ_PIO2_RX0 + SM as u8)
                                .chain_to()
                                .bits(self.dma_ch as u8)
                                .en()
                                .set_bit()
                });
                dma.multi_chan_trigger().write(|w| unsafe { w.bits(1 << self.dma_ch) });
                pio.txf(SM).write(|w| unsafe { w.bits(buf.len() as u32 - 1) });
                pio.ctrl().modify(|r, w| unsafe { w.sm_enable().bits(r.sm_enable().bits() | (1 << SM)) });
                self.armed = true;
        }

        fn frame_complete(&mut self) -> bool {
                if !self.armed {
                        return true;
                }
                let dma = unsafe { &*pac::DMA::ptr() };
                if dma.ch(self.dma_ch).ch_ctrl_trig().read().busy().bit_is_set() {
                        return false;
                }
                self.armed = false;
                self.frames = self.frames.wrapping_add(1);
                true
        }

        fn stop(&mut self) {
                if self.armed {
                        self.park();
                }
        }
}
