# mk5 proposals — the improvement backlog

A working list of candidate design changes for mk5, extracted from an examination of the mk4
structure (2026-09-17). This is a **transitional** document: as each item is designed and agreed, its
design is written into the relevant subsystem document (labelled an mk5 decision) and its status here
is updated to point there. An item is not a decision until it says so.

**Status values:** `proposed` (captured, not yet designed) · `designing` · `decided` (design written
into the spec) · `applied` (spec and code in step).

The items are independent enough to take in any order, but **A** and **B** set the shape the others
instantiate against, so they come first.

---

## A. Generalize the board-support layer to every board — *decided*

> **Decided (mk5), design recorded** in [09](09-application-model.md#) (required board layering),
> [07](07-ports-and-shell.md#) (shell glue as a `shell` module in the port), and
> [03](03-input.md#) (generic input runtime modules + a `TouchController` trait). Per the A/B split
> decision, A also folds in the generic input-module extraction, so board crates become
> board-specific-only immediately; B covers the remaining app-coupled modules (RTC, audio, power).
> **Not yet applied to code.**

- **Status quo (mk4).** The app-agnostic board-support crate — pin map and peripheral hand-over,
  shell ABI glue, and board-generic input modules — exists for a *single* board. Every other board
  re-implements the same wiring inside its per-board application crate, and the shared application
  crate does not actually own the wiring: each per-board crate re-does it.
- **Friction.** The board wiring and the same application are duplicated once per board; drift risk
  grows with each board; adding a board means copying hundreds of lines.
- **Proposed mk5 direction.** Make "one board-support crate per board + a thin per-board
  instantiation crate" the *required* structure, so per-board crates collapse to tens of lines (as
  the deduplicated executables already have). The board-support crate holds everything
  board-specific-but-app-agnostic; the instantiation crate only constructs peripherals, embeds
  assets, and wires modules.
- **Touches.** [09-application-model.md](09-application-model.md), [00-overview.md](00-overview.md)
  (principles / conventions).

## B. Promote board-generic runtime modules into the framework — *implemented*

> **Implemented (mk5).** RTC → `light_rtc::RtcMod` (over a new `Rtc` trait); power →
> `light_power_manager::PowerMod` (349 facade removed, 349/4.0 diagnostics unfused, the dictaphone's
> audio-busy defer preserved). Both roll out across the touch boards and build to `.uf2`; RTC is
> hardware-verified on the 4.0. Event recognition is by function pointer, not a trait, wherever the
> events live in the board's extension type (orphan rule). **Audio: resolved without an extraction** —
> the reusable recorder is already the generic `light_dictaphone_core` crate and does not decompose
> into a framework primitive (see [04](04-audio-and-midi.md#)); the real dedup wins are RTC + power.
> Decision design below is superseded by these implementation notes.


> **Decided (mk5), design recorded** in [06](06-power-and-time.md#) (power lifecycle → a `PowerMod`
> in `light-power-manager`, storage/PSRAM diagnostics unfused; RTC → an `RtcMod` + a new `Rtc` driver
> trait in `light-rtc`), [04](04-audio-and-midi.md#) (a framework audio-*player* primitive in
> `light-audio`; the card recorder stays a reusable app-level engine), and [09](09-application-model.md#)
> (the seam). Each module is generic over its driver/mechanism, a clock, and the app event via a small
> per-subsystem event trait, extending the `BoardEvent` pattern. **Not yet applied to code.**

- **Status quo (mk4).** The generic input modules are shared but live in one board crate; the
  RTC, audio, and power modules are app-coupled and re-implemented per application.
- **Friction.** Reusable runtime modules are re-done per app/board instead of being framework-provided.
- **Proposed mk5 direction.** Promote the reusable runtime modules (input, RTC, power, audio) into
  framework crates, parameterized by the board's driver and the app's event type, so a board wires
  *drivers*, not *modules*. Sharpen the app-agnostic vs app-coupled boundary as the rule that decides
  what can be framework-level.
- **Touches.** [09-application-model.md](09-application-model.md), [03-input.md](03-input.md),
  [06-power-and-time.md](06-power-and-time.md), [04-audio-and-midi.md](04-audio-and-midi.md),
  [01-core-runtime.md](01-core-runtime.md) (module model).

## C. Decompose `light-ui` — *decided*

> **Decided (mk5), design recorded** in [02](02-graphics-and-ui.md#) — split the ~4.6k-line toolkit
> into focused modules (model, desc, style, layout, scroll, input, nav, anim, render; lui/theme
> already separate), mirroring `light-core`. Behaviour-preserving refactor. **Not yet applied to code.**

- **Status quo (mk4).** The widget toolkit is a single ~4.6k-line source file (plus the LUI runtime
  and theme). By contrast `light-core` is cleanly split into focused ~100–400-line modules.
- **Friction.** Hard to test a part in isolation, hard to reason about, monolithic recompiles.
- **Proposed mk5 direction.** Split the toolkit into focused modules — layout, widgets,
  animation/transitions, the LUI runtime, theme — mirroring how `light-core` is organized. A
  structural refactor with no behaviour change, making each part independently testable.
- **Touches.** [02-graphics-and-ui.md](02-graphics-and-ui.md).

## D. One UI construction path: data only — *implemented*

> **Implemented (mk5)**, design in [09](09-application-model.md#). `UiSource` removed from both the
> demo and the dictaphone — each app's display config now holds a parsed `Lui` blob directly. The two
> const-tree holdouts (the OLED Pico and the bare-CMSIS STM32) were ported to LUI blobs, so every
> application is now blob-authored; they share one `design.json` via `extends`, overriding only device
> size and row height. The const-tree API (`Page`/`Desc`, `navigate(&Page)`/`build(&Desc)`) is retired
> as an app-facing whole-page path — no app uses it — but stays `pub`, being the machinery behind
> `file_list!` and the toolkit's own test fixtures. Verified: full host suite green; six firmware
> targets build (both key boards, both dictaphone variants, two touch demos).

- **Status quo (mk4).** A UI can be built two ways — a hand-written `const` page tree, or a compiled
  LUI blob (the `UiSource` `Const` | `Blob` dual path).
- **Friction.** Two construction paths that must be kept behaviorally identical.
- **Proposed mk5 direction.** Make the LUI blob the single path; retire the const path, or keep it
  strictly as a test fixture. Every interface is authored as data.
- **Touches.** [02-graphics-and-ui.md](02-graphics-and-ui.md),
  [09-application-model.md](09-application-model.md) (interface-as-data).

## E. One audio streaming path: the prefetch ring — *implemented*

> **Implemented (mk5)**, design in [04](04-audio-and-midi.md#). Removed `PioI2sOut`'s polled path
> (`start_stream`/`refill`, the polled-only `underruns()`/`reset_polled_underruns()` accessors, and
> the `last_busy`/`underruns` fields they used) from `light-rp2`. The `AudioStream` contract was
> already ring-only, the polled methods had no callers, and their removal also retired the
> idle-drain-miscounted-as-starvation wrinkle. The IRQ prefetch ring is the single model; a RAM-tight
> board is a port concern (proposal G), not a second contract path. Verified: the two touch349 audio
> targets (ui_demo, dictaphone) build and link.

- **Status quo (mk4).** `AudioStream` has a polled path and an IRQ prefetch-ring path; the polled
  path silently miscounts idle draining as starvation.
- **Friction.** Two implementations of one contract, and the simpler one has a correctness wrinkle.
- **Proposed mk5 direction.** The IRQ prefetch ring is the model the contract is built around; the
  polled path becomes a documented degenerate fallback, or is dropped.
- **Touches.** [04-audio-and-midi.md](04-audio-and-midi.md),
  [01-core-runtime.md](01-core-runtime.md) (`AudioStream` hal).

## F. Rework the build and port seams — *implemented*

> **Implemented (mk5)**, design in [10](10-build-and-release.md#). Two workspaces: the repository-root
> `Cargo.toml` is now portable-only (framework + portable apps + host tools), and a new
> `firmware/Cargo.toml` virtual manifest holds the ports, board-support, per-board instantiation
> crates and `module/*` executables. Each firmware crate names the firmware manifest with
> `workspace = "…/firmware"`, so no root `--exclude`/`exclude` list exists on either side — host tests
> are `cargo test --workspace` (bar the two GUI tools), and adding a board touches only
> `firmware/Cargo.toml`'s members. Corrosion imports the firmware staticlibs from `firmware/Cargo.toml`.
> The four `_app` crates were renamed to `light_app_<name>` (part 1, done separately). The
> single-global critical-section stays a per-firmware property; F changes the build around it. Verified:
> full host suite green with no port excludes; RP2350 (ui_demo_touch349) and STM32H7 (light_mk4_h7)
> firmware build and link through the firmware workspace.

- **Status quo (mk4).** The build system's executable/crate namespace collision forces an `_app`
  suffix on some crates; the single-global `critical-section` means the two flavours cannot coexist,
  so there is no workspace-wide cross build and host tests must exclude every port.
- **Friction.** A naming workaround baked into crate names; no single build/test invocation; the
  ports are carved out of the host-test surface.
- **Proposed mk5 direction.** Design a build/port model where one invocation builds and tests, the
  namespace workaround disappears, and the critical-section choice composes per-firmware without
  poisoning a workspace-wide build. *Deep item — needs its own design pass.*
- **Touches.** [10-build-and-release.md](10-build-and-release.md),
  [07-ports-and-shell.md](07-ports-and-shell.md).

## G. Static capacities and the memory model — *implemented*

> **Capacities half — implemented (mk5)**, design in [01](01-core-runtime.md#). `EventBus` and
> `Runtime` gained default const-generic parameters (`light_core::DEFAULT_MODULES` /
> `DEFAULT_EVENT_DEPTH`), so a board names a capacity only when it differs; the default subscriber
> count *is* the module default (one slot per module), so a default bus cannot under-provision a
> default runtime, and over-provisioning surfaces at startup, never as a mid-run panic. Backward
> compatible with the explicit-capacity boards; `Ui`'s arena stays explicit (no meaningful default).
> Verified: light-core tests (incl. a defaults test) + full host suite green; a firmware board with
> explicit capacities still builds.
>
> **Memory-model half — implemented (mk5)**, design in [02](02-graphics-and-ui.md#). Region buffering
> keeps the page slide on a SINGLE buffer: the outgoing image survives in the live buffer as pixels and
> is scrolled off in place by `Canvas::shift_region` (a new in-place single-axis buffer scroll) while
> the incoming is painted into the strip it uncovers — no second frame and no second widget tree. A
> board opts in with `Display::set_region_buffering` and supplies no back buffer; every direction is a
> reveal (one buffer cannot slide the incoming in), and rotation snaps (single-buffer already does).
> Verified: a differential host test proves the region path is byte-identical to the capture path at
> every step of a horizontal reveal; the 3.49 ui_demo was flipped to region buffering (215 KB back
> buffer removed) and confirmed smooth and tear-free on the glass, frames advancing with zero skips and
> zero chunk timeouts. The `Full`/over/scanout paths are untouched, so boards not opted in are
> unchanged.

- **Status quo (mk4).** Fixed-capacity generics (the event bus, the UI arena, the runtime) push
  sizing onto every board; the double framebuffer dominates RAM and left the largest board genuinely
  tight.
- **Friction.** Every board picks capacity numbers; features are RAM-gated by a single fixed
  buffering strategy.
- **Proposed mk5 direction.** Derive or default capacities where possible, and make the buffering
  strategy configurable (for example partial/region buffering as an option) so features are not gated
  by one fixed memory model. *Deep item — needs its own design pass.*
- **Touches.** [01-core-runtime.md](01-core-runtime.md) (capacities),
  [02-graphics-and-ui.md](02-graphics-and-ui.md) (display buffering),
  [09-application-model.md](09-application-model.md).
