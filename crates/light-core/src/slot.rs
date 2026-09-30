//! A `.bss` home for state that `StaticCell` cannot hold.
//!
//! `StaticCell` wants its value `Send`, and a state machine that borrows a shared resource through a
//! `&RefCell` (an open recording over a shared card, say) is not. Such state still must not live on
//! a small core stack -- a kilobyte-scale value inline there is how one core's stack overflows into
//! another's -- so it lives here instead, and the single-core argument that makes it sound is made
//! explicitly at the one place it is taken.

use core::cell::UnsafeCell;

/// One `Option<T>` in static storage, taken once as `&'static mut`.
pub struct CoreSlot<T>(UnsafeCell<Option<T>>);

// SAFETY: the application core is single-core and cooperative, and `take` is the only access, made
// once; so no two references to the contents ever coexist, on any core.
unsafe impl<T> Sync for CoreSlot<T> {}

impl<T> CoreSlot<T> {
        pub const fn new() -> Self {
                Self(UnsafeCell::new(None))
        }

        /// The slot's contents, for the rest of the program.
        ///
        /// # Safety
        ///
        /// Call once per slot, from the core that runs the application; a second call would alias
        /// the first reference.
        #[allow(clippy::mut_from_ref)]
        pub unsafe fn take(&'static self) -> &'static mut Option<T> {
                unsafe { &mut *self.0.get() }
        }
}

impl<T> Default for CoreSlot<T> {
        fn default() -> Self {
                Self::new()
        }
}
