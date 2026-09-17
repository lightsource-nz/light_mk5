//! The RTC runtime module: keeps and reports the wall clock, generic over the driver ([`Rtc`]) and
//! the application's event type. It publishes nothing -- it reads and sets the clock in response to
//! the app's events and logs the result -- so a board wires a driver, not a module.
//!
//! The app's RTC requests usually live in its board-specific extension event, which the module
//! cannot name; the application passes two recognizers instead (as `TouchMod` takes its
//! `reads_held` gate): `is_report` -- true for a request on which to log the time (e.g. `stats` or
//! an explicit "show") -- and `get_set` -- `Some(t)` for a request to set the clock.

use light_core::{info, warn, Bus, Module, Poll, Subscription};

use crate::{Datetime, Rtc};

pub struct RtcMod<R: Rtc, A: Copy + 'static> {
        rtc: R,
        bus: &'static dyn Bus<A>,
        sub: Subscription,
        is_report: fn(&A) -> bool,
        get_set: fn(&A) -> Option<Datetime>,
}

impl<R: Rtc, A: Copy + 'static> RtcMod<R, A> {
        /// Wire an RTC driver to an app's `bus`. `is_report` recognises a request to log the time;
        /// `get_set` yields the time to set, if the event is a set request.
        pub fn new(rtc: R, bus: &'static dyn Bus<A>, is_report: fn(&A) -> bool, get_set: fn(&A) -> Option<Datetime>) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { rtc, bus, sub, is_report, get_set }
        }

        fn report(&mut self) {
                match self.rtc.now() {
                        Ok((t, kept)) => info!(
                                "rtc: {:04}-{:02}-{:02} {:02}:{:02}:{:02} (weekday {}){}",
                                t.year,
                                t.month,
                                t.day,
                                t.hour,
                                t.minute,
                                t.second,
                                t.weekday,
                                if kept { "" } else { " UNSET since power loss" }
                        ),
                        Err(e) => warn!("rtc read failed: {e:?}"),
                }
        }
}

impl<R: Rtc, A: Copy + 'static> Module for RtcMod<R, A> {
        fn name(&self) -> &'static str {
                "rtc"
        }
        fn load(&mut self) -> Result<(), ()> {
                match self.rtc.init() {
                        Ok(()) => self.report(),
                        Err(e) => warn!("rtc did not answer: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = self.bus.poll(&self.sub) {
                        if let Some(t) = (self.get_set)(&ev) {
                                busy = true;
                                match self.rtc.set(&t) {
                                        Ok(()) => self.report(),
                                        Err(e) => warn!("rtc set failed: {e:?}"),
                                }
                        } else if (self.is_report)(&ev) {
                                busy = true;
                                self.report();
                        }
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
}
