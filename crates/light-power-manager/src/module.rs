//! The power runtime module: the framework's power *behaviour* as a [`Module`], generic over the
//! board's [`PowerMechanism`], a [`Clock`], and the app's event type. It owns a [`PowerManager`],
//! runs its dim/power-off/shutdown lifecycle each poll, applies backlight commands, and reports
//! battery/external-power on `stats` (only when the mechanism has a gauge). A board wires its
//! mechanism, not a hand-written module.
//!
//! Its two app-event hooks are recognizer fns (as `TouchMod` takes its `reads_held` gate): a
//! backlight command carries a level, and a request reports the battery. Storage/PSRAM diagnostics
//! are a separate concern and stay app-side, not fused into the board module.

use light_core::hal::Clock;
use light_core::{info, Bus, Module, Poll, Subscription};

use crate::{PowerManager, PowerMechanism};

pub struct PowerMod<M: PowerMechanism, C: Clock, A: Copy + 'static> {
        power: PowerManager<M, C>,
        bus: &'static dyn Bus<A>,
        sub: Subscription,
        backlight_of: fn(&A) -> Option<u16>,
        is_stats: fn(&A) -> bool,
        busy_of: fn(&A) -> Option<bool>,
}

impl<M: PowerMechanism, C: Clock, A: Copy + 'static> PowerMod<M, C, A> {
        /// Build a power module over the board's `mech` and `clock`. `backlight_of` yields a
        /// per-mille level for a backlight command; `is_stats` recognises the report request;
        /// `busy_of` yields a busy flag for a request that defers the on-battery power-off (e.g.
        /// audio in flight). A board with no busy-defer passes `|_| None`.
        pub fn new(mech: M, clock: C, bus: &'static dyn Bus<A>, backlight_of: fn(&A) -> Option<u16>, is_stats: fn(&A) -> bool, busy_of: fn(&A) -> Option<bool>) -> Self {
                let sub = bus.subscribe().expect("subscriber slot");
                Self { power: PowerManager::new(mech, clock), bus, sub, backlight_of, is_stats, busy_of }
        }
}

impl<M: PowerMechanism, C: Clock, A: Copy + 'static> Module for PowerMod<M, C, A> {
        fn name(&self) -> &'static str {
                "power"
        }
        fn load(&mut self) -> Result<(), ()> {
                self.power.on_load();
                Ok(())
        }
        fn poll(&mut self) -> Poll {
                let mut busy = false;
                while let Some(ev) = self.bus.poll(&self.sub) {
                        if let Some(level) = (self.backlight_of)(&ev) {
                                busy = true;
                                self.power.set_backlight(level);
                                info!("backlight {level}");
                        } else if (self.is_stats)(&ev) {
                                if let Some(mv) = self.power.battery_mv() {
                                        info!("battery: {} mV, {}", mv, if self.power.on_external_power() { "external power" } else { "on battery" });
                                }
                        } else if let Some(b) = (self.busy_of)(&ev) {
                                //   busy (e.g. audio in flight) holds off the on-battery power-off so a
                                // recording is never cut short; it never defers the dim
                                self.power.set_busy(b);
                        }
                }
                //   the shared power policy: dim on idle, power off on battery after a longer idle,
                // and the button-hold shutdown -- flowing into the runtime, which unloads every
                // module before this one's unload cuts power
                if let Poll::Shutdown = self.power.tick() {
                        return Poll::Shutdown;
                }
                if busy { Poll::Busy } else { Poll::Idle }
        }
        fn unload(&mut self) {
                self.power.on_unload();
        }
}
