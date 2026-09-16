---
name: refer-to-spec
description: Consult and uphold the Light Framework specification when working on framework code. Use whenever reading, changing, reviewing, or designing any Light Framework crate (light-core, light-draw, light-display, light-ui, light-input, light-audio, light-midi, light-sd, light-fs, light-power, light-rtc, a port, crush, or an app/board crate). The spec in light_mk5/documents is normative and the code follows it.
---

# Refer to the Light Framework spec

The specification under `light_mk5/documents/` is **normative**: it defines the structure and
behaviour the code implements, and the code follows it. Consult it before and during any framework
work — do not design or change framework behaviour from memory when a document covers it.

## Before starting

1. Identify the subsystem you are touching and open its document:
   - `00-overview.md` — principles, layering, and the **Conventions** section (read this first).
   - `01-core-runtime.md` — light-core: the hal port interface, runtime, event bus, log, CLI.
   - `02-graphics-and-ui.md` — font / draw / display / ui / components.
   - `03-input.md` — touch, gesture, IMU, `BoardEvent`.
   - `04-audio-and-midi.md` · `05-storage.md` · `06-power-and-time.md` ·
     `07-ports-and-shell.md` · `08-assets-and-tooling.md` ·
     `09-application-model.md` · `10-build-and-release.md`.
2. Treat the document's stated **contracts, public surface, and invariants** as the requirements.
   Implement to the contract, not to one piece of hardware.

## While working

Follow the **Conventions** in `00-overview.md`. In particular:

- Portable code names no hardware; it reaches the world only through `light_core::hal` traits.
- Modules couple only through the event bus — never by calling each other or sharing state.
- Large objects live in `.bss` via `StaticCell`/`ConstStaticCell`, taken once as `&'static mut`.
- The application core is cooperative and single-core; multi-core is used where a chip offers it,
  never required.
- Hardware support is added by implementing the relevant seam (a `DisplayDriver`, an `ImuDriver`, a
  `PowerSource`, a codec over `I2cBus`, a port's hal impls, …) — not by special-casing above it.
  Specific drivers in the spec are reference/example implementations of a general contract.

## When code and spec disagree

Divergence is a defect. Either fix the code to match the spec, or change the spec deliberately
(use the `record-in-spec` skill) and say why. Never silently leave them inconsistent — flag it to the
user.
