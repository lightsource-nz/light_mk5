//! A full-speed USB bus built out of PIO: the wire-level half of the PIO host role. What a
//! USB controller does in silicon -- NRZI, bit stuffing, SYNC and EOP, the CRCs, the turnaround
//! timing -- is done here by three state machines on one PIO block, a DMA channel, and a little
//! processor time per packet. Nothing in this module knows what a transfer is; that is the
//! engine's business ([`crate::usb_host_pio`]). This module sends one packet and receives one
//! packet, and reports the state of the two wires.
//!
//! THE PROGRAMS ARE THE REFERENCE IMPLEMENTATION'S, hand-assembled here the way every PIO program
//! in this crate is (the C shell owns pioasm; this crate owns registers): the Pico-PIO-USB
//! project's `usb_tx_dpdm`, `usb_nrzi_decoder` and `usb_edge_detector` programs, copyright
//! 2021-2024 sekigon-gonnoc, MIT licence, taken from the copy the platform's SDK tree carries.
//! The packet-level procedure around them -- what to wait for, for how long, in what order --
//! follows that project's host, because every one of those waits is a timing the USB
//! specification sets and the reference has had measured against real devices. Where this
//! module departs from it, it says so.
//!
//! What is deliberately NOT here: low speed. The reference switches programs and clock dividers
//! mid-transaction to talk to a 1.5 Mbit/s device, directly or through a hub with a preamble.
//! The instruments this host exists for are a full-speed class, so this bus runs at 12 Mbit/s
//! only, and a low-speed device is reported as present and then left alone, which is the
//! honest outcome. Half the reference's speed-switching machinery goes with it.
//!
//! The three state machines fill the block's 32 instructions exactly:
//!
//! ```text
//! ; usb_tx_dpdm, at address 0 -- it MUST be at 0: the encoded data is a stream of two-bit
//! ; instruction addresses, each one an `out pc, 2` target. Side-set drives D+ and D-.
//! ; Runs at 48 MHz; each instruction with [3] is one bit time at 12 MHz.
//!  0: irq nowait 0    side SE0 [7]   ; EOP: two bit times of SE0, then the completion flag
//!  1: out pc, 2       side J   [3]   ; drive J for a bit, fetch the next symbol
//!  2: set pindirs, 0  side J   [3]   ; COMP: release the bus (pins to input)
//!  3: out pc, 2       side K   [3]   ; drive K for a bit, fetch the next symbol
//!  4: set pindirs, 3  side J         ; start: drive the bus, falls into 1
//!
//! ; usb_nrzi_decoder, at 5 -- runs at the system clock, triggered a bit at a time by the
//! ; edge detector's irq 4. x is the last line state, y counts ones for the stuff bit.
//! ; OSR is preloaded with all ones, so `in osr, 1` shifts in a 1 and `in null, 1` a 0.
//!  5: set y, 6
//!  6: wait 1 irq 4                   ; the detector says: sample now
//!  7: jmp !y 11                      ; six ones in a row: the next bit is a stuff bit, skip it
//!  8: jmp pin 12                     ; D+ high (inverted input, so: the line is K)
//!  9: jmp !x 13
//! 10: in null, 1                     ; a transition: a 0
//! 11: mov x, ~x                      ; remember the new line state
//! 12: jmp !x 10
//! 13: in osr, 1                      ; no transition: a 1
//! 14: jmp y-- 6
//!
//! ; usb_edge_detector, at 15 -- runs at 96 MHz, eight samples a bit; resynchronises the
//! ; decoder's sample point on every D+ edge, and raises irq 2 on an EOP (both lines low,
//! ; which with inverted inputs reads as both high) and irq 3 at the start of a packet.
//! 15: irq wait 2                     ; EOP seen: flag it and stop until re-armed
//! 16: jmp pin 16                     ; idle: wait for D- (jmp pin) to fall
//! 17: irq nowait 3 [1]               ; packet start
//! 18: irq nowait 4 [1]               ; trigger the decoder (pin still low)
//! 19..24: jmp pin 27                 ; six chances to see the rising edge, one per cycle
//! 25: mov x, isr [1]
//! 26: jmp x-- 15                     ; both inputs high: EOP
//! 27: mov isr, null [1]              ; rising edge: resync
//! 28: irq nowait 4                   ; trigger the decoder
//! 29: in pins, 1                     ; sample D+ for the EOP check
//! 30: jmp pin 25
//! 31: jmp 21
//! ```
//!
//! BOTH DATA PINS ARE INPUT-INVERTED at the pad, which is how the detector's `jmp pin` tests
//! find a low line with a "jump if high" instruction. Every raw line-state read in this module
//! un-inverts.

use crate::gpio;
use crate::pac;

/// The USB packet identifiers this host sends or expects, with their check nibbles.
pub const PID_OUT: u8 = 0xE1;
pub const PID_IN: u8 = 0x69;
pub const PID_SOF: u8 = 0xA5;
pub const PID_SETUP: u8 = 0x2D;
pub const PID_DATA0: u8 = 0xC3;
pub const PID_DATA1: u8 = 0x4B;
pub const PID_ACK: u8 = 0xD2;
pub const PID_NAK: u8 = 0x5A;
pub const PID_STALL: u8 = 0x1E;
/// The SYNC byte, as it goes down the wire LSB first.
pub const SYNC: u8 = 0x80;

/// The largest data packet this bus moves: a full-speed bulk or interrupt endpoint's maximum.
pub const MAX_PACKET: usize = 64;
/// Room for a data packet encoded for the transmitter: SYNC, PID, 64 bytes and the CRC, bit
/// stuffed at worst one in six, two bits a symbol, plus the EOP and completion symbols and the
/// padding to a whole byte.
pub const ENCODED_MAX: usize = (MAX_PACKET + 4) * 8 * 7 / 6 / 4 + 4;
/// What one received packet may occupy, SYNC and CRC included.
pub const RX_BUFFER: usize = 128;

/// The two-bit symbols the encoder emits: each is the address of the transmitter instruction
/// that produces it, and named for what that instruction puts ON THE WIRE -- instruction 1
/// side-sets D+ high, a J, and instruction 3 D- high, a K.
///
///   THE REFERENCE NAMES THESE THE OTHER WAY ROUND (its K is 1) and is right anyway, because its
/// encoder starts from a state it calls K as well; the two inversions cancel. Taking its names
/// with an encoder that starts from J sent every packet with the line inverted: SYNC went out as
/// JKJKJKJJ, whose first bit is no transition out of idle at all, and no device ever answered.
/// The test `the_symbols_are_what_the_transmitter_drives` holds these to the program.
const SYM_SE0: u8 = 0;
const SYM_J: u8 = 1;
const SYM_COMP: u8 = 2;
const SYM_K: u8 = 3;

/// The transmitter, `usb_tx_dpdm`: see the module's note. Its side-set is the top two bits of
/// the delay/side-set field, D+ in the low bit.
const TX_PROGRAM: [u16; 5] = [0xC700, 0x6BA2, 0xEB80, 0x73A2, 0xE883];

/// The transmitter's instruction for "start a packet": `jmp 4` with the side-set holding J.
/// Executed by hand before every packet; the program then wraps into its symbol loop.
const TX_START: u32 = 0x0804;
/// The decoder's reset: back to its first instruction, and `set x, 0`.
const RX_RESET: u32 = 0x0005;
const RX_RESET_X: u32 = 0xE020;
/// `mov osr, ~null`: the decoder's source of ones.
const RX_LOAD_OSR: u32 = 0xA0EB;

const SM_TX: usize = 0;
const SM_RX: usize = 1;
const SM_EOP: usize = 2;

const TX_ORIGIN: u16 = 0;
const RX_ORIGIN: u16 = 5;
const EOP_ORIGIN: u16 = 15;

/// The PIO's own interrupt flags the programs raise. Bit 0 is the transmitter's EOP-sent flag;
/// 2 is "EOP received", 3 "packet started", 4 the decoder trigger.
const IRQ_TX_EOP: u32 = 1 << 0;
const IRQ_RX_EOP: u32 = 1 << 2;
const IRQ_RX_START: u32 = 1 << 3;
const IRQ_RX_ALL: u32 = (1 << 1) | (1 << 2) | (1 << 3) | (1 << 4);

/// The state of the two wires, D- in bit 1 and D+ in bit 0, un-inverted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum LineState {
        /// Both low: no device, or a reset, or the end of a packet.
        Se0 = 0b00,
        /// D+ high: a full-speed device's idle.
        FullSpeedIdle = 0b01,
        /// D- high: a low-speed device's idle.
        LowSpeedIdle = 0b10,
        /// Both high: not a legal state.
        Se1 = 0b11,
}

/// The bus: its pins, its DMA channel, and the one receive buffer.
pub struct PioUsbBus {
        pin_dp: usize,
        pin_dm: usize,
        dma_ch: usize,
        /// Whether the chip revision needs the pad input re-enabled before a line read
        /// (the RP2350-E9 erratum: a disabled-then-enabled input drains a leaked level).
        /// A byte rather than a bool, so that an `Option` of this bus has no niche to put its
        /// `None` in and the engine's static stays all-zero, which is what keeps it in .bss.
        e9_workaround: u8,
        /// The packet most recently received, SYNC and PID first.
        pub rx: [u8; RX_BUFFER],
        /// Whether the most recent receive saw a packet start inside the turnaround, and how
        /// many bytes of it arrived. Bytes, not a bool, for the same reason as `e9_workaround`.
        pub rx_started: u8,
        pub rx_count: u8,
}

impl PioUsbBus {
        /// Claim PIO0 whole -- all three state machines and all 32 instructions -- the DMA channel
        /// and the two pins, and bring the bus up idle with nothing attached.
        ///
        /// `sys_hz` must be a multiple of 12 MHz: the transmitter runs at 48 MHz and the edge
        /// detector at 96 MHz from integer dividers, and a fractional divider puts jitter on every
        /// bit. 240 MHz is the clock the board is designed for; 120 and 192 also divide.
        ///
        /// # Safety
        ///
        /// Construct once; nothing else may use PIO0, the channel or the pins while this lives.
        pub unsafe fn new(pin_dp: usize, dma_ch: usize, sys_hz: u32) -> Self {
                //   D- is the next pin up: the transmitter side-sets the pair as a base and a
                // count, and the reference programs put D+ first
                let pin_dm = pin_dp + 1;
                let pio = unsafe { &*pac::PIO0::ptr() };
                crate::reset_cycle(false, |w| w, |w| w.pio0().clear_bit(), |r| r.pio0().bit_is_set());

                // --- the programs ---------------------------------------------------------
                let tx = TX_PROGRAM;
                let r =|target: u16| RX_ORIGIN + target;
                let rx: [u16; 10] = [0xE046, 0x20C4, jmp(0x0066, r(6)), jmp(0x00C7, r(7)), jmp(0x0028, r(8)), 0x4061, 0xA029, jmp(0x0025, r(5)), 0x40E1, jmp(0x0081, r(1))];
                let e = |target: u16| EOP_ORIGIN + target;
                let eop: [u16; 17] = [
                        0xC022,
                        jmp(0x00C1, e(1)),
                        0xC103,
                        0xC104,
                        jmp(0x00CC, e(12)),
                        jmp(0x00CC, e(12)),
                        jmp(0x00CC, e(12)),
                        jmp(0x00CC, e(12)),
                        jmp(0x00CC, e(12)),
                        jmp(0x00CC, e(12)),
                        0xA126,
                        jmp(0x0040, e(0)),
                        0xA1C3,
                        0xC004,
                        0x4001,
                        jmp(0x00CA, e(10)),
                        jmp(0x0006, e(6)),
                ];
                for (i, ins) in tx.iter().enumerate() {
                        pio.instr_mem(usize::from(TX_ORIGIN) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }
                for (i, ins) in rx.iter().enumerate() {
                        pio.instr_mem(usize::from(RX_ORIGIN) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }
                for (i, ins) in eop.iter().enumerate() {
                        pio.instr_mem(usize::from(EOP_ORIGIN) + i).write(|w| unsafe { w.bits(u32::from(*ins)) });
                }

                // --- the pins --------------------------------------------------------------
                //   both to the PIO, pulled down (the host's end of the bus idles low until a
                // device pulls one line up), fast slew and the strongest drive for a line with
                // a 27 ohm series resistor and a cable's worth of capacitance on it
                let pads = unsafe { &*pac::PADS_BANK0::ptr() };
                let io = unsafe { &*pac::IO_BANK0::ptr() };
                for pin in [pin_dp, pin_dm] {
                        gpio::set_function(pin, gpio::FUNC_PIO0);
                        pads.gpio(pin).modify(|_, w| w.pue().clear_bit().pde().set_bit().slewfast().set_bit().drive()._12m_a());
                        // INVERTED INPUTS: see the module's note
                        io.gpio(pin).gpio_ctrl().modify(|_, w| w.inover().invert());
                }

                // --- the transmitter: state machine 0 ----------------------------------------
                let sm = pio.sm(SM_TX);
                sm.sm_pinctrl().write(|w| unsafe { w.out_base().bits(pin_dp as u8).out_count().bits(2).set_base().bits(pin_dp as u8).set_count().bits(2).sideset_base().bits(pin_dp as u8).sideset_count().bits(2) });
                sm.sm_execctrl().write(|w| unsafe { w.wrap_bottom().bits(TX_ORIGIN as u8 + 1).wrap_top().bits(TX_ORIGIN as u8 + 4).side_en().clear_bit().side_pindir().clear_bit() });
                // shift left, autopull at 8 bits, the FIFO joined for depth
                sm.sm_shiftctrl().write(|w| unsafe { w.out_shiftdir().clear_bit().autopull().set_bit().pull_thresh().bits(8).fjoin_tx().set_bit() });
                set_clkdiv(sm, sys_hz, 48_000_000);
                //   pins start as inputs reading J: `set pins, 0b01` then `set pindirs, 0`
                sm.sm_instr().write(|w| unsafe { w.bits(0xE001) });
                sm.sm_instr().write(|w| unsafe { w.bits(0xE080) });
                sm.sm_instr().write(|w| unsafe { w.bits(u32::from(TX_ORIGIN) + 1) });

                // --- the decoder: state machine 1 ----------------------------------------------
                let sm = pio.sm(SM_RX);
                sm.sm_pinctrl().write(|w| unsafe { w.in_base().bits(pin_dp as u8) });
                sm.sm_execctrl().write(|w| unsafe { w.wrap_bottom().bits(RX_ORIGIN as u8).wrap_top().bits(RX_ORIGIN as u8 + 6).jmp_pin().bits(pin_dp as u8) });
                // shift right, autopush at 8 bits: each byte lands in the top of the FIFO word
                sm.sm_shiftctrl().write(|w| unsafe { w.in_shiftdir().set_bit().autopush().set_bit().push_thresh().bits(8).fjoin_rx().set_bit() });
                sm.sm_clkdiv().write(|w| unsafe { w.int().bits(1).frac().bits(0) });
                sm.sm_instr().write(|w| unsafe { w.bits(RX_LOAD_OSR) });
                sm.sm_instr().write(|w| unsafe { w.bits(RX_RESET) });

                // --- the edge detector: state machine 2 --------------------------------------
                let sm = pio.sm(SM_EOP);
                sm.sm_pinctrl().write(|w| unsafe { w.in_base().bits(pin_dp as u8) });
                sm.sm_execctrl().write(|w| unsafe { w.wrap_bottom().bits(EOP_ORIGIN as u8 + 3).wrap_top().bits(EOP_ORIGIN as u8 + 9).jmp_pin().bits(pin_dm as u8) });
                sm.sm_shiftctrl().write(|w| unsafe { w.in_shiftdir().clear_bit().autopush().clear_bit().push_thresh().bits(8) });
                set_clkdiv(sm, sys_hz, 96_000_000);
                sm.sm_instr().write(|w| unsafe { w.bits(u32::from(EOP_ORIGIN)) });

                // --- the DMA channel: bytes from memory into the transmitter's FIFO ------------
                let dma = unsafe { &*pac::DMA::ptr() };
                let ch = dma.ch(dma_ch);
                ch.ch_write_addr().write(|w| unsafe { w.bits(pio.txf(SM_TX).as_ptr() as u32) });
                // DREQ: PIO0's TX FIFOs are 0..3 on both chips
                ch.ch_al1_ctrl().write(|w| unsafe { w.data_size().size_byte().incr_read().set_bit().incr_write().clear_bit().treq_sel().bits(SM_TX as u8).chain_to().bits(dma_ch as u8).en().set_bit() });

                //   the transmitter and the edge detector run; the decoder is started per packet
                pio.ctrl().modify(|_, w| unsafe { w.sm_enable().bits((1 << SM_TX) | (1 << SM_EOP)) });

                let sysinfo = unsafe { &*pac::SYSINFO::ptr() };
                let e9_workaround = u8::from(sysinfo.chip_id().read().revision().bits() <= 2);

                Self { pin_dp, pin_dm, dma_ch, e9_workaround, rx: [0; RX_BUFFER], rx_started: 0, rx_count: 0 }
        }

        /// The state of the two wires right now.
        #[inline(always)]
        pub fn line_state(&self) -> LineState {
                let pads = unsafe { &*pac::PADS_BANK0::ptr() };
                if self.e9_workaround != 0 {
                        //   the erratum: a pad whose input is enabled can hold a leaked level;
                        // disabling and re-enabling the input drains it before the read
                        pads.gpio(self.pin_dp).modify(|_, w| w.ie().clear_bit());
                        pads.gpio(self.pin_dm).modify(|_, w| w.ie().clear_bit());
                        for _ in 0..8 {
                                core::hint::spin_loop();
                        }
                        pads.gpio(self.pin_dp).modify(|_, w| w.ie().set_bit());
                        pads.gpio(self.pin_dm).modify(|_, w| w.ie().set_bit());
                }
                let sio = unsafe { &*pac::SIO::ptr() };
                let raw = sio.gpio_in().read().bits();
                // the inputs are inverted at the pad: a set bit is a LOW line
                let dp = if raw & (1 << self.pin_dp) != 0 { 0 } else { 1 };
                let dm = if raw & (1 << self.pin_dm) != 0 { 0 } else { 2 };
                match dm | dp {
                        0b00 => LineState::Se0,
                        0b01 => LineState::FullSpeedIdle,
                        0b10 => LineState::LowSpeedIdle,
                        _ => LineState::Se1,
                }
        }

        /// Hold the bus in SE0 -- a reset, as the host signals it -- by overriding both pads
        /// below the PIO, or let it go again.
        pub fn force_se0(&self, on: bool) {
                let io = unsafe { &*pac::IO_BANK0::ptr() };
                for pin in [self.pin_dp, self.pin_dm] {
                        if on {
                                io.gpio(pin).gpio_ctrl().modify(|_, w| w.outover().low().oeover().enable());
                        } else {
                                io.gpio(pin).gpio_ctrl().modify(|_, w| w.outover().normal().oeover().normal());
                        }
                }
        }

        /// Send an encoded packet and wait until the bus is released after its EOP.
        #[unsafe(link_section = ".time_critical.light_usb_pio_send")]
        #[inline(never)]
        pub fn send(&self, encoded: &[u8]) {
                let pio = unsafe { &*pac::PIO0::ptr() };
                let dma = unsafe { &*pac::DMA::ptr() };
                pio.sm(SM_TX).sm_instr().write(|w| unsafe { w.bits(TX_START) });
                let ch = dma.ch(self.dma_ch);
                ch.ch_read_addr().write(|w| unsafe { w.bits(encoded.as_ptr() as u32) });
                ch.ch_al1_trans_count_trig().write(|w| unsafe { w.bits(encoded.len() as u32) });
                pio.irq().write(|w| unsafe { w.bits(IRQ_TX_EOP) });
                while pio.irq().read().bits() & IRQ_TX_EOP == 0 {}
                pio.irq().write(|w| unsafe { w.bits(IRQ_TX_EOP) });
                //   the EOP flag rises at the start of the SE0; the bus is ours until the
                // completion symbol has run the release instruction
                while pio.sm(SM_TX).sm_addr().read().bits() < u32::from(SYM_COMP) {}
        }

        /// A token packet: PID, address, endpoint, CRC5.
        #[unsafe(link_section = ".time_critical.light_usb_pio_token")]
        #[inline(never)]
        pub fn send_token(&self, pid: u8, address: u8, endpoint: u8) {
                let fields = (u16::from(endpoint & 0xF) << 7) | u16::from(address & 0x7F);
                let crc = crc5(fields);
                let packet = [SYNC, pid, (fields & 0xFF) as u8, (crc << 3) | ((fields >> 8) & 0x1F) as u8];
                let mut encoded = [0u8; 4 * 8 * 7 / 6 / 4 + 4];
                let n = encode(&packet, &mut encoded);
                self.send(&encoded[..n]);
        }

        /// Ready the decoder for a packet: FIFO cleared, program restarted -- and left STOPPED.
        /// It is started by [`Self::start_receive`], after this end's own packet has gone: running
        /// through a transmission it decodes that packet into its FIFO, and the reply is read
        /// from behind it.
        #[unsafe(link_section = ".time_critical.light_usb_pio_prepare")]
        #[inline(never)]
        pub fn prepare_receive(&self) {
                let pio = unsafe { &*pac::PIO0::ptr() };
                sm_enable(pio, SM_RX, false);
                // a FIFO is emptied by toggling its join bit, which the SDK's clear does too
                pio.sm(SM_RX).sm_shiftctrl().modify(|_, w| w.fjoin_rx().clear_bit().fjoin_tx().set_bit());
                pio.sm(SM_RX).sm_shiftctrl().modify(|_, w| w.fjoin_tx().clear_bit().fjoin_rx().set_bit());
                pio.ctrl().modify(|_, w| unsafe { w.sm_restart().bits(1 << SM_RX) });
                pio.sm(SM_RX).sm_instr().write(|w| unsafe { w.bits(RX_RESET) });
                pio.sm(SM_RX).sm_instr().write(|w| unsafe { w.bits(RX_RESET_X) });
        }

        /// Start the decoder and arm the receive flags: cleared, and confirmed clear, so the next
        /// packet's start is the first thing seen.
        #[inline(always)]
        pub fn start_receive(&self) {
                let pio = unsafe { &*pac::PIO0::ptr() };
                sm_enable(pio, SM_RX, true);
                pio.irq().write(|w| unsafe { w.bits(IRQ_RX_ALL) });
                while pio.irq().read().bits() & IRQ_RX_ALL != 0 {}
        }

        /// Stop the decoder after a transaction, and forget the PID it saw: the next
        /// transaction's answer is read from the same two bytes, and a stale one would pass for
        /// a reply that never came.
        #[inline(always)]
        pub fn end_receive(&mut self) {
                let pio = unsafe { &*pac::PIO0::ptr() };
                sm_enable(pio, SM_RX, false);
                self.rx[0] = 0;
                self.rx[1] = 0;
        }

        /// Wait for a packet to start, inside the turnaround the specification allows: 16 bit
        /// times, which is 1.33 us at full speed. Counted in whole microseconds, so three is
        /// the bound that is never short.
        #[inline(always)]
        fn wait_for_start(&self) -> bool {
                let pio = unsafe { &*pac::PIO0::ptr() };
                let start = micros();
                while micros().wrapping_sub(start) <= 3 {
                        if pio.irq().read().bits() & IRQ_RX_START != 0 {
                                return true;
                        }
                }
                false
        }

        /// Receive a handshake: the PID, or 0 for none, a malformed one, or a timeout.
        #[unsafe(link_section = ".time_critical.light_usb_pio_handshake")]
        #[inline(never)]
        pub fn wait_handshake(&mut self) -> u8 {
                self.rx_count = 0;
                self.rx_started = u8::from(self.wait_for_start());
                if self.rx_started == 0 {
                        return 0;
                }
                let pio = unsafe { &*pac::PIO0::ptr() };
                let mut idx = 0usize;
                //   seven microseconds of silence ends it: enough for a byte at low speed, and
                // the way a packet with no EOP -- a device unplugged mid-word -- is let go of
                let mut start = micros();
                while micros().wrapping_sub(start) <= 7 {
                        if idx < 2 && rx_level(pio) != 0 {
                                self.rx[idx] = (pio.rxf(SM_RX).read().bits() >> 24) as u8;
                                idx += 1;
                                start = micros();
                        } else if pio.irq().read().bits() & IRQ_RX_EOP != 0 {
                                break;
                        }
                }
                self.rx_count = idx as u8;
                if idx != 2 || self.rx[0] != SYNC {
                        return 0;
                }
                self.rx[1]
        }

        /// Receive a data packet into `rx`, check its CRC, and answer it with `handshake`
        /// (an ACK only if the CRC held). Returns the number of data bytes -- SYNC, PID and
        /// CRC excluded -- or `None` for a timeout, a short packet or a bad CRC, in which case
        /// `rx[1]` still says what PID arrived, if any.
        #[unsafe(link_section = ".time_critical.light_usb_pio_receive")]
        #[inline(never)]
        pub fn receive_packet_and_handshake(&mut self, handshake: u8) -> Option<usize> {
                self.rx_count = 0;
                self.rx_started = u8::from(self.wait_for_start());
                if self.rx_started == 0 {
                        return None;
                }
                let pio = unsafe { &*pac::PIO0::ptr() };
                let mut crc: u16 = 0xFFFF;
                let mut crc_prev: u16 = 0xFFFF;
                let mut crc_receive: u16 = 0xFFFF;
                let mut crc_match = false;
                let mut idx = 0usize;
                let mut start = micros();
                loop {
                        if rx_level(pio) != 0 {
                                let data = (pio.rxf(SM_RX).read().bits() >> 24) as u8;
                                if idx < RX_BUFFER {
                                        self.rx[idx] = data;
                                }
                                start = micros();
                                if idx >= 2 {
                                        //   the CRC is kept two bytes behind: when the EOP comes,
                                        // the last two bytes received are the CRC itself, and the
                                        // running value before them is what it must equal
                                        let crc_prev2 = crc_prev;
                                        crc_prev = crc;
                                        crc = crc16_update(crc, data);
                                        crc_receive = (crc_receive >> 8) | (u16::from(data) << 8);
                                        crc_match = (crc_receive ^ 0xFFFF) == crc_prev2;
                                }
                                idx += 1;
                                self.rx_count = idx.min(255) as u8;
                        } else if pio.irq().read().bits() & IRQ_RX_EOP != 0 {
                                //   the handshake must follow within the turnaround: no
                                // waiting, no logging, nothing between the EOP and the send
                                if handshake == PID_ACK {
                                        if idx >= 4 && crc_match {
                                                self.send(&ACK_ENCODED);
                                                return Some(idx - 4);
                                        }
                                } else if handshake == PID_NAK {
                                        self.send(&NAK_ENCODED);
                                } else {
                                        self.send(&STALL_ENCODED);
                                }
                                return None;
                        } else if micros().wrapping_sub(start) > 7 {
                                return None;
                        }
                }
        }

        /// Whether the most recent receive saw an EOP at all: a transaction that ended without
        /// one is a device that stopped mid-packet.
        #[inline(always)]
        pub fn saw_eop(&self) -> bool {
                let pio = unsafe { &*pac::PIO0::ptr() };
                pio.irq().read().bits() & IRQ_RX_EOP != 0
        }
}

/// A jump instruction relocated: the opcode and condition kept, the target rebased.
const fn jmp(instruction: u16, target: u16) -> u16 {
        (instruction & 0xFFE0) | (target & 0x1F)
}

#[inline(always)]
fn set_clkdiv(sm: &pac::pio0::SM, sys_hz: u32, rate_hz: u32) {
        let div256 = u64::from(sys_hz) * 256 / u64::from(rate_hz);
        sm.sm_clkdiv().write(|w| unsafe { w.int().bits((div256 >> 8) as u16).frac().bits((div256 & 0xFF) as u8) });
}

/// Enable or disable one state machine without disturbing the others' bits. The control
/// register is read-modify-written by both the frame handler and the constructor, on one
/// core, with the handler atomic against the other: no race here.
#[inline(always)]
fn sm_enable(pio: &pac::pio0::RegisterBlock, sm: usize, on: bool) {
        pio.ctrl().modify(|r, w| unsafe {
                let bits = r.sm_enable().bits();
                w.sm_enable().bits(if on { bits | (1 << sm) } else { bits & !(1 << sm) })
        });
}

#[inline(always)]
fn rx_level(pio: &pac::pio0::RegisterBlock) -> u8 {
        pio.flevel().read().rx1().bits()
}

/// The low word of the microsecond timer, for the waits above: every one is a few microseconds,
/// and a wrap is handled by the subtraction.
#[inline(always)]
fn micros() -> u32 {
        let timer = unsafe { &*pac::TIMER0::ptr() };
        timer.timerawl().read().bits()
}

// --- encoding and checks ----------------------------------------------------------------------------

/// Encode a packet for the transmitter: NRZI with bit stuffing, each bit a two-bit instruction
/// address packed four to a byte, then the EOP, the completion symbol, and J symbols to the end
/// of the byte (they fall on a released bus, and leave the stalled transmitter side-setting the
/// idle state, as the reference does). Returns the encoded length. The data goes down
/// the wire LSB first, as USB sends it.
#[unsafe(link_section = ".time_critical.light_usb_pio_encode")]
#[inline(never)]
pub fn encode(data: &[u8], out: &mut [u8]) -> usize {
        let mut bit_idx = 0usize;
        // the line is J to start: a 1 keeps it, a 0 flips it
        let mut state_k = false;
        let mut ones = 0u8;
        let put = |out: &mut [u8], bit_idx: &mut usize, sym: u8| {
                let i = *bit_idx >> 2;
                out[i] = (out[i] << 2) | sym;
                *bit_idx += 1;
        };
        for &byte in data {
                for b in 0..8 {
                        if byte & (1 << b) != 0 {
                                put(out, &mut bit_idx, if state_k { SYM_K } else { SYM_J });
                                ones += 1;
                        } else {
                                state_k = !state_k;
                                put(out, &mut bit_idx, if state_k { SYM_K } else { SYM_J });
                                ones = 0;
                        }
                        if ones == 6 {
                                // the stuff bit: a forced transition after six ones
                                state_k = !state_k;
                                put(out, &mut bit_idx, if state_k { SYM_K } else { SYM_J });
                                ones = 0;
                        }
                }
        }
        put(out, &mut bit_idx, SYM_SE0);
        put(out, &mut bit_idx, SYM_COMP);
        while bit_idx & 3 != 0 {
                put(out, &mut bit_idx, SYM_J);
        }
        bit_idx >> 2
}

/// The encoded length of a packet of `n` raw bytes at worst (full stuffing).
pub const fn encoded_len(n: usize) -> usize {
        n * 8 * 7 / 6 / 4 + 4
}

#[unsafe(link_section = ".time_critical.light_usb_pio_crc5")]
static CRC5_TABLE: [u8; 32] = [
        0x00, 0x0b, 0x16, 0x1d, 0x05, 0x0e, 0x13, 0x18, 0x0a, 0x01, 0x1c, 0x17, 0x0f, 0x04, 0x19, 0x12, 0x14, 0x1f, 0x02, 0x09, 0x11, 0x1a, 0x07, 0x0c, 0x1e, 0x15, 0x08, 0x03, 0x1b, 0x10, 0x0d, 0x06,
];

/// The token CRC over its eleven field bits.
#[inline(always)]
pub fn crc5(data: u16) -> u8 {
        let data = data ^ 0x1F;
        let lsb = ((data >> 1) & 0x1F) as u8;
        let msb = ((data >> 6) & 0x1F) as u8;
        let mut crc = if data & 1 != 0 { 0x14 } else { 0x00 };
        crc = CRC5_TABLE[usize::from(lsb ^ crc)];
        crc = CRC5_TABLE[usize::from(msb ^ crc)];
        crc ^ 0x1F
}

#[unsafe(link_section = ".time_critical.light_usb_pio_crc16")]
static CRC16_TABLE: [u16; 256] = [
        0x0000, 0xc0c1, 0xc181, 0x0140, 0xc301, 0x03c0, 0x0280, 0xc241, 0xc601, 0x06c0, 0x0780, 0xc741, 0x0500, 0xc5c1, 0xc481, 0x0440, 0xcc01, 0x0cc0, 0x0d80, 0xcd41, 0x0f00, 0xcfc1, 0xce81, 0x0e40, 0x0a00, 0xcac1, 0xcb81, 0x0b40, 0xc901, 0x09c0, 0x0880, 0xc841, 0xd801, 0x18c0, 0x1980, 0xd941, 0x1b00, 0xdbc1, 0xda81, 0x1a40, 0x1e00, 0xdec1, 0xdf81, 0x1f40, 0xdd01, 0x1dc0, 0x1c80, 0xdc41, 0x1400, 0xd4c1, 0xd581, 0x1540, 0xd701, 0x17c0, 0x1680, 0xd641, 0xd201, 0x12c0, 0x1380, 0xd341, 0x1100, 0xd1c1, 0xd081, 0x1040, 0xf001, 0x30c0, 0x3180, 0xf141, 0x3300, 0xf3c1, 0xf281, 0x3240, 0x3600, 0xf6c1, 0xf781, 0x3740, 0xf501, 0x35c0, 0x3480, 0xf441, 0x3c00, 0xfcc1, 0xfd81, 0x3d40, 0xff01, 0x3fc0, 0x3e80, 0xfe41, 0xfa01, 0x3ac0, 0x3b80, 0xfb41, 0x3900, 0xf9c1, 0xf881, 0x3840, 0x2800, 0xe8c1, 0xe981, 0x2940, 0xeb01, 0x2bc0, 0x2a80, 0xea41, 0xee01, 0x2ec0, 0x2f80, 0xef41, 0x2d00, 0xedc1, 0xec81, 0x2c40, 0xe401, 0x24c0, 0x2580, 0xe541, 0x2700, 0xe7c1, 0xe681, 0x2640, 0x2200, 0xe2c1, 0xe381, 0x2340, 0xe101, 0x21c0, 0x2080, 0xe041, 0xa001, 0x60c0, 0x6180, 0xa141, 0x6300, 0xa3c1, 0xa281, 0x6240, 0x6600, 0xa6c1, 0xa781, 0x6740, 0xa501, 0x65c0, 0x6480, 0xa441, 0x6c00, 0xacc1, 0xad81, 0x6d40, 0xaf01, 0x6fc0, 0x6e80, 0xae41, 0xaa01, 0x6ac0, 0x6b80, 0xab41, 0x6900, 0xa9c1, 0xa881, 0x6840, 0x7800, 0xb8c1, 0xb981, 0x7940, 0xbb01, 0x7bc0, 0x7a80, 0xba41, 0xbe01, 0x7ec0, 0x7f80, 0xbf41, 0x7d00, 0xbdc1, 0xbc81, 0x7c40, 0xb401, 0x74c0, 0x7580, 0xb541, 0x7700, 0xb7c1, 0xb681, 0x7640, 0x7200, 0xb2c1, 0xb381, 0x7340, 0xb101, 0x71c0, 0x7080, 0xb041, 0x5000, 0x90c1, 0x9181, 0x5140, 0x9301, 0x53c0, 0x5280, 0x9241, 0x9601, 0x56c0, 0x5780, 0x9741, 0x5500, 0x95c1, 0x9481, 0x5440, 0x9c01, 0x5cc0, 0x5d80, 0x9d41, 0x5f00, 0x9fc1, 0x9e81, 0x5e40, 0x5a00, 0x9ac1, 0x9b81, 0x5b40, 0x9901, 0x59c0, 0x5880, 0x9841, 0x8801, 0x48c0, 0x4980, 0x8941, 0x4b00, 0x8bc1, 0x8a81, 0x4a40, 0x4e00, 0x8ec1, 0x8f81, 0x4f40, 0x8d01, 0x4dc0, 0x4c80, 0x8c41, 0x4400, 0x84c1, 0x8581, 0x4540, 0x8701, 0x47c0, 0x4680, 0x8641, 0x8201, 0x42c0, 0x4380, 0x8341, 0x4100, 0x81c1, 0x8081, 0x4040,
];

#[inline(always)]
fn crc16_update(crc: u16, byte: u8) -> u16 {
        (crc >> 8) ^ CRC16_TABLE[usize::from((crc ^ u16::from(byte)) & 0xFF)]
}

/// The data CRC, as the packet carries it (inverted, little-endian on the wire).
#[inline(always)]
pub fn crc16(data: &[u8]) -> u16 {
        let mut crc = 0xFFFFu16;
        for &b in data {
                crc = crc16_update(crc, b);
        }
        crc ^ 0xFFFF
}

/// The three handshakes, pre-encoded: SYNC and PID are two bytes with no stuffing possible, so
/// each is four symbols a byte for sixteen bits, then EOP, COMP and two J -- five bytes.
#[unsafe(link_section = ".time_critical.light_usb_pio_ack")]
static ACK_ENCODED: [u8; 5] = encode_const([SYNC, PID_ACK]);
#[unsafe(link_section = ".time_critical.light_usb_pio_nak")]
static NAK_ENCODED: [u8; 5] = encode_const([SYNC, PID_NAK]);
#[unsafe(link_section = ".time_critical.light_usb_pio_stall")]
static STALL_ENCODED: [u8; 5] = encode_const([SYNC, PID_STALL]);

/// [`encode`] at compile time, for the handshakes: the same algorithm, so the test below holds
/// the two to each other.
const fn encode_const(data: [u8; 2]) -> [u8; 5] {
        let mut out = [0u8; 5];
        let mut bit_idx = 0usize;
        let mut state_k = false;
        let mut ones = 0u8;
        let mut i = 0;
        while i < 2 {
                let byte = data[i];
                let mut b = 0;
                while b < 8 {
                        let sym;
                        if byte & (1 << b) != 0 {
                                sym = if state_k { SYM_K } else { SYM_J };
                                ones += 1;
                        } else {
                                state_k = !state_k;
                                sym = if state_k { SYM_K } else { SYM_J };
                                ones = 0;
                        }
                        out[bit_idx >> 2] = (out[bit_idx >> 2] << 2) | sym;
                        bit_idx += 1;
                        if ones == 6 {
                                state_k = !state_k;
                                let s = if state_k { SYM_K } else { SYM_J };
                                out[bit_idx >> 2] = (out[bit_idx >> 2] << 2) | s;
                                bit_idx += 1;
                                ones = 0;
                        }
                        b += 1;
                }
                i += 1;
        }
        out[bit_idx >> 2] = (out[bit_idx >> 2] << 2) | SYM_SE0;
        bit_idx += 1;
        out[bit_idx >> 2] = (out[bit_idx >> 2] << 2) | SYM_COMP;
        bit_idx += 1;
        while bit_idx & 3 != 0 {
                out[bit_idx >> 2] = (out[bit_idx >> 2] << 2) | SYM_J;
                bit_idx += 1;
        }
        out
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;

        /// Decode an encoded stream back to line symbols, for the tests to read.
        fn symbols(encoded: &[u8]) -> std::vec::Vec<u8> {
                let mut v = std::vec::Vec::new();
                for &b in encoded {
                        for shift in [6, 4, 2, 0] {
                                v.push((b >> shift) & 3);
                        }
                }
                v
        }

        /// NRZI-decode a symbol run into bits, dropping stuff bits, up to the EOP.
        fn decode(encoded: &[u8]) -> std::vec::Vec<u8> {
                let syms = symbols(encoded);
                let mut bits = std::vec::Vec::new();
                let mut state_k = false;
                let mut ones = 0;
                for &s in &syms {
                        let k = match s {
                                SYM_J => false,
                                SYM_K => true,
                                _ => break,
                        };
                        if k == state_k {
                                ones += 1;
                                bits.push(1);
                        } else {
                                state_k = k;
                                if ones == 6 {
                                        ones = 0;
                                        continue;
                                }
                                ones = 0;
                                bits.push(0);
                        }
                }
                let mut bytes = std::vec::Vec::new();
                for chunk in bits.chunks(8) {
                        if chunk.len() == 8 {
                                bytes.push(chunk.iter().enumerate().fold(0u8, |acc, (i, b)| acc | (b << i)));
                        }
                }
                bytes
        }

        #[test]
        fn the_handshakes_encode_the_same_at_compile_time_and_run_time() {
                for (pid, want) in [(PID_ACK, ACK_ENCODED), (PID_NAK, NAK_ENCODED), (PID_STALL, STALL_ENCODED)] {
                        let mut out = [0u8; 8];
                        let n = encode(&[SYNC, pid], &mut out);
                        assert_eq!(n, 5);
                        assert_eq!(&out[..5], &want);
                        assert_eq!(decode(&want), std::vec![SYNC, pid]);
                }
        }

        #[test]
        fn encoding_round_trips_through_an_nrzi_decoder_with_stuffing() {
                // 0xFF runs force a stuff bit every six ones, and 0x00 runs none
                let data = [SYNC, PID_DATA0, 0xFF, 0xFF, 0x00, 0x7E, 0xFF, 0xFF, 0xFF, 0x12];
                let mut out = [0u8; 64];
                let n = encode(&data, &mut out);
                assert!(n <= encoded_len(data.len()));
                assert_eq!(decode(&out[..n]), data.to_vec());
                let syms = symbols(&out[..n]);
                let eop = syms.iter().position(|&s| s == SYM_SE0).expect("an EOP");
                assert_eq!(syms[eop + 1], SYM_COMP, "the completion symbol follows the EOP");
                assert!(syms[eop + 2..].iter().all(|&s| s == SYM_J), "padding is J");
        }

        /// The side-set an instruction drives: the top two bits of its delay/side-set field.
        fn side(instruction: u16) -> u16 {
                (instruction >> 11) & 3
        }

        #[test]
        fn the_symbols_are_what_the_transmitter_drives() {
                //   the encoder's names against the program, not against each other: D+ high
                // (side-set 0b01) is J, D- high (0b10) is K, neither is SE0
                assert_eq!(side(TX_PROGRAM[usize::from(SYM_J)]), 0b01, "SYM_J's instruction drives D+ high");
                assert_eq!(side(TX_PROGRAM[usize::from(SYM_K)]), 0b10, "SYM_K's instruction drives D- high");
                assert_eq!(side(TX_PROGRAM[usize::from(SYM_SE0)]), 0b00, "SYM_SE0's instruction drives both low");
        }

        #[test]
        fn a_packet_leaves_idle_with_a_transition() {
                //   the bus idles J, and SYNC's first bit is a 0: the first symbol on the wire
                // must be a K, or the device never sees the packet begin
                let mut out = [0u8; 8];
                encode(&[SYNC, PID_ACK], &mut out);
                let syms = symbols(&out);
                assert_eq!(&syms[..8], &[SYM_K, SYM_J, SYM_K, SYM_J, SYM_K, SYM_J, SYM_K, SYM_K], "SYNC is KJKJKJKK");
        }

        #[test]
        fn a_worst_case_packet_fits_the_encoded_buffer() {
                let mut data = [0xFFu8; MAX_PACKET + 4];
                data[0] = SYNC;
                data[1] = PID_DATA1;
                let mut out = [0u8; ENCODED_MAX];
                let n = encode(&data, &mut out);
                assert!(n <= ENCODED_MAX);
                assert_eq!(decode(&out[..n]), data.to_vec());
        }

        /// The token CRC done bit by bit, the way the specification describes it: the
        /// polynomial x^5 + x^2 + 1 over the eleven field bits as they go down the wire, LSB
        /// first, the register starting and finishing inverted.
        fn crc5_bitwise(data: u16) -> u8 {
                let mut crc = 0x1Fu8;
                for i in 0..11 {
                        let bit = ((data >> i) & 1) as u8;
                        let lsb = crc & 1;
                        crc >>= 1;
                        if lsb ^ bit == 1 {
                                crc ^= 0x14;
                        }
                }
                crc ^ 0x1F
        }

        #[test]
        fn the_token_crc_table_agrees_with_the_bitwise_definition_everywhere() {
                for fields in 0..0x800u16 {
                        assert_eq!(crc5(fields), crc5_bitwise(fields), "fields {fields:#05x}");
                }
        }

        #[test]
        fn the_data_crc_is_the_usb_variant() {
                // the catalogue check value for CRC-16/USB: reflected, 0x8005, inverted in and out
                assert_eq!(crc16(b"123456789"), 0xB4C8);
                assert_eq!(crc16(&[]), 0x0000);
        }

        #[test]
        fn a_received_packet_checks_the_way_it_is_received() {
                //   the receiver keeps the running CRC two bytes behind and compares the last
                // two bytes to it, inverted: the same arithmetic as the sender's, exercised
                // here on a packet the sender's half built
                let payload = [0x09u8, 0x90, 0x3C, 0x64];
                let c = crc16(&payload);
                let wire = [SYNC, PID_DATA0, payload[0], payload[1], payload[2], payload[3], (c & 0xFF) as u8, (c >> 8) as u8];
                let mut crc = 0xFFFFu16;
                let (mut prev, mut receive) = (0xFFFFu16, 0xFFFFu16);
                let mut matched = false;
                for (idx, &b) in wire.iter().enumerate() {
                        if idx >= 2 {
                                let prev2 = prev;
                                prev = crc;
                                crc = crc16_update(crc, b);
                                receive = (receive >> 8) | (u16::from(b) << 8);
                                matched = (receive ^ 0xFFFF) == prev2;
                        }
                }
                assert!(matched);
        }
}
