# 01 — Getting started

By the end of this chapter you will have built a firmware image, put it on a board, watched it
report on its console, and run the host test suite. Everything goes through the scripts in
`scripts/`; you never need to run `cmake` or `cargo` by hand.

Specification: [10-build-and-release.md](../documents/10-build-and-release.md).

## What you need

**PowerShell 7** (`pwsh`). The script layer is PowerShell on every platform, Linux included.

**A git checkout layout.** The framework looks for the things it depends on *beside itself*, in
one directory of sibling checkouts:

```
projects/
    light_mk5/        this repository
    pico-sdk/         the pico-sdk checkout the RP2 boards build against
```

The framework's CI uses lightsource's fork of pico-sdk, which carries the board headers for the
touch boards; the stock SDK builds the plain Pico targets. If your layout differs, see
*Configuring a different layout* below.

**Toolchains.** On Windows these are unpacked under `~/tools` (or wherever `LIGHT_TOOLS_PATH`
points); on Linux they are normally distro packages already on `PATH`. The build looks under
`LIGHT_TOOLS_PATH` first and then on `PATH`.

| Tool | Used for | Notes |
| --- | --- | --- |
| CMake ≥ 3.25 and Ninja | the outer build driver | on Windows, w64devkit supplies them |
| A host C compiler | host builds, pico-sdk's own tools | w64devkit's gcc on Windows |
| Arm GNU Toolchain 14.2.rel1 (`arm-none-eabi`) | the C shell and the link | the version is pinned on purpose; CI uses the same one |
| `rustup` with a stable toolchain | every Rust crate | on Windows the **host** toolchain must be MSVC (`stable-x86_64-pc-windows-msvc`), not GNU |
| Rust cross targets | the firmware staticlib | `thumbv8m.main-none-eabi` (RP2350, Arm), `thumbv6m-none-eabi` (RP2040), `thumbv7em-none-eabihf` (STM32); `riscv32imac-unknown-none-elf` plus a RISC-V GCC for the RP2350's Hazard3 cores |
| OpenOCD (xpack 0.12.0-7) | boards flashed and debugged over SWD | `probe-rs` is an optional fast path |
| `picotool` | products with a bootloader and a flash map | reads the map out of the built bootloader |

Two things about Rust are worth knowing before anything fails. The RP2350 target is the
**soft-float** ABI (`eabi`, not `eabihf`) because pico-sdk compiles the Cortex-M33 with
`-mfloat-abi=softfp`, and a hard-float staticlib fails at link. And on Windows the GNU host
toolchain does not work under the build environment, because w64devkit's gcc is first on `PATH`
and lacks what the proc-macro crates need to link.

Install the Rust targets with:

```
rustup target add thumbv8m.main-none-eabi thumbv6m-none-eabi thumbv7em-none-eabihf
```

## Configuring a different layout

You probably do not need to. Every path has a default derived from the checkout: a dependency
named `pico-sdk` is looked for at `../pico-sdk`, and tools are looked for under `LIGHT_TOOLS_PATH`
(default `~/tools`) and then on `PATH`.

Where your machine deviates, copy `user.config.example.ps1` to `user.config.ps1` (it is
gitignored) and fill in only what differs:

```powershell
@{
        Projects = @{
                'pico-sdk' = 'D:/src/pico-sdk'
        }
        Tools = @{
                'arm-toolchain' = 'C:/tools/arm-gnu-toolchain-14.2.rel1-mingw-w64-x86_64-arm-none-eabi/bin'
                'openocd'       = 'C:/tools/xpack-openocd-0.12.0-7/bin'
        }
}
```

Precedence, highest first: an environment variable already set in your shell (`PICO_SDK_PATH`,
`LIGHT_PATH`, …), then this file, then the derived default. `LIGHT_USER_CONFIG` can point at a
config file somewhere else entirely, which is how a CI runner supplies its own.

## Building

```
scripts/build.ps1                          # the project's default target
scripts/build.ps1 -Target light_pico2      # a named target
scripts/build.ps1 -Target light_pico2 -Clean
```

A **target** is a firmware executable: the name of a directory under `module/`, which is also
the name of the `.uf2`/`.elf` it produces. Each target belongs to a **preset** (a board
configuration from `CMakePresets.json`), and each preset owns a **build tree** (`build-<board>/`).
That mapping is declared once in `scripts/project.config.ps1`, so you name only the target.

The framework's own targets:

| Target | Board | Flashes over |
| --- | --- | --- |
| `light_pico2` | a Raspberry Pi Pico 2 wearing a 64×128 SH1107 OLED board (the widget demo on two keys) | SWD |
| `ui_demo_touch169` | a 240×280 touch board (the widget demo) | BOOTSEL |
| `ui_demo_touch28` | a 240×320 touch board | BOOTSEL |
| `ui_demo_touch35`, `dictaphone_touch35` | a 320×480 touch board | BOOTSEL |
| `ui_demo_touch349`, `dictaphone_touch349`, `dictaphone_wide_touch349`, `light_lui_touch349` | a 172×640 touch board with audio, a card slot and a clock | BOOTSEL |
| `ui_demo_touch4` | a 480×480 RGB panel board | BOOTSEL |
| `usb_host_probe` | a Pico 2 with its USB port in the host role | SWD |
| `light_bootloader` | the A/B bootloader, built in the Pico 2 trees | SWD |
| `light_h7`, `light_f411` | the two STM32 boards over bare CMSIS | SWD |

Some presets build the same target differently; name the preset to pick one:

```
scripts/build.ps1 -Target light_pico2 -Preset conf-light_mk5-pico-debug        # the RP2040 Pico
scripts/build.ps1 -Target light_pico2 -Preset conf-light_mk5-pico2-release     # optimised, LTO
scripts/build.ps1 -Target light_pico2 -Preset conf-light_mk5-pico2-riscv-debug # the Hazard3 cores
```

The build checks that the tree it is about to build belongs to the preset you asked for, and
refuses rather than silently building something else. A first build configures the tree, fetches
Corrosion (the CMake-to-cargo bridge), builds `crush` (the asset compiler) for the host, and then
compiles the C shell and the Rust staticlib. Later builds are incremental.

Two linker warnings about the `FLASH` region on every RP2350 link are inherent to pico-sdk's
linker scripts; the script filters them so that real warnings stand out. `-Verbose` shows
everything.

## Flashing

```
scripts/flash.ps1 -Target ui_demo_touch169        # builds if needed, then flashes
scripts/flash.ps1 -Target light_pico2 -NoBuild
```

How an image reaches the board depends on the target's `Flash` method in
`scripts/project.config.ps1`:

- **`uf2`** — the script opens the board's console at 1200 baud, which the firmware takes as the
  signal to reboot into the BOOTSEL bootloader, then copies the UF2 to the volume that appears.
  No button pressing. A board that has panicked or halted no longer serves that reset; hold BOOT
  while plugging it in, and run the script again.
- **`swd`** — the script hands over to `debug.ps1`, which writes the image through the debug
  probe and starts it. Where a product has a bootloader, this path writes *all* of it: the
  bootloader, the application in its slot, and any asset pack in the data partition, each at the
  address the bootloader's own flash map gives it.

For SWD boards `scripts/debug.ps1 -Target <t> -ProbeRs` is the quickest flash-and-run: no
OpenOCD, no gdb, just the new image running.

## The console

Every device-role board presents a USB serial console with the same identity, so the script finds
it without being told a port:

```
scripts/console.ps1                            # capture for 30 s
scripts/console.ps1 -Seconds 120 -Out boot.log
scripts/console.ps1 -Send "stats"              # send a command, print its reply
scripts/console.ps1 -Send "help","loglevel debug"
scripts/console.ps1 -Port COM19                # with more than one board attached
```

A board whose USB port is in the host role has no USB console; its console is the UART on a
debug probe, which you name with `-Port`.

**Anything about starting up needs `-FromReset`.** A board says everything about coming up in
its first few milliseconds, and a console opened afterwards sees none of it. Where a project's
wrapper provides it, `console.ps1 -FromReset -Target <t>` resets the board over the debugger
while the console already holds the port, so the transcript begins at the board's own zero.

What you see is the log, one record per line:

```
[     0.012] info  light_app_pico2: pico2 runtime started at 150000000 Hz; type 'help' on the console
```

The bracketed number is seconds since boot; then the level, the module that logged it (Rust's
`module_path!`), and the text.

Every application answers these built-in commands:

| Command | Does |
| --- | --- |
| `help` | lists the built-ins and the application's own commands with their usage |
| `loglevel error\|warn\|info\|debug\|trace` | sets what is kept; records above the level cost nothing |
| `loop [detail\|off]` | reports the runtime loop's pass rate; `detail` also times each module |
| `quit` | shuts the runtime down cleanly (every module's `unload` runs) |

By convention every application also has `stats`, which asks each module to report its counters.

## Debugging

```
scripts/debug.ps1 -Target light_pico2                  # flash, then gdb at the prompt
scripts/debug.ps1 -Target light_pico2 -Ex 'break main' # with extra gdb commands
scripts/debug.ps1 -Target light_pico2 -ServerOnly      # just OpenOCD on :3333
scripts/debug.ps1 -Target light_pico2 -Attach          # attach without flashing
```

A `reset` from the scripts, gdb or a telnet session is a whole-chip reboot: the configurations
reset the second core through the watchdog before the stock core-0 reset, so a debugger never
leaves core 1 running an old image.

## The host tests

```
scripts/test.ps1
```

This configures a host build tree and runs `cargo test --workspace` over the portable crates:
every framework crate and every portable application crate, against mocked hardware. The ports
and the board-linked crates live in a second cargo workspace (`firmware/Cargo.toml`) and are
never part of it. [Chapter 06](06-running-without-hardware.md) says more.

## What is where

```
Cargo.toml              the PORTABLE workspace: framework crates, portable apps, host tools
firmware/Cargo.toml     the FIRMWARE workspace: ports, board-support crates, per-board crates
crates/light-*          the framework crates (light-core, light-draw, light-ui, …)
crates/light_app_*      portable application crates, and their per-board instantiation crates
crates/light-board-*    board-support crates (pins, peripherals, power mechanism)
module/<target>/        one firmware executable: CMakeLists.txt and, usually, rust/ (its staticlib)
module/light_shell      the pico-sdk C shell every RP2 firmware links
module/light_shell_cmsis   the bare-CMSIS C shell the STM32 boards link
documents/              the specification
guide/                  this guide
tools/                  host tools: crush (the asset compiler), the host GUI, the editor, the simulator
themes/                 the framework's base look-and-feel themes
cmake/                  the CMake helpers (assets, signing, presets) a project includes
scripts/                the script layer; light-*.ps1 are shared with every project
```
