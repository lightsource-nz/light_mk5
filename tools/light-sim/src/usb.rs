//! A simulated USB-MIDI bus: a hub with sockets that instruments plug into, behind the same
//! [`light_midi::Host`] the real host stacks implement.
//!
//! What the application sees is exactly what it sees from a port's host stack: mount and
//! unmount reports with the hub port each instrument sits on, packets arriving from an
//! instrument's cables, packets it writes going out to them. What the simulator's interface
//! sees is the other side of the cable: four sockets, an instrument to plug into each, the
//! packets the application sent it, and a way to send packets as if the instrument had played
//! them. Slots are allocated the way the real stacks allocate them -- the first free index --
//! so an application's slot bookkeeping is exercised the same way.
//!
//! The hub is always there, at a fixed address, as it is on the board that hosts through one.
//! An instrument plugged "straight into the root port" is a socket of a different kind and is
//! not modelled: every real board this framework has either hosts through a hub or is an
//! adapter cable away from one.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use light_midi::{BusInfo, Host, MidiEvent, Mount, Packet, Transport};

/// Device slots, as the port's host stacks count them.
pub const SLOTS: usize = 4;
/// The hub's sockets, numbered 1 to 4 as the hub numbers its ports.
pub const PORTS: u8 = 4;
/// The hub's device address: the root device, enumerated first.
pub const HUB_ADDR: u8 = 2;

/// What an instrument is, as far as the bus cares.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Instrument {
        pub name: String,
        /// Cables from the instrument to the host.
        pub rx_cables: u8,
        /// Cables from the host to the instrument.
        pub tx_cables: u8,
}

impl Instrument {
        pub fn new(name: &str, rx_cables: u8, tx_cables: u8) -> Self {
                Self { name: name.to_owned(), rx_cables, tx_cables }
        }
}

/// One mounted instrument.
struct Slot {
        port: u8,
        daddr: u8,
        instrument: Instrument,
        /// Packets the instrument has played, waiting for the application.
        to_host: VecDeque<Packet>,
        /// Packets the application wrote, waiting for the interface.
        from_host: VecDeque<Packet>,
        /// Flushes the application asked for.
        flushes: u32,
}

struct Inner {
        slots: [Option<Slot>; SLOTS],
        events: VecDeque<MidiEvent>,
        dropped_events: u32,
        next_daddr: u8,
        /// Packets written to a slot that is not there: counted, like a port counts what it
        /// could not accept.
        lost_out: u32,
}

/// What a socket holds, for the interface.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Socket {
        pub port: u8,
        pub slot: u8,
        pub daddr: u8,
        pub instrument: Instrument,
        pub flushes: u32,
}

/// The interface's side of the bus: plug, unplug, play, and see what was sent.
#[derive(Clone)]
pub struct BusHandle {
        inner: Arc<Mutex<Inner>>,
}

/// The application's side: the [`Host`].
pub struct SimUsbHost {
        inner: Arc<Mutex<Inner>>,
}

impl SimUsbHost {
        /// A bus with its hub up and every socket empty.
        pub fn new() -> (Self, BusHandle) {
                let inner = Arc::new(Mutex::new(Inner { slots: [const { None }; SLOTS], events: VecDeque::new(), dropped_events: 0, next_daddr: HUB_ADDR + 1, lost_out: 0 }));
                (Self { inner: inner.clone() }, BusHandle { inner })
        }
}

impl BusHandle {
        /// Plug `instrument` into socket `port` (1 to 4). Refused if the socket is taken or every
        /// slot is in use -- the latter the way a host stack refuses a fifth device.
        pub fn plug(&self, port: u8, instrument: Instrument) -> Result<u8, &'static str> {
                if !(1..=PORTS).contains(&port) {
                        return Err("no such socket");
                }
                let mut i = self.inner.lock().expect("bus");
                if i.slots.iter().flatten().any(|s| s.port == port) {
                        return Err("the socket is taken");
                }
                let Some(idx) = i.slots.iter().position(Option::is_none) else {
                        return Err("no free slot");
                };
                let daddr = i.next_daddr;
                i.next_daddr = if daddr >= 127 { HUB_ADDR + 1 } else { daddr + 1 };
                let mount = Mount { daddr, rx_cables: instrument.rx_cables.max(1), tx_cables: instrument.tx_cables.max(1) };
                i.slots[idx] = Some(Slot { port, daddr, instrument, to_host: VecDeque::new(), from_host: VecDeque::new(), flushes: 0 });
                i.events.push_back(MidiEvent::Mounted { idx: idx as u8, mount, bus: Some(BusInfo { hub_addr: HUB_ADDR, hub_port: port }) });
                Ok(idx as u8)
        }

        /// Pull the instrument out of socket `port`.
        pub fn unplug(&self, port: u8) -> bool {
                let mut i = self.inner.lock().expect("bus");
                let Some(idx) = i.slots.iter().position(|s| s.as_ref().is_some_and(|s| s.port == port)) else {
                        return false;
                };
                i.slots[idx] = None;
                i.events.push_back(MidiEvent::Unmounted { idx: idx as u8 });
                true
        }

        /// The instrument in socket `port` plays `packet`.
        pub fn play(&self, port: u8, packet: Packet) -> bool {
                let mut i = self.inner.lock().expect("bus");
                match i.slots.iter_mut().flatten().find(|s| s.port == port) {
                        Some(s) => {
                                s.to_host.push_back(packet);
                                true
                        }
                        None => false,
                }
        }

        /// What the application has sent to the instrument in socket `port` since last asked.
        pub fn take_sent(&self, port: u8) -> Vec<Packet> {
                let mut i = self.inner.lock().expect("bus");
                match i.slots.iter_mut().flatten().find(|s| s.port == port) {
                        Some(s) => s.from_host.drain(..).collect(),
                        None => Vec::new(),
                }
        }

        /// Every socket that holds an instrument.
        pub fn sockets(&self) -> Vec<Socket> {
                let i = self.inner.lock().expect("bus");
                i.slots.iter().enumerate().filter_map(|(idx, s)| s.as_ref().map(|s| Socket { port: s.port, slot: idx as u8, daddr: s.daddr, instrument: s.instrument.clone(), flushes: s.flushes })).collect()
        }

        pub fn socket(&self, port: u8) -> Option<Socket> {
                self.sockets().into_iter().find(|s| s.port == port)
        }

        /// Packets written to a slot that held nothing.
        pub fn lost_out(&self) -> u32 {
                self.inner.lock().expect("bus").lost_out
        }
}

impl Host for SimUsbHost {
        fn task(&mut self) {
                // the bus has no work of its own: plugging and playing happen on the other side
        }

        fn next_event(&mut self) -> Option<MidiEvent> {
                self.inner.lock().expect("bus").events.pop_front()
        }

        fn dropped_events(&self) -> u32 {
                self.inner.lock().expect("bus").dropped_events
        }
}

impl Transport for SimUsbHost {
        fn read(&mut self, idx: u8) -> Option<Packet> {
                let mut i = self.inner.lock().expect("bus");
                i.slots.get_mut(usize::from(idx))?.as_mut()?.to_host.pop_front()
        }

        fn write(&mut self, idx: u8, packet: &Packet) {
                let mut i = self.inner.lock().expect("bus");
                match i.slots.get_mut(usize::from(idx)).and_then(Option::as_mut) {
                        Some(s) => s.from_host.push_back(*packet),
                        None => i.lost_out += 1,
                }
        }

        fn flush(&mut self, idx: u8) {
                let mut i = self.inner.lock().expect("bus");
                if let Some(s) = i.slots.get_mut(usize::from(idx)).and_then(Option::as_mut) {
                        s.flushes += 1;
                }
        }

        fn dropped_packets(&self) -> (u32, u32) {
                (0, self.lost_out_raw())
        }
}

impl SimUsbHost {
        fn lost_out_raw(&self) -> u32 {
                self.inner.lock().expect("bus").lost_out
        }
}

// --- packets, for people -----------------------------------------------------------------------------------------

/// A USB-MIDI event packet for a MIDI message on `cable`: the code index number from the
/// status byte, the bytes behind it.
pub fn packet(cable: u8, status: u8, d1: u8, d2: u8) -> Packet {
        let cin = match status & 0xF0 {
                0x80 => 0x8,
                0x90 => 0x9,
                0xA0 => 0xA,
                0xB0 => 0xB,
                0xC0 => 0xC,
                0xD0 => 0xD,
                0xE0 => 0xE,
                // a system message of three bytes; one- and two-byte ones are not what a
                // simulated instrument plays
                _ => 0x3,
        };
        [((cable & 0xF) << 4) | cin, status, d1, d2]
}

pub fn note_on(cable: u8, channel: u8, note: u8, velocity: u8) -> Packet {
        packet(cable, 0x90 | (channel & 0xF), note & 0x7F, velocity & 0x7F)
}

pub fn note_off(cable: u8, channel: u8, note: u8) -> Packet {
        packet(cable, 0x80 | (channel & 0xF), note & 0x7F, 0x40)
}

pub fn control_change(cable: u8, channel: u8, controller: u8, value: u8) -> Packet {
        packet(cable, 0xB0 | (channel & 0xF), controller & 0x7F, value & 0x7F)
}

/// A packet as a person reads it: the cable, then the message.
pub fn describe(p: &Packet) -> String {
        let cable = p[0] >> 4;
        let cin = p[0] & 0x0F;
        let channel = (p[1] & 0x0F) + 1;
        let body = match cin {
                0x8 => format!("note off ch{channel} {} vel {}", p[2], p[3]),
                0x9 => format!("note on ch{channel} {} vel {}", p[2], p[3]),
                0xA => format!("poly pressure ch{channel} {} {}", p[2], p[3]),
                0xB => format!("control ch{channel} cc{} = {}", p[2], p[3]),
                0xC => format!("program ch{channel} {}", p[2]),
                0xD => format!("pressure ch{channel} {}", p[2]),
                0xE => format!("bend ch{channel} {}", (u16::from(p[3]) << 7 | u16::from(p[2])) as i32 - 8192),
                0x0 | 0x1 => "padding".to_owned(),
                _ => format!("{:02x} {:02x} {:02x} (cin {cin:x})", p[1], p[2], p[3]),
        };
        format!("cable {cable}: {body}")
}

#[cfg(test)]
mod tests {
        use super::*;

        fn keys() -> Instrument {
                Instrument::new("keys", 1, 1)
        }

        #[test]
        fn plugging_in_mounts_in_the_first_free_slot_with_its_port() {
                let (mut host, bus) = SimUsbHost::new();
                assert_eq!(bus.plug(2, keys()), Ok(0));
                assert_eq!(bus.plug(4, Instrument::new("pads", 2, 1)), Ok(1));
                match host.next_event() {
                        Some(MidiEvent::Mounted { idx: 0, mount, bus: Some(b) }) => {
                                assert_eq!(mount, Mount { daddr: 3, rx_cables: 1, tx_cables: 1 });
                                assert_eq!(b, BusInfo { hub_addr: HUB_ADDR, hub_port: 2 });
                        }
                        other => panic!("{other:?}"),
                }
                assert!(matches!(host.next_event(), Some(MidiEvent::Mounted { idx: 1, .. })));
                assert!(host.next_event().is_none());
                assert_eq!(bus.plug(2, keys()), Err("the socket is taken"));
                assert_eq!(bus.plug(9, keys()), Err("no such socket"));
        }

        #[test]
        fn unplugging_frees_the_slot_and_the_next_instrument_takes_it() {
                let (mut host, bus) = SimUsbHost::new();
                bus.plug(1, keys()).unwrap();
                bus.plug(2, keys()).unwrap();
                assert!(bus.unplug(1));
                assert!(!bus.unplug(1));
                while host.next_event().is_some_and(|e| matches!(e, MidiEvent::Mounted { .. })) {}
                assert_eq!(bus.plug(3, keys()), Ok(0));
                assert_eq!(bus.sockets().len(), 2);
                assert_eq!(bus.socket(3).unwrap().slot, 0);
        }

        #[test]
        fn a_fifth_instrument_is_refused() {
                let (_host, bus) = SimUsbHost::new();
                for port in 1..=4 {
                        bus.plug(port, keys()).unwrap();
                }
                // every socket is taken too, but the slots run out first in spirit: unplug one
                // socket and re-plug it, then there is still no fifth slot for a fifth socket
                assert_eq!(bus.plug(1, keys()), Err("the socket is taken"));
        }

        #[test]
        fn packets_cross_the_cable_in_both_directions() {
                let (mut host, bus) = SimUsbHost::new();
                let idx = bus.plug(1, keys()).unwrap();
                let on = note_on(0, 0, 60, 100);
                assert!(bus.play(1, on));
                assert!(!bus.play(2, on));
                assert_eq!(host.read(idx), Some(on));
                assert_eq!(host.read(idx), None);
                host.write(idx, &note_off(0, 0, 60));
                host.flush(idx);
                assert_eq!(bus.take_sent(1), [note_off(0, 0, 60)]);
                assert!(bus.take_sent(1).is_empty());
                assert_eq!(bus.socket(1).unwrap().flushes, 1);
                host.write(3, &on);
                assert_eq!(host.dropped_packets(), (0, 1));
        }

        #[test]
        fn packets_describe_themselves() {
                assert_eq!(describe(&note_on(1, 0, 60, 100)), "cable 1: note on ch1 60 vel 100");
                assert_eq!(describe(&control_change(0, 9, 7, 127)), "cable 0: control ch10 cc7 = 127");
                assert_eq!(describe(&[0x0E, 0xE0, 0x00, 0x40]), "cable 0: bend ch1 0");
                assert_eq!(describe(&[0x00, 0, 0, 0]), "cable 0: padding");
        }
}
