//! Input: touch, gesture and orientation. The gesture tracker over a controller's samples
//! (`touch`), the orientation model (`imu`), the runtime touch and IMU modules (`module`), and the
//! [`BoardEvent`] contract that relates them to an application's bus. The reference hardware drivers
//! live under `drivers`, each implementing an agnostic contract the core defines. Everything reaches
//! hardware only through [`light_core::hal`], so the crate builds and tests on the host.

#![no_std]

pub mod drivers;
pub mod imu;
pub mod module;
pub mod touch;

pub use module::{ImuMod, TouchMod};
pub use touch::{Gesture, Swipe, TouchController, TouchDiagnostics, TouchSample, Tracker};

/// The contract a board's input modules need from an application's bus-event type: how to raise the
/// events a touch panel and IMU produce, and how to recognise the requests those modules react to.
/// An application implements this for its event enum, so a board's generic touch/IMU modules (the
/// same wiring on every app) publish onto its bus without naming the app.
pub trait BoardEvent: Copy {
        /// The event a raw touch sample raises.
        fn touch(sample: TouchSample) -> Self;
        /// The event a settled swipe gesture raises.
        fn gesture(gesture: Gesture) -> Self;
        /// The event a settled orientation raises.
        fn orientation(orientation: imu::Orientation) -> Self;
        /// Whether this event is the `stats` request, on which an input module logs its diagnostics.
        fn is_stats(&self) -> bool;
        /// Whether this event reports that a drag was consumed by scrolling -- the touch tracker then
        /// suppresses the release's swipe classification. Defaults to never.
        fn drag_consumed(&self) -> bool {
                false
        }
}
