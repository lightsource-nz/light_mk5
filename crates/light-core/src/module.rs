//! Modules, and the runtime that loads and polls them.
//!
//! Registration is EXPLICIT. An application constructs its modules as values and adds each one
//! to the runtime; the runtime orders them by their declared dependencies and loads them in that
//! order. There is no linker-section walk, no `used` attribute holding the whole thing together,
//! and no per-port linker script -- the three things the predecessor C framework's registration
//! needed, one of which silently broke at -O1 until the attribute was found. A dependency that was never added is an
//! error at start-up with the name in it, not a module that quietly loads without it.
//!
//! Modules own their state. The runtime holds `&mut` to each for as long as it runs, which is
//! how a module's poll can mutate it without a lock, and how two modules cannot alias each
//! other's state by construction.

use heapless::Vec;

/// What a module's poll reports back to the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
        /// Nothing to do right now.
        Idle,
        /// Did work; poll again soon.
        Busy,
        /// Ask the runtime to stop and unload everything.
        Shutdown,
        /// Something went wrong that the module cannot recover from.
        Error,
}

/// How fast the loop is going round, and where its time goes.
///
/// THE LOOP PERIOD IS THE LATENCY. A cooperative runtime answers nothing until the pass that
/// answers it comes round, so however quick a module's own work is, what a person or a wire
/// waits for is one turn of the loop -- and what they wait for at worst is the longest turn,
/// not the average one. A module that spends half a millisecond somewhere is not slow by
/// itself and does not look slow in isolation; it is half a millisecond added to everything
/// else's response. That is the number this exists to expose.
///
/// The pass count is always kept, because it costs one increment. Per-module attribution is
/// not: it reads the clock once per module per pass, which is small but not free, so it is
/// turned on for as long as someone is looking and left off otherwise. WHILE IT IS ON THE
/// LOOP IS GENUINELY SLOWER -- the rate reported under `detail` is of a loop carrying the
/// measurement, so compare shares between modules, and take the rate itself from a plain
/// report.
pub mod timing {
        //   the crate's own atomics, not core's: `swap` below does not exist on the smallest
        // core this framework builds for, and reaching for core's here compiles everywhere
        // except there -- see the re-export in lib.rs
        use crate::atomic::{AtomicBool, Ordering};

        static DETAIL: AtomicBool = AtomicBool::new(false);
        static WANTED: AtomicBool = AtomicBool::new(false);

        /// Time each module separately from the next pass on.
        pub fn set_detail(on: bool) {
                DETAIL.store(on, Ordering::Relaxed);
        }

        pub fn detail() -> bool {
                DETAIL.load(Ordering::Relaxed)
        }

        /// Ask the runtime to say what it has at the end of the next pass, and start a fresh
        /// window. Deferred rather than done here because the figures belong to the runtime,
        /// and whoever asks -- a console command, a key, a timer -- is inside a module and so
        /// inside the very pass being measured.
        pub fn request() {
                WANTED.store(true, Ordering::Relaxed);
        }

        pub(crate) fn wanted() -> bool {
                WANTED.swap(false, Ordering::Relaxed)
        }
}

/// The running tally behind [`timing`]. Microseconds throughout; a window long enough to
/// overflow a `u32` of them is over an hour, and reporting resets it.
struct Timing<const N: usize> {
        /// When the current window began, by the log's clock.
        since_us: u64,
        passes: u32,
        /// Passes in which at least one module reported [`Poll::Busy`].
        busy: u32,
        /// The longest single pass, and the module that took most of it.
        worst_us: u32,
        worst_module: usize,
        /// Per module, in load order: time inside its `poll`. Only filled under `detail`.
        module_us: [u32; N],
        /// Whether any pass in this window was timed. Asked rather than asking `detail`,
        /// because the pass that turns detail on is not in the window it ends -- reporting a
        /// table of zeros there would read as "no module uses any time".
        detailed: bool,
}

impl<const N: usize> Timing<N> {
        const fn new() -> Self {
                Self { since_us: 0, passes: 0, busy: 0, worst_us: 0, worst_module: 0, module_us: [0; N], detailed: false }
        }

        fn restart(&mut self, now_us: u64) {
                self.since_us = now_us;
                self.passes = 0;
                self.busy = 0;
                self.worst_us = 0;
                self.worst_module = 0;
                self.module_us = [0; N];
                self.detailed = false;
        }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
        /// The runtime's fixed capacity is full. Raise `N`, or register fewer modules.
        Capacity,
        /// A module names a dependency nobody registered.
        MissingDependency { module: &'static str, dep: &'static str },
        /// A dependency cycle reaches this module.
        Cycle(&'static str),
        /// A module's `load` failed.
        Load(&'static str),
        /// `run` was called before `start`, or `start` twice.
        NotStarted,
        /// A module returned `Poll::Error`.
        Module(&'static str),
}

pub trait Module {
        /// Unique within one application; also what `deps` refers to.
        fn name(&self) -> &'static str;

        /// Names of modules that must load before this one.
        fn deps(&self) -> &'static [&'static str] {
                &[]
        }

        /// Called once, after every dependency has loaded.
        fn load(&mut self) -> Result<(), ()> {
                Ok(())
        }

        /// Called once at shutdown, in reverse load order.
        fn unload(&mut self) {}

        /// Called repeatedly while the application runs, in load order.
        fn poll(&mut self) -> Poll {
                Poll::Idle
        }
}

/// The application's module set, ordered and driven.
///
/// `N` is the capacity, defaulting to [`DEFAULT_MODULES`](crate::DEFAULT_MODULES); a board names it
/// only for an unusually large module set. Exceeding it is a value the caller sees ([`Error::Capacity`]
/// from [`add`](Runtime::add)) rather than a silently dropped module.
pub struct Runtime<'a, const N: usize = { crate::DEFAULT_MODULES }> {
        modules: Vec<&'a mut dyn Module, N>,
        /// Indices into `modules`, in load order. Empty until `start`.
        order: Vec<usize, N>,
        /// How many of `order` have been loaded (so a failed start unloads only those).
        loaded: usize,
        timing: Timing<N>,
}

impl<'a, const N: usize> Default for Runtime<'a, N> {
        fn default() -> Self {
                Self::new()
        }
}

impl<'a, const N: usize> Runtime<'a, N> {
        pub const fn new() -> Self {
                Self { modules: Vec::new(), order: Vec::new(), loaded: 0, timing: Timing::new() }
        }

        /// Register a module. Order of registration only matters as a tie-break among modules
        /// that do not depend on each other.
        pub fn add(&mut self, module: &'a mut dyn Module) -> Result<(), Error> {
                self.modules.push(module).map_err(|_| Error::Capacity)
        }

        /// Resolve dependencies and load every module in dependency order.
        pub fn start(&mut self) -> Result<(), Error> {
                if !self.order.is_empty() {
                        return Err(Error::NotStarted);
                }
                self.resolve_order()?;
                for i in 0..self.order.len() {
                        let idx = self.order[i];
                        let m = &mut self.modules[idx];
                        if m.load().is_err() {
                                let name = m.name();
                                self.unload_loaded();
                                return Err(Error::Load(name));
                        }
                        self.loaded += 1;
                }
                //   the first window starts here rather than at zero, so a report taken early
                // is not diluted by however long the board spent coming up
                self.timing.restart(crate::log::now_us());
                Ok(())
        }

        /// Poll every module once, in load order. `Shutdown` and `Error` from any module end
        /// the pass; the rest are summarised as `Busy` if anyone was.
        ///
        /// Counting the pass is unconditional and costs an add. Timing each module is not --
        /// see [`timing`] for why, and for what the figures mean.
        pub fn poll_once(&mut self) -> Result<Poll, Error> {
                if self.order.is_empty() {
                        return Err(Error::NotStarted);
                }
                let detail = timing::detail();
                let began = if detail { crate::log::now_us() } else { 0 };
                let mut mark = began;
                let (mut worst_module, mut worst_module_us) = (0usize, 0u32);
                let mut result = Poll::Idle;
                for i in 0..self.order.len() {
                        let m = &mut self.modules[self.order[i]];
                        let polled = m.poll();
                        if detail {
                                let now = crate::log::now_us();
                                let took = (now - mark) as u32;
                                mark = now;
                                self.timing.module_us[i] += took;
                                if took > worst_module_us {
                                        (worst_module, worst_module_us) = (i, took);
                                }
                        }
                        match polled {
                                Poll::Idle => {}
                                Poll::Busy => result = Poll::Busy,
                                Poll::Shutdown => return Ok(Poll::Shutdown),
                                Poll::Error => return Err(Error::Module(m.name())),
                        }
                }
                self.timing.passes += 1;
                if result == Poll::Busy {
                        self.timing.busy += 1;
                }
                if detail {
                        self.timing.detailed = true;
                        let pass = (mark - began) as u32;
                        if pass > self.timing.worst_us {
                                self.timing.worst_us = pass;
                                self.timing.worst_module = worst_module;
                        }
                }
                if timing::wanted() {
                        self.report_timing();
                }
                Ok(result)
        }

        /// Say where the loop's time went over the window just finished, and begin another.
        ///
        /// Written from the runtime because only the runtime holds both halves: the figures
        /// and the names they belong to. A module asking for this ([`timing::request`]) gets
        /// its answer on the console like any other.
        fn report_timing(&mut self) {
                let now = crate::log::now_us();
                let span = now.saturating_sub(self.timing.since_us);
                let passes = self.timing.passes;
                //   a window with no clock behind it (the log's clock is installed by the port,
                // and a host test has none) would divide by zero rather than say anything
                if span == 0 || passes == 0 {
                        self.timing.restart(now);
                        return;
                }
                crate::info!(
                        "loop: {} passes in {} ms -- {} a second, {} us each; {}% of them had work",
                        passes,
                        span / 1000,
                        passes as u64 * 1_000_000 / span,
                        span / passes as u64,
                        self.timing.busy as u64 * 100 / passes as u64,
                );
                if self.timing.detailed {
                        let worst = self.modules[self.order[self.timing.worst_module]].name();
                        crate::info!("loop: the longest pass was {} us, most of it in {}", self.timing.worst_us, worst);
                        //   what the measuring costs, measured the same way: each line below is
                        // one clock read's worth high, because a module's figure runs from the
                        // reading that ended the module before it to the reading that ends it.
                        // Said rather than silently subtracted -- a profiler that quietly
                        // adjusts its own numbers cannot be checked
                        const SAMPLES: u32 = 64;
                        let t0 = crate::log::now_us();
                        for _ in 0..SAMPLES {
                                core::hint::black_box(crate::log::now_us());
                        }
                        let each = (crate::log::now_us() - t0) * 1000 / SAMPLES as u64;
                        crate::info!("loop: reading the clock costs {} ns, which is in every figure below once", each);
                        for i in 0..self.order.len() {
                                let took = self.timing.module_us[i] as u64;
                                crate::info!(
                                        "loop:   {} -- {}% of the loop, {} ns a pass",
                                        self.modules[self.order[i]].name(),
                                        took * 100 / span,
                                        took * 1000 / passes as u64,
                                );
                        }
                }
                self.timing.restart(now);
        }

        /// Poll until a module asks for shutdown or fails, then unload everything in reverse
        /// load order. `idle` is called between passes in which nothing was busy -- the place
        /// a port sleeps or waits for an interrupt.
        pub fn run(&mut self, mut idle: impl FnMut()) -> Result<(), Error> {
                let result = loop {
                        match self.poll_once() {
                                Ok(Poll::Shutdown) => break Ok(()),
                                Ok(Poll::Idle) => idle(),
                                Ok(_) => {}
                                Err(e) => break Err(e),
                        }
                };
                self.unload_loaded();
                result
        }

        /// Names in load order; empty before `start`.
        pub fn load_order(&self) -> impl Iterator<Item = &'static str> + '_ {
                self.order.iter().map(|&i| self.modules[i].name())
        }

        fn unload_loaded(&mut self) {
                while self.loaded > 0 {
                        self.loaded -= 1;
                        let idx = self.order[self.loaded];
                        self.modules[idx].unload();
                }
                self.order.clear();
        }

        /// Depth-first over declared dependencies, in registration order, so the result is the
        /// same every start. A `Visiting` mark on the current path is how a cycle is told
        /// apart from a diamond.
        fn resolve_order(&mut self) -> Result<(), Error> {
                #[derive(Clone, Copy, PartialEq)]
                enum Mark {
                        Unvisited,
                        Visiting,
                        Done,
                }
                let mut marks = [Mark::Unvisited; N];
                //   an explicit stack rather than recursion: N is small but the stack is a
                // fixed budget on a microcontroller, and an iterative walk is bounded by N
                let mut stack: Vec<(usize, usize), N> = Vec::new();

                for root in 0..self.modules.len() {
                        if marks[root] != Mark::Unvisited {
                                continue;
                        }
                        marks[root] = Mark::Visiting;
                        let _ = stack.push((root, 0));
                        while let Some(&(idx, next_dep)) = stack.last() {
                                let deps = self.modules[idx].deps();
                                if next_dep < deps.len() {
                                        stack.last_mut().unwrap().1 += 1;
                                        let dep_name = deps[next_dep];
                                        let dep_idx = self
                                                .modules
                                                .iter()
                                                .position(|m| m.name() == dep_name)
                                                .ok_or(Error::MissingDependency {
                                                        module: self.modules[idx].name(),
                                                        dep: dep_name,
                                                })?;
                                        match marks[dep_idx] {
                                                Mark::Done => {}
                                                Mark::Visiting => return Err(Error::Cycle(dep_name)),
                                                Mark::Unvisited => {
                                                        marks[dep_idx] = Mark::Visiting;
                                                        let _ = stack.push((dep_idx, 0));
                                                }
                                        }
                                } else {
                                        stack.pop();
                                        marks[idx] = Mark::Done;
                                        let _ = self.order.push(idx);
                                }
                        }
                }
                Ok(())
        }
}

#[cfg(test)]
mod tests {
        use super::*;
        use core::cell::RefCell;
        use std::rc::Rc;
        use std::vec::Vec as StdVec;

        extern crate std;

        /// Records load/unload/poll events into a shared journal so order can be asserted.
        struct Probe {
                name: &'static str,
                deps: &'static [&'static str],
                journal: Rc<RefCell<StdVec<std::string::String>>>,
                fail_load: bool,
                polls_until_shutdown: Option<u32>,
        }

        impl Probe {
                fn new(name: &'static str, deps: &'static [&'static str], journal: &Rc<RefCell<StdVec<std::string::String>>>) -> Self {
                        Self { name, deps, journal: journal.clone(), fail_load: false, polls_until_shutdown: None }
                }
                fn note(&self, what: &str) {
                        self.journal.borrow_mut().push(std::format!("{}:{}", what, self.name));
                }
        }

        impl Module for Probe {
                fn name(&self) -> &'static str {
                        self.name
                }
                fn deps(&self) -> &'static [&'static str] {
                        self.deps
                }
                fn load(&mut self) -> Result<(), ()> {
                        self.note("load");
                        if self.fail_load { Err(()) } else { Ok(()) }
                }
                fn unload(&mut self) {
                        self.note("unload");
                }
                fn poll(&mut self) -> Poll {
                        match self.polls_until_shutdown {
                                Some(0) => Poll::Shutdown,
                                Some(n) => {
                                        self.polls_until_shutdown = Some(n - 1);
                                        Poll::Busy
                                }
                                None => Poll::Idle,
                        }
                }
        }

        fn journal() -> Rc<RefCell<StdVec<std::string::String>>> {
                Rc::new(RefCell::new(StdVec::new()))
        }

        #[test]
        fn loads_dependencies_first_and_unloads_in_reverse() {
                let j = journal();
                //   registered app-first, the way an application would list them, with a
                // diamond: app -> {ui, cli} -> core
                let mut app = Probe::new("app", &["ui", "cli"], &j);
                let mut ui = Probe::new("ui", &["core"], &j);
                let mut cli = Probe::new("cli", &["core"], &j);
                let mut core = Probe::new("core", &[], &j);
                app.polls_until_shutdown = Some(2);

                let mut rt: Runtime<8> = Runtime::new();
                rt.add(&mut app).unwrap();
                rt.add(&mut ui).unwrap();
                rt.add(&mut cli).unwrap();
                rt.add(&mut core).unwrap();
                rt.start().unwrap();
                let order: StdVec<_> = rt.load_order().collect();
                assert_eq!(order, ["core", "ui", "cli", "app"]);
                rt.run(|| {}).unwrap();

                let j = j.borrow();
                let unloads: StdVec<_> = j.iter().filter(|e| e.starts_with("unload")).collect();
                assert_eq!(unloads, ["unload:app", "unload:cli", "unload:ui", "unload:core"]);
        }

        #[test]
        fn a_missing_dependency_is_named() {
                let j = journal();
                let mut app = Probe::new("app", &["display"], &j);
                let mut rt: Runtime<4> = Runtime::new();
                rt.add(&mut app).unwrap();
                assert_eq!(
                        rt.start(),
                        Err(Error::MissingDependency { module: "app", dep: "display" })
                );
                assert!(j.borrow().is_empty(), "nothing loads when resolution fails");
        }

        #[test]
        fn a_cycle_is_an_error_not_a_hang() {
                let j = journal();
                let mut a = Probe::new("a", &["b"], &j);
                let mut b = Probe::new("b", &["c"], &j);
                let mut c = Probe::new("c", &["a"], &j);
                let mut rt: Runtime<4> = Runtime::new();
                rt.add(&mut a).unwrap();
                rt.add(&mut b).unwrap();
                rt.add(&mut c).unwrap();
                assert!(matches!(rt.start(), Err(Error::Cycle(_))));
        }

        #[test]
        fn a_failed_load_unloads_what_already_loaded() {
                let j = journal();
                let mut app = Probe::new("app", &["core"], &j);
                let mut core = Probe::new("core", &[], &j);
                app.fail_load = true;
                let mut rt: Runtime<4> = Runtime::new();
                rt.add(&mut app).unwrap();
                rt.add(&mut core).unwrap();
                assert_eq!(rt.start(), Err(Error::Load("app")));
                assert_eq!(*j.borrow(), ["load:core", "load:app", "unload:core"]);
        }

        #[test]
        fn capacity_is_an_error_not_a_silent_drop() {
                let j = journal();
                let mut a = Probe::new("a", &[], &j);
                let mut b = Probe::new("b", &[], &j);
                let mut rt: Runtime<1> = Runtime::new();
                rt.add(&mut a).unwrap();
                assert_eq!(rt.add(&mut b), Err(Error::Capacity));
        }

        #[test]
        fn idle_runs_only_when_nothing_was_busy() {
                let j = journal();
                let mut app = Probe::new("app", &[], &j);
                let mut quiet = Probe::new("quiet", &[], &j);
                app.polls_until_shutdown = Some(3);
                let mut rt: Runtime<4> = Runtime::new();
                rt.add(&mut app).unwrap();
                rt.add(&mut quiet).unwrap();
                rt.start().unwrap();
                let mut idles = 0;
                rt.run(|| idles += 1).unwrap();
                assert_eq!(idles, 0, "app was busy on every pass before shutting down");
        }
}
