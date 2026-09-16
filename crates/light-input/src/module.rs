//! The touch and IMU runtime modules, generic over the hardware. A touch panel and an IMU behave
//! the same whatever the application is -- they read the hardware and publish the events those
//! devices raise -- so the modules live here, generic over the app's bus-event type through
//! [`BoardEvent`], over the driver through [`TouchController`] / [`ImuDriver`], and over the clock
//! through [`light_core::hal::Clock`]. A board hands each module its constructed driver, its clock
//! and (for the IMU) its axis map; the module names no board and no port.

use core::fmt::Debug;

use light_core::hal::Clock;
use light_core::{debug, info, warn, Bus, Module, Poll, Subscription};

use crate::cst816t::Event;
use crate::imu::{AxisMap, Imu, ImuDriver};
use crate::{BoardEvent, TouchController, Tracker};

/// Owns a touch controller: reads samples on the driver's cadence, publishes touch and gesture
/// events, and reports the driver's diagnostics on `stats`. Generic over the controller `T`, the
/// app event `A` and the clock `C`.
pub struct TouchMod<A, T, C>
where
        A: BoardEvent + Debug + Send + 'static,
        T: TouchController + Send + 'static,
        C: Clock + 'static,
{
        touch: T,
        tracker: Tracker,
        bus: &'static dyn Bus<A>,
        sub: Subscription,
        clock: C,
        //   the push-versus-touch bisect: whether touch reads should hold while the panel is being
        // pushed. The app owns the state (a console command toggles it); the module just asks.
        reads_held: fn() -> bool,
        /// The last sampled coordinates, for the release log (the up event carries none).
        last: (u16, u16),
        moves: u32,
}

impl<A, T, C> TouchMod<A, T, C>
where
        A: BoardEvent + Debug + Send + 'static,
        T: TouchController + Send + 'static,
        C: Clock + 'static,
{
        /// Wire a controller to an app's `bus`. `reads_held` is the app's push/touch bisect gate.
        pub fn new(touch: T, tracker: Tracker, bus: &'static dyn Bus<A>, clock: C, reads_held: fn() -> bool) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { touch, tracker, bus, sub, clock, reads_held, last: (0, 0), moves: 0 }
        }
}

impl<A, T, C> Module for TouchMod<A, T, C>
where
        A: BoardEvent + Debug + Send + 'static,
        T: TouchController + Send + 'static,
        C: Clock + 'static,
{
        fn name(&self) -> &'static str {
                "touch"
        }
        fn load(&mut self) -> Result<(), ()> {
                //   after the display module's load has reset and initialised any shared chip; the
                // touch read may be the only probe a protocol offers
                let mut result = self.touch.probe();
                for _ in 0..2 {
                        if result.is_ok() {
                                break;
                        }
                        self.clock.delay_ms(20);
                        result = self.touch.probe();
                }
                match result {
                        Ok(()) => info!("touch controller answering"),
                        Err(e) => warn!("touch controller did not answer the probe: {e:?}"),
                }
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = self.bus.poll(&self.sub) {
                        if ev.is_stats() {
                                let d = self.touch.diagnostics();
                                info!("touch: {} failed reads ({} nack, {} timeout, {} bus)", d.failures, d.nacks, d.timeouts, d.bus_errors);
                        } else if ev.drag_consumed() {
                                self.tracker.suppress();
                        }
                }
                if (self.reads_held)() {
                        return Poll::Idle;
                }
                let now_ms = (self.clock.now_us() / 1000) as u32;
                let Some(ev) = self.touch.poll(now_ms) else { return Poll::Idle };
                match ev {
                        Event::Down { x, y } => {
                                self.moves = 0;
                                self.last = (x, y);
                                debug!("touch down at {x},{y}");
                        }
                        Event::Move { x, y } => {
                                self.last = (x, y);
                                self.moves += 1;
                        }
                        Event::Up => debug!("touch up after {} moves at {},{}", self.moves, self.last.0, self.last.1),
                        Event::Reset => {}
                }
                if let Err(e) = self.bus.publish(A::touch(ev)) {
                        warn!("event bus full; dropped {e:?}");
                }
                if let Some(g) = self.tracker.feed(ev, Some(&mut self.touch)) {
                        let _ = self.bus.publish(A::gesture(g));
                }
                Poll::Busy
        }
}

/// Owns an IMU: reads acceleration, settles it into an orientation, and publishes a change.
/// Reports its diagnostics on `stats`. Generic over the driver `D`, the app event `A` and the
/// clock `C`.
pub struct ImuMod<A, D, C>
where
        A: BoardEvent + Send + 'static,
        D: ImuDriver + Send + 'static,
        C: Clock + 'static,
{
        imu: Imu<D>,
        bus: &'static dyn Bus<A>,
        sub: Subscription,
        clock: C,
        axis_map: AxisMap,
}

impl<A, D, C> ImuMod<A, D, C>
where
        A: BoardEvent + Send + 'static,
        D: ImuDriver + Send + 'static,
        C: Clock + 'static,
{
        /// Wire an IMU to an app's `bus`. `axis_map` rotates the chip frame into the device frame.
        pub fn new(imu: Imu<D>, bus: &'static dyn Bus<A>, clock: C, axis_map: AxisMap) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { imu, bus, sub, clock, axis_map }
        }
}

impl<A, D, C> Module for ImuMod<A, D, C>
where
        A: BoardEvent + Send + 'static,
        D: ImuDriver + Send + 'static,
        C: Clock + 'static,
{
        fn name(&self) -> &'static str {
                "imu"
        }
        fn load(&mut self) -> Result<(), ()> {
                match self.imu.driver().probe() {
                        Ok(Some(id)) => info!("imu chip id confirmed: 0x{id:02x}"),
                        Ok(None) => info!("imu present"),
                        Err(e) => warn!("imu did not answer the chip id read: {e:?}"),
                }
                if let Err(e) = self.imu.driver().configure() {
                        warn!("imu configuration failed: {e:?}");
                }
                self.imu.set_axis_map(self.axis_map);
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                while let Some(ev) = self.bus.poll(&self.sub) {
                        if ev.is_stats() {
                                let a = self.imu.accel_mg;
                                info!("imu: accel {} {} {} mg, {:?}, {} failed reads, {}.{} C", a[0], a[1], a[2], self.imu.orientation, self.imu.failures, self.imu.temperature_mc / 1000, (self.imu.temperature_mc % 1000).abs() / 100);
                        }
                }
                let now_ms = (self.clock.now_us() / 1000) as u32;
                if !self.imu.poll(now_ms) {
                        return Poll::Idle;
                }
                if let Some(o) = self.imu.take_orientation() {
                        info!("orientation: {o:?}");
                        let _ = self.bus.publish(A::orientation(o));
                }
                Poll::Busy
        }
}
