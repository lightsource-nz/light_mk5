# 07 — The scripts

Every repeated operation is a script. A project's `scripts/` holds thin **wrappers** that supply
that project's defaults (its default target, its preset-to-tree map, how to reset its boards) and
call the **shared layer** — the `light-*.ps1` scripts in the framework's `scripts/` — for the
logic. The wrappers take the same parameters as the shared script beneath them, so the commands
below are the same in every project.

All of them are PowerShell 7 and run on Windows and Linux. Where a script covers an operation,
use it rather than `cmake`, `cargo`, `openocd` or `git` directly: the scripts check things the
tools do not (that a tree belongs to the preset you named, that the volume a UF2 is copied to
has a drive letter, that a tag does not go backwards).

## The everyday wrappers

| Script | Does | Parameters |
| --- | --- | --- |
| `build.ps1` | Configures the target's tree if needed, then builds the target. Refuses a tree that belongs to another preset. | `-Target <name>` (default: the project's `DefaultTarget`), `-Preset <name>` (default: the target's), `-Clean`, `-Verbose` (show the filtered linker warnings) |
| `flash.ps1` | Builds unless told not to, then puts the image on the board: a `uf2` target over BOOTSEL via the console's 1200-baud reset; an `swd` target through `debug.ps1 -Batch`, writing every image the board's flash map calls for. | `-Target`, `-Preset`, `-NoBuild`, `-TimeoutSeconds <n>` (default 15, waiting for the BOOTSEL volume) |
| `debug.ps1` | Starts OpenOCD against the target, writes its images where the flash map puts them, reads them back, and starts gdb. | `-Target`, `-Preset`, `-NoBuild`, `-ServerOnly` (OpenOCD on :3333, no gdb), `-Attach` (no flashing), `-Ex <cmd>[,<cmd>]` (gdb commands after the standard ones; comma-separated), `-Batch` (run the commands and exit with gdb's status), `-ProbeRs` (flash and run through probe-rs; no OpenOCD, no gdb) |
| `console.ps1` | Opens the board's USB console and captures it, or sends commands and captures their replies. | `-Seconds <n>` (default 30), `-Out <file>` (streamed as it arrives), `-Until <regex>` (stop once seen), `-Send "cmd"[,"cmd"]` (each sent with Enter; the boot output is drained first), `-SettleMs <n>` (how long a reply may stay quiet, default 1500), `-Quiet`, `-Port <COMn or /dev/ttyACMn>`, `-Board <rp2|stm32h7|…>` (choose by chip family when several are attached). Project wrappers add `-FromReset -Target <t> [-Preset <p>]`: reset the board over the debugger once the console is open, so the capture begins at boot. |
| `test.ps1` | Configures the host tree if needed, builds it, and runs the project's tests under ctest (`cargo test --workspace` over the portable crates), plus any smoke commands the project declares. Fails if ctest finds no tests. | `-Preset <name>`, `-NoBuild` |
| `edit-ui.ps1` | Builds the UI editor and opens a crate's `design.json` in it. With no crate, lists the designs in the tree. | `-Crate <app crate or path to a .json>`, `-Release` |

A target is a directory name under `module/`; a preset is a name from `CMakePresets.json`; the
mapping between them, the flash method and the bootloader are in `scripts/project.config.ps1`.

## The shared layer

These live in the framework's `scripts/` and are what the wrappers call. You run them directly
only from the framework repository itself (whose wrappers are the same files) or when a project
has no wrapper for one.

| Script | Does | Parameters |
| --- | --- | --- |
| `light-build.ps1` | The logic behind `build.ps1`. | as `build.ps1`, plus `-ProjectRoot <path>` |
| `light-flash.ps1` | The logic behind `flash.ps1`. | as `flash.ps1`, plus `-ProjectRoot` |
| `light-debug.ps1` | The logic behind `debug.ps1`. | as `debug.ps1`, plus `-ProjectRoot` |
| `light-console.ps1` | The logic behind `console.ps1`. | as `console.ps1`, plus `-OnOpen <scriptblock>` (what a wrapper's `-FromReset` runs once the port is open) |
| `light-update.ps1` | Gives a running board new firmware over its console, into the slot it is not running: asks which slot it runs, announces the image's length and CRC-32 (`receive`), sends it a line at a time (`rx`), each line waiting for its answer, then follows the board through its reboot and reports whether the new image committed itself. Needs a target that boots through a bootloader, running firmware that answers `receive`. A project with an A/B product wraps it as `update.ps1`. | `-Target`, `-Preset`, `-NoBuild`, `-Image <file>` (a slot image to send instead of the target's `.bin`), `-Port`, `-Board`, `-ProjectRoot` |
| `light-test.ps1` | The logic behind `test.ps1`. | as `test.ps1`, plus `-ProjectRoot` |
| `light-edit-ui.ps1` | The logic behind `edit-ui.ps1`. | as `edit-ui.ps1`, plus `-ProjectRoot` |
| `light-new-project.ps1` | Creates a new project from `templates/project/`: copies it, substitutes the name in contents and paths, writes the paths to the framework relative to each file, and runs `git init` (nothing committed). Refuses a name that is not lower-case-and-underscores, an existing directory, and a directory on another volume. | `-Name <name>`, `-Path <parent directory>` (default: the directory holding the framework, so the project is its sibling), `-NoGit` |
| `light-configure.ps1` | Configures one preset's tree without building. Refuses a tree that belongs to another preset unless forced. | `-Preset <name>`, `-Force`, `-ProjectRoot` |
| `light-clean.ps1` | Removes a preset's build tree, or all of them. | `-Preset <name>`, `-All`, `-WhatIf` |
| `light-version.ps1` | Reports the version a checkout is working toward, derived from its git tags (`1.8.0-dev.5` is five commits past `v1.7.2`). | `-Bump minor\|patch\|major`, `-Json`, `-Path <repo>` |
| `light-release.ps1` | Mints a release: checks the tree is clean, the version moves forward and the tag is new, then creates an annotated tag. Local unless pushed. | `-Version <x.y.z>` (default: the newest tag bumped by `-Bump`), `-Bump minor\|patch\|major`, `-PreRelease <rc.1>`, `-Message <text>`, `-Push`, `-AllowDirty`, `-Path <repo>` |
| `light-coverage.ps1` | A coverage run of the host tests, under a container. | `-ProjectRoot`, `-NoHtml`, `-Distro <name>`, `-Functions <glob>`, `-Reuse` |
| `light-env.ps1` | Sets up the toolchain environment for the current shell. Dot-source it: `. $env:LIGHT_PATH/scripts/light-env.ps1`. Every other script does this itself. | `-Quiet` |
| `light-tools.ps1` | Puts `~/.cargo/bin` on `PATH` and sets `$LightScripts` for the wrappers. | — |

## The project configuration

`scripts/project.config.ps1` returns one hashtable:

| Key | Holds |
| --- | --- |
| `Name` | the project's name |
| `Trees` | preset name → build directory, relative to the project |
| `Targets` | target name → `@{ Preset; Flash = 'uf2' or 'swd'; Bootloader = <target> (optional) }` |
| `Debug` | preset name → `@{ Config = <OpenOCD .cfg>; Svd = <path> (optional); Chip = <probe-rs chip name> }` |
| `Test` | `@{ Preset; Ctest = $true; Target = <name> (optional); Smoke = @(...) (optional) }` |
| `DefaultTarget` | what `build.ps1` and `flash.ps1` do with no `-Target` |
| `Expect` | per preset, the cache values a correctly configured tree must show; used to check a tree against its preset |

## The user configuration

`user.config.ps1` (gitignored; template in `user.config.example.ps1`) is the only file allowed
to hold absolute paths, and most machines need none: dependencies are found beside the project
and tools under `LIGHT_TOOLS_PATH` (default `~/tools`) and then on `PATH`.

| Key | Holds |
| --- | --- |
| `Projects` | dependency directory name → path, where a checkout is not beside the project (`'pico-sdk' = 'D:/src/pico-sdk'`) |
| `Tools` | tool name → the directory it lives in: `w64devkit`, `arm-toolchain`, `openocd`, `openocd-scripts`, `riscv-toolchain` |

An environment variable already set in the shell (`PICO_SDK_PATH`, `LIGHT_PATH`,
`LIGHT_TOOLS_PATH`, `LIGHT_USER_CONFIG`) beats the file; the file beats the derived default.
