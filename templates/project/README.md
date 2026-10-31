# __PROJECT__

A device built on the **Light Framework**. The framework supplies the C shell, the portable
crates, the chip ports, the asset compilers and the script layer; this repository supplies the
application, the boards it runs on, and its own version.

It starts as a working firmware for a Raspberry Pi Pico 2: the LED blinks, the BOOTSEL button
starts and stops it, and the USB serial console answers `help`, `led on|off|blink` and `stats`.

## Building and running

```
scripts/build.ps1                      # the default target, __PROJECT___pico2
scripts/flash.ps1                      # put it on the board over BOOTSEL
scripts/console.ps1 -Send "help"       # talk to it
scripts/test.ps1                       # the application's host tests
```

The framework is found through `LIGHT_PATH`, or at the location this project was created
against. A first build fetches Corrosion and compiles the C shell and the Rust staticlib; later
builds are incremental. The framework's guide, `guide/01-getting-started.md` in its checkout,
lists the toolchain.

## Where things are

| Path | What it is |
| --- | --- |
| `crates/light_app___PROJECT__/` | the application: events, modules, console commands, host tests. No hardware. |
| `module/__PROJECT___pico2/` | the Pico 2 firmware: `CMakeLists.txt` for the executable, `rust/` for the pins and the entry points |
| `Cargo.toml` | the portable workspace (the application, host tools) |
| `firmware/Cargo.toml` | the firmware workspace (crates that select a chip port) |
| `CMakeLists.txt`, `CMakePresets.json` | the build; one preset per board and build type |
| `scripts/project.config.ps1` | which preset builds which target, and how each is flashed |
| `.github/workflows/ci.yml` | host tests and a firmware build, through the framework's workflows |

## Growing it

- **Your own logic** goes in the application crate, as modules on the event bus. Test each one
  on the host by publishing the event a console line or a button would have published.
- **Another board** is another directory under `module/` with its own preset: add its crate to
  `firmware/Cargo.toml` and to `corrosion_import_crate` in `CMakeLists.txt`, its target to
  `scripts/project.config.ps1`, and a branch choosing it in `CMakeLists.txt`.
- **A display and an interface as data** — fonts, themes, UI designs — are one CMake call each;
  see the asset block in the module's `CMakeLists.txt`.
- **Signed images, a bootloader and field updates** are described in the framework's guide,
  chapter 05.

The framework's `guide/` explains all of this with worked examples; its `documents/` is the
normative specification.
