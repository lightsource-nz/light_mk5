# 06 — Running without hardware

Most of an application can be built, run and verified with no board on the desk. This chapter
covers the three ways: the **host tests**, which pin down one module's behaviour at a time; the
**simulated board**, which runs the whole application as a desktop process with a person at the
controls; and the **desktop preview and editor**, which render an interface exactly as the panel
would.

Specification: [01-core-runtime.md](../documents/01-core-runtime.md) (the host-test model) and
[08-assets-and-tooling.md](../documents/08-assets-and-tooling.md) (`light-sim`, `light-host-gui`,
`light-ui-editor`).

## Why it works

Nothing in a portable crate touches hardware. Every driver, every module and every application
crate reaches the world through the traits in `light_core::hal`, and a port implements those
traits for a chip. On the host, something else implements them: a mock that records what was
written, a clock a test advances by hand, a byte vector standing in for a card, a decoded model of
a display controller. The portable code is byte-for-byte the same.

This is structural, not a convention. The ports live in a separate cargo workspace
(`firmware/Cargo.toml`) that the host build never sees, so a portable crate *cannot* name a pin
and *cannot* fail to be host-testable.

## The host tests

```
scripts/test.ps1                                   # everything: the ctest that is `cargo test --workspace`
cargo test -p light-ui                             # one crate, from the repository root
cargo test -p light_app_ui_demo some_test_name     # one test
```

`cargo test --workspace` at the repository root runs every framework crate's tests and every
portable application crate's, excluding only the two desktop GUI tools (which have no tests and
pull in a windowing stack). There is no list of ports to exclude, and adding a board never touches
the test command.

What the suites cover, so you know what to rely on:

- **`light-core`** — the runtime (load order, cycles, capacity, a failed load unloading what
  loaded), the bus (every subscriber sees every event in order; refusal when full; the lock-free
  empty case; thousands of events across threads), the log (ordering, truncation, drop counting,
  concurrent producers), the CLI grammar.
- **`light-draw`** — rotation equivalence (the same shape drawn under all four rotations covers
  the same pixels), band rendering byte-identical to whole-frame rendering, clipping.
- **`light-display`** — the chunk model against a fake panel, the frame layer's region tracking,
  the single-buffer page slide byte-identical to the double-buffered one at every step.
- **`light-ui`** — layouts, scrolling, the tap-versus-drag state machine, page transitions, LUI
  and LTH parsing.
- **`light-input`**, **`light-fs`**, **`light-power`**, **`light-power-manager`**,
  **`light-midi`**, **`light-wireless`** — each driver and policy against a mocked bus, clock or
  mechanism.
- **`crush-core`** — every compiler, including the design `extends` merge and its inverse.

### Writing one

A test supplies the traits a module is generic over. The power manager's suite is a compact
model:

```rust
#[cfg(test)]
mod tests {
        use super::*;
        extern crate std;
        use core::cell::Cell;

        struct MockClock(Cell<u64>);
        impl MockClock {
                fn advance_us(&self, us: u64) { self.0.set(self.0.get() + us); }
        }
        impl Clock for MockClock {
                fn now_us(&self) -> u64 { self.0.get() }
        }

        #[derive(Default)]
        struct MockMech { external: Cell<bool>, backlight: Cell<u16>, powered_off: Cell<bool> }
        impl PowerMechanism for MockMech {
                fn set_backlight(&mut self, level: u16) { self.backlight.set(level); }
                fn on_external_power(&self) -> bool { self.external.get() }
                fn power_off(&mut self) { self.powered_off.set(true); }
                fn power_button_pressed(&self) -> bool { false }
                fn battery_mv(&mut self) -> Option<u32> { Some(3900) }
        }

        #[test]
        fn dims_after_the_idle_and_wakes_on_activity() {
                let mut p = PowerManager::new(MockMech::default(), MockClock(Cell::new(0)));
                // … advance the clock past the idle interval, poll, assert on backlight …
        }
}
```

Patterns worth copying:

- **`Cell` inside the mock**, so a method taking `&self` can still be scripted from the test
  after the module has taken ownership of the mock.
- **A hand-advanced clock.** Time is an input; a test that sleeps is slow and flaky, a test that
  advances a counter is neither.
- **Drive through the bus.** Publish the event a console line would have published, poll, and
  assert on what the mocks recorded. Every input path ends as the same event, so one test covers
  all of them.
- **A bus per test**: `Box::leak(Box::new(EventBus::new()))` gives the `'static` a module wants
  without sharing a static across the test binary.
- **`extern crate std;`** at the top of a `no_std` crate's test module, for `Vec`, `format!`,
  threads and `Box`.

The runtime itself runs on the host too: a test can add real modules to a `Runtime`, `start` it,
and `run` it with a closure that stops after a few passes.

## The simulated board

`tools/light-sim` is a host library that implements the port seams on the desktop: a clock off
the process's time, pins as shared lines, a display controller that **decodes the real driver's
commands** into controller RAM and renders what the glass would show, a USB-MIDI bus with a hub
whose sockets instruments plug into, the console as a byte feed, and the log as a drain. With it,
the unmodified application crate — the one the firmware links — runs as a desktop process, every
module together, with a person at the controls.

It is not a model of a chip: time is the host's, the loop turns as fast as the host turns it, and
parts answer at once where real ones take microseconds. What it reproduces is the application's
logic against its seams. Module-level behaviour is still pinned down by the host tests.

### What it provides

| Item | Stands in for |
| --- | --- |
| `now_us()`, `SimClock`, `SimIdle` | the port's clock and the idle hook (a millisecond's sleep between idle passes) |
| `Line::new(high)` with `.output()` / `.input()` | one GPIO level shared between the application's pin and the window; `is_high`/`set` for the window side |
| `sh1107::Sh1107Panel::new(width, height, glass_offset)` → `(panel, PanelView)` | an SH1107 OLED over `SpiDisplayBus`; the view's `pixels()` is the picture |
| `usb::SimUsbHost::new()` → `(host, BusHandle)` | the USB-MIDI host role; `plug(port, Instrument)`, `unplug`, `play(port, packet)`, `take_sent(port)` on the handle |
| `start_log_drain(echo) -> LogView` | the console core's log drain; lines, and a tick count as the console core's pulse |
| `start_stdin(push)`, `send_line(push, line)` | console input from a terminal or a window |

The panel is told its **glass offset** — the controller RAM row the glass is bonded at — so a
firmware that configures the right display offset shows the right picture, and one that gets it
wrong shows it wrapped, as the real panel would.

### Putting a window on it

The library has no window in it. A project's own simulator binary (a `std` crate in the
project's portable workspace, excluded from its test run) does three things:

1. **Compiles the assets at start-up** with `crush-core`, the same code the firmware build runs:
   rasterise the font, compile the theme and the design.
2. **Runs the application on a thread**: construct the simulated parts, build the same modules
   the firmware builds over them, and call the application's run function — it never returns.
3. **Draws the window** with `light-host-gui` (an `eframe`/`egui` window): the glass from the
   panel view, the indicator from its line, a hold-to-press button, the hub's sockets with
   instruments to plug in and notes to play, the console with the log and an input line.

A project launches it with a `scripts/sim.ps1` wrapper around `cargo run -p <project>-sim`.
Console lines can also be piped in on standard input, and the log is echoed to standard output,
so a script can drive the simulated board the way `console.ps1 -Send` drives a real one.

The USB-MIDI forwarder project that first used it is the reference for the shape; the framework's
own simulator surface is documented in [08-assets-and-tooling.md](../documents/08-assets-and-tooling.md).

## The desktop preview and editor

**`light-host-gui`** drives the framework's one hardware-facing render seam — the
`DisplayDriver` trait — into a desktop window. Its `NullDriver` pushes nothing: the `Display`
renders into its own buffer, and the window shows that buffer. So a `light-ui` interface previews
through the identical `light-ui`/`light-draw`/`light-display` path the firmware uses, with
nothing re-implemented on the host.

**`light-ui-editor`** is the design editor built on it, opened with `scripts/edit-ui.ps1 -Crate
<app crate>`. The device preview in the centre is pixel-faithful because it *is* the firmware's
renderer; the chrome around it is ordinary desktop controls. [Chapter 04](04-the-interface-as-data.md)
describes its modes. Both are `cargo run -p` targets with no tests, excluded from `scripts/test.ps1`.
