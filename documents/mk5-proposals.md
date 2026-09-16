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

## B. Promote board-generic runtime modules into the framework — *decided*

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

## D. One UI construction path: data only — *decided*

> **Decided (mk5), design recorded** in [09](09-application-model.md#) — the LUI blob is the one
> app-facing path; `UiSource` collapses to the blob and the const-tree public API (`Page`/`Desc`,
> `navigate(&Page)`/`build(&Desc)`) is retired app-facing, with the low-level widget creators kept
> for the LUI builder and the toolkit's tests. Grounded in the finding that no app uses `Const` today.
> **Not yet applied to code.**

- **Status quo (mk4).** A UI can be built two ways — a hand-written `const` page tree, or a compiled
  LUI blob (the `UiSource` `Const` | `Blob` dual path).
- **Friction.** Two construction paths that must be kept behaviorally identical.
- **Proposed mk5 direction.** Make the LUI blob the single path; retire the const path, or keep it
  strictly as a test fixture. Every interface is authored as data.
- **Touches.** [02-graphics-and-ui.md](02-graphics-and-ui.md),
  [09-application-model.md](09-application-model.md) (interface-as-data).

## E. One audio streaming path: the prefetch ring — *decided*

> **Decided (mk5), design recorded** in [04](04-audio-and-midi.md#) — remove the polled path; the IRQ
> prefetch ring is the single model. Grounded in the finding that the `AudioStream` contract is
> already ring-only and the polled `start_stream`/`refill` are unused inherent methods; removing them
> also retires the idle-drain-miscounted-as-starvation wrinkle. A RAM-tight board is a port concern
> (proposal G), not a second contract path. **Not yet applied to code.**

- **Status quo (mk4).** `AudioStream` has a polled path and an IRQ prefetch-ring path; the polled
  path silently miscounts idle draining as starvation.
- **Friction.** Two implementations of one contract, and the simpler one has a correctness wrinkle.
- **Proposed mk5 direction.** The IRQ prefetch ring is the model the contract is built around; the
  polled path becomes a documented degenerate fallback, or is dropped.
- **Touches.** [04-audio-and-midi.md](04-audio-and-midi.md),
  [01-core-runtime.md](01-core-runtime.md) (`AudioStream` hal).

## F. Rework the build and port seams — *proposed*

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

## G. Static capacities and the memory model — *proposed*

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
