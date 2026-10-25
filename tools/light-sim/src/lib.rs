//! A simulated board: the port seams, implemented on the host.
//!
//! The framework's portable code reaches hardware only through `light_core::hal` and a few
//! equally narrow traits above it -- a pin, a display bus, a host stack. Every one of those is a
//! seam a desktop implementation can stand behind, and this crate is that implementation: a
//! clock off the process's monotonic time, pins that are shared flags, a display controller
//! that decodes the very commands the real driver sends into an image, a USB-MIDI bus with
//! instruments that plug in and unplug, the console as a byte feed, the log as a drain. An
//! application instantiated over these parts is the same crate the firmware links, built by
//! `cargo` for the host, run as a process.
//!
//! WHAT THIS IS FOR is working on an application when the board is elsewhere -- in production,
//! on someone else's bench, not yet designed -- and seeing it behave: the display drawn, the
//! indicators lit, instruments arriving and MIDI flowing between them, the console answering.
//! It is not a cycle model of a chip. Time is the host's, the loop runs as fast as the host
//! turns it, and the parts answer instantly where real ones take microseconds. What it
//! reproduces is the application's logic against its seams, which is what the host tests
//! reproduce one module at a time; here they run together, with a person at the controls.
//!
//! No window lives here. The seams carry shared handles ([`Line`], [`PanelView`],
//! [`BusHandle`], [`LogView`]) that a project's simulator binary reads from and writes to from
//! whatever interface it puts up -- a window, a terminal, a script. This crate therefore
//! depends on nothing graphical and is tested on the host like any other.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use light_core::hal::{Clock, Idle, InputPin, OutputPin};
use light_core::log::{self, Record};

pub mod sh1107;
pub mod usb;

// --- time ---------------------------------------------------------------------------------------------------------

static START: OnceLock<Instant> = OnceLock::new();

/// Microseconds since the simulation first asked the time; monotonic. The `fn() -> u64` the
/// framework's log and display take.
pub fn now_us() -> u64 {
        START.get_or_init(Instant::now).elapsed().as_micros() as u64
}

/// The simulated board's [`Clock`]: [`now_us`], and a delay that sleeps the thread.
#[derive(Clone, Copy, Default)]
pub struct SimClock;

impl Clock for SimClock {
        fn now_us(&self) -> u64 {
                now_us()
        }

        fn delay_ms(&mut self, ms: u32) {
                std::thread::sleep(Duration::from_millis(u64::from(ms)));
        }
}

/// What the runtime does between idle passes: give the host a moment. A spin would take a
/// whole core for nothing; a millisecond's sleep keeps the loop at about a thousand passes a
/// second when nothing is happening, which is far above anything a person can notice and far
/// below what a tight loop costs.
#[derive(Clone, Copy, Default)]
pub struct SimIdle;

impl Idle for SimIdle {
        fn idle(&mut self) {
                std::thread::sleep(Duration::from_millis(1));
        }
}

// --- pins -----------------------------------------------------------------------------------------------------------

/// One digital line, shared between the application's pin and whoever is looking at it or
/// driving it from outside: an indicator the window shows, a button the window presses.
#[derive(Clone, Default)]
pub struct Line {
        high: Arc<AtomicBool>,
}

impl Line {
        pub fn new(high: bool) -> Self {
                Self { high: Arc::new(AtomicBool::new(high)) }
        }

        pub fn is_high(&self) -> bool {
                self.high.load(Ordering::Relaxed)
        }

        pub fn set(&self, high: bool) {
                self.high.store(high, Ordering::Relaxed);
        }

        /// The line as the application's output: what it drives, the outside reads.
        pub fn output(&self) -> SimOutput {
                SimOutput(self.clone())
        }

        /// The line as the application's input: what the outside drives, it reads.
        pub fn input(&self) -> SimInput {
                SimInput(self.clone())
        }
}

/// An [`OutputPin`] over a [`Line`].
pub struct SimOutput(Line);

impl OutputPin for SimOutput {
        fn set(&mut self, high: bool) {
                self.0.set(high);
        }
}

/// An [`InputPin`] over a [`Line`]: `is_low` is the line's level, so a button wired to ground
/// is "pressed" when its line is set low.
pub struct SimInput(Line);

impl InputPin for SimInput {
        fn is_low(&self) -> bool {
                !self.0.is_high()
        }
}

// --- the log and the console ---------------------------------------------------------------------------------------

/// What the log drain has collected, for an interface to show: every record as the firmware's
/// console would print it, oldest first, and a count of drains as the "console core's" pulse.
#[derive(Clone, Default)]
pub struct LogView {
        lines: Arc<Mutex<Vec<String>>>,
        ticks: Arc<AtomicU32>,
}

impl LogView {
        /// The records so far, as lines.
        pub fn lines(&self) -> Vec<String> {
                self.lines.lock().expect("log lines").clone()
        }

        /// Lines past `from`, so a view appends rather than copies.
        pub fn lines_from(&self, from: usize) -> Vec<String> {
                let lines = self.lines.lock().expect("log lines");
                lines.get(from..).map(<[String]>::to_vec).unwrap_or_default()
        }

        pub fn len(&self) -> usize {
                self.lines.lock().expect("log lines").len()
        }

        pub fn is_empty(&self) -> bool {
                self.len() == 0
        }

        /// How many times the drain has run: the simulated console core's heartbeat, for an
        /// application's probation checks.
        pub fn ticks(&self) -> u32 {
                self.ticks.load(Ordering::Relaxed)
        }

        /// Add a line that did not come from the log -- a console echo, say.
        pub fn note(&self, line: String) {
                self.lines.lock().expect("log lines").push(line);
        }
}

/// Start the thread that drains the framework's log, as the console core does on a device:
/// every record goes to the returned view and, if `echo` is set, to standard output. Runs for
/// the life of the process.
pub fn start_log_drain(echo: bool) -> LogView {
        let view = LogView::default();
        let v = view.clone();
        std::thread::Builder::new()
                .name("light-sim log".into())
                .spawn(move || loop {
                        let mut batch = Vec::new();
                        log::drain(64, |r: &Record| batch.push(r.to_string()));
                        v.ticks.fetch_add(1, Ordering::Relaxed);
                        if batch.is_empty() {
                                //   the queue is a few records deep and a console command answers
                                // in a burst, so the nap is short: a device's console core never
                                // naps at all
                                std::thread::sleep(Duration::from_micros(200));
                                continue;
                        }
                        if echo {
                                for line in &batch {
                                        println!("{line}");
                                }
                        }
                        v.lines.lock().expect("log lines").append(&mut batch);
                })
                .expect("spawn the log drain");
        view
}

/// Start the thread that reads standard input and feeds every byte to the application's
/// console, line by line as a terminal would. Ends quietly at end of input.
pub fn start_stdin(push: fn(u8)) {
        std::thread::Builder::new()
                .name("light-sim stdin".into())
                .spawn(move || {
                        use std::io::BufRead;
                        let stdin = std::io::stdin();
                        for line in stdin.lock().lines() {
                                let Ok(line) = line else { break };
                                send_line(push, &line);
                        }
                })
                .expect("spawn the stdin reader");
}

/// Feed one console line to the application, terminated the way a terminal terminates it.
pub fn send_line(push: fn(u8), line: &str) {
        for b in line.bytes() {
                push(b);
        }
        push(b'\n');
}

#[cfg(test)]
mod tests {
        use super::*;

        #[test]
        fn a_line_is_one_level_seen_from_both_sides() {
                let line = Line::new(true);
                let mut out = line.output();
                let input = line.input();
                assert!(!input.is_low());
                out.set(false);
                assert!(input.is_low());
                assert!(!line.is_high());
                line.set(true);
                assert!(!input.is_low());
        }

        #[test]
        fn the_clock_is_monotonic() {
                let a = now_us();
                let b = now_us();
                assert!(b >= a);
                assert!(SimClock.now_us() >= b);
        }

        #[test]
        fn a_log_view_hands_out_only_what_is_new() {
                let v = LogView::default();
                v.note("one".into());
                v.note("two".into());
                assert_eq!(v.lines_from(1), ["two"]);
                assert!(v.lines_from(5).is_empty());
                assert_eq!(v.len(), 2);
        }

        #[test]
        fn a_console_line_arrives_terminated() {
                use std::sync::Mutex;
                static GOT: Mutex<Vec<u8>> = Mutex::new(Vec::new());
                fn push(b: u8) {
                        GOT.lock().unwrap().push(b);
                }
                send_line(push, "stats");
                assert_eq!(*GOT.lock().unwrap(), b"stats\n");
        }
}
