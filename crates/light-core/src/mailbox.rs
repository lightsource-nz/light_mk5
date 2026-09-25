//! A bounded, lock-protected queue of plain values, for one module to hand work to another.
//!
//! Modules are owned by the runtime and cannot reference each other, so anything crossing
//! between them goes through a static like this. It is the seed of the typed event bus: the
//! same drop-and-count policy as the log queue, safe across cores
//! through the port's critical section, and `const`-constructible so it can be a `static`.

use core::cell::RefCell;
use portable_atomic::{AtomicU32, Ordering};
use critical_section::Mutex;
use heapless::Deque;

pub struct Mailbox<E: Copy, const N: usize> {
        queue: Mutex<RefCell<Deque<E, N>>>,
        dropped: AtomicU32,
        /// The queue's length again, outside the lock. Maintained under it, so it is exact
        /// for whoever holds it and can only be stale by one operation for whoever does not.
        /// See [`Mailbox::pop`].
        waiting: AtomicU32,
}

impl<E: Copy, const N: usize> Mailbox<E, N> {
        pub const fn new() -> Self {
                Self { queue: Mutex::new(RefCell::new(Deque::new())), dropped: AtomicU32::new(0), waiting: AtomicU32::new(0) }
        }

        /// Queue an event. On a full mailbox the event comes back in the `Err`, and the drop is
        /// counted -- the producer decides whether that matters; the mailbox never blocks.
        pub fn push(&self, event: E) -> Result<(), E> {
                let r = critical_section::with(|cs| {
                        let mut q = self.queue.borrow_ref_mut(cs);
                        let r = q.push_back(event);
                        if r.is_ok() {
                                // after the event is in, so a reader that sees the count finds it
                                self.waiting.store(q.len() as u32, Ordering::Release);
                        }
                        r
                });
                if r.is_err() {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                r
        }

        /// Take the oldest event, if there is one.
        ///
        ///   AN EMPTY MAILBOX IS ANSWERED WITHOUT THE LOCK, for the same reason the event bus
        /// does it: this is called in the runtime's inner loop -- once per device slot per pass
        /// by a forwarder, once a pass by a console reader -- and nearly every call finds
        /// nothing. A critical section to learn that costs more than the rest of the call, and
        /// on a chip with more than one core it is a spinlock held across cores.
        ///
        ///   A push landing between the load and the return is not seen until the next call.
        /// Where the producer is the same core there is no such window at all, and where it is
        /// the other core the event waits one pass, which is what it would have done had it
        /// arrived a moment later.
        pub fn pop(&self) -> Option<E> {
                if self.waiting.load(Ordering::Acquire) == 0 {
                        return None;
                }
                critical_section::with(|cs| {
                        let mut q = self.queue.borrow_ref_mut(cs);
                        let e = q.pop_front();
                        self.waiting.store(q.len() as u32, Ordering::Release);
                        e
                })
        }

        pub fn len(&self) -> usize {
                self.waiting.load(Ordering::Acquire) as usize
        }

        pub fn is_empty(&self) -> bool {
                self.len() == 0
        }

        /// Events refused since construction.
        pub fn dropped(&self) -> u32 {
                self.dropped.load(Ordering::Relaxed)
        }
}

impl<E: Copy, const N: usize> Default for Mailbox<E, N> {
        fn default() -> Self {
                Self::new()
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;

        #[test]
        fn fifo_with_a_counted_drop_on_full() {
                let m: Mailbox<u8, 2> = Mailbox::new();
                assert_eq!(m.push(1), Ok(()));
                assert_eq!(m.push(2), Ok(()));
                assert_eq!(m.push(3), Err(3), "the refused event comes back");
                assert_eq!(m.dropped(), 1);
                assert_eq!(m.pop(), Some(1));
                assert_eq!(m.pop(), Some(2));
                assert_eq!(m.pop(), None);
        }

        #[test]
        fn works_as_a_static_across_threads() {
                static M: Mailbox<u32, 64> = Mailbox::new();
                let producers: std::vec::Vec<_> = (0..4)
                        .map(|p| {
                                std::thread::spawn(move || {
                                        for i in 0..1000u32 {
                                                let _ = M.push(p * 1000 + i);
                                        }
                                })
                        })
                        .collect();
                let mut got = 0u32;
                while producers.iter().any(|h| !h.is_finished()) {
                        while M.pop().is_some() {
                                got += 1;
                        }
                }
                for h in producers {
                        h.join().unwrap();
                }
                while M.pop().is_some() {
                        got += 1;
                }
                assert_eq!(got + M.dropped(), 4000);
        }
}
