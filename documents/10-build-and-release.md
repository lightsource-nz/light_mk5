# Build, Flash and Release

How a firmware image is built, put on a board, tested on the host, and versioned. The defining
choice is that **CMake stays the outer build driver** and drives cargo through Corrosion — the C
shell's build owns the image; Rust is a library it links.

## Responsibility

- Produce a firmware executable from the C shell plus the Rust staticlib, for a chosen board.
- Compile the assets (fonts, themes, UI designs) as part of that build and hand them to the crate.
- Run the portable crates' tests on the host.
- Flash an image to a board and open its console.
- Keep one version across the whole workspace and tag releases.

Everything below is invoked through a thin `scripts/*.ps1` layer that wraps a shared
`$LIGHT_PATH/scripts/light-*.ps1` implementation, so the same commands work across the lightsource
projects.

## The build model — CMake over Corrosion

- CMake is the outer driver. The pico-sdk build owns `crt0`, `boot2`, the linker script, multicore
  launch, PIO and TinyUSB; the bare-CMSIS shell stands in for it on the STM32 ports. See
  [07-ports-and-shell.md](07-ports-and-shell.md).
- **Corrosion** imports the Rust crates into CMake: `corrosion_import_crate` names the per-board
  `staticlib` crates from the workspace `Cargo.toml`; only the crate the selected executable links is
  actually built. `crush` is imported as a **host** tool (`corrosion_set_hostbuild`) and built for
  the host inside the cross build, so it can compile assets during the firmware build without a
  second CMake tree.
- The top `CMakeLists.txt` selects, by `PICO_BOARD` / `LIGHT_BOARD`, which `module/<target>/`
  executable subdirectory to add. Each executable links its instantiation crate's staticlib plus the
  pico-sdk libraries it needs.

## mk5 decision — two workspaces and uniform target naming (proposal F)

*Decided for mk5.* mk4 keeps one cargo workspace, which forces two workarounds that mk5 removes.

- **Two workspaces, one version.** The single-global `critical-section` (a property of that crate, not
  a defect — see [07-ports-and-shell.md](07-ports-and-shell.md)) means the ports cannot build
  together and none build on the host, so mk4's host test is `cargo test --workspace` with a
  seventeen-entry `--exclude` list. mk5 splits the tree into a **portable workspace** (the framework
  crates, the portable application crates, and the host tools) and a **firmware workspace** (the
  ports, the board-support crates, the per-board instantiation crates, and the `module/*` executables,
  each firmware selecting one port). The portable workspace then host-tests with a plain `cargo test`
  — no excludes — and no build ever pulls two ports together. The firmware workspace depends on the
  portable crates by path across the boundary; the shared version (see *Versioning*) is coordinated
  across both by the release tooling.
- **Uniform target naming.** mk4's `_app` suffix exists because a CMake `add_executable(<name>)`
  target and a Corrosion-imported crate of the same name collide in one CMake namespace. mk5 adopts a
  uniform convention where the instantiation crate and the executable never share a stem — the crate
  is `light_app_<name>` and the executable is `<name>` (the pattern the ui_demo targets already use,
  which need no suffix) — so the collision cannot arise and the `_app` workaround is dropped.

The single-global critical-section itself stays a per-firmware property; F changes the build *around*
it, not the choice.

## The asset pipeline in the build

Three CMake helpers turn authored files into blobs and hand their paths to the linked crate as env
vars, which the crate `include_bytes!(env!("…"))`'s:

- `light_mk4_add_font(<name> FONT … DISPLAY … CRATE <crate> ENV LIGHT_FONT_LGF)` — renders a
  TrueType face to an LGF blob.
- `light_mk4_add_theme(<name> [THEME <file>] [MONO] CRATE <crate> ENV LIGHT_THEME_LTH)` — compiles a
  look-and-feel to an LTH blob; without a file it takes the framework default (steel, or mono), and
  a board theme may `extends` a base under `themes/`.
- `light_mk4_add_ui(<name> UI <design.json> CRATE <crate> ENV LIGHT_UI_LUI)` — compiles a UI design
  to an LUI blob, resolving its `extends` chain against `--crates crates/`, and depends on every
  `crates/*/design.json` so editing a parent design rebuilds its consumers.

Each helper makes the crate's `cargo-prebuild` target depend on the compile, and cargo tracks the
blob through `include_bytes!`, so editing an asset recompiles it and rebuilds the crate. A UI, a
font, or a theme is data: an authored file and one call, no firmware source touched. See
[08-assets-and-tooling.md](08-assets-and-tooling.md).

## Presets, targets and configuration

- `CMakePresets.json` supplies the board configurations (each a `conf-light_mk4-<board>-debug`
  preset). The framework's preinit resolves `PICO_SDK_PATH` / `PICO_PLATFORM` / `PICO_BOARD` from the
  preset's `LIGHT_*` variables.
- `scripts/project.config.ps1` maps each firmware **target** to its preset and its flash method
  (`uf2` over BOOTSEL, or `swd`), and names the default target. Several targets can share one board
  preset and build tree (e.g. several apps targeting one board share its preset and build tree).

## Building and flashing

- `scripts/build.ps1 [-Target <name>] [-Clean]` — builds a target through CMake/Corrosion.
- `scripts/flash.ps1 -Target <name> [-NoBuild]` — builds if needed, then flashes. For a `uf2`
  target it resets the running board into BOOTSEL over the console's 1200-baud touch and copies the
  UF2 to the mounted volume; a halted board that no longer serves the reset needs a manual BOOTSEL
  (hold BOOT, replug). `swd` targets flash over the debug probe.
- `scripts/console.ps1` / `debug.ps1` — open the board's console / a debug session. `console.ps1`
  captures output for a window (`-Seconds`, `-Until`), or drives the CLI non-interactively with
  `-Send "cmd"` (or a list) — sending each command and capturing its reply — so a script, CI, or an
  agent can read `stats` and issue commands without a terminal.

`cargo build --workspace --target thumbv8m…` does **not** work: `crush` is a `std` binary, and a
workspace-wide cross build unifies features across the two `critical-section` flavours. Build the
firmware through the scripts, or a single crate with `cargo build -p <crate> --target …`.

## Host tests

- `scripts/test.ps1` configures a host build tree and runs `cargo test` on the portable crates.
- The port crates and the per-board `staticlib` crates are **excluded** from the host test run:
  `light-rp2` needs a chip chosen, the ports carry `no_std`/target-only code, and a workspace-wide
  invocation cannot unify the cortex-m single-core `critical-section` (the STM32 ports) with
  `light-rp2`'s own. Shared app-board libraries (a board-specific app crate) are excluded for
  the same reason.
- The host suite is also run in CI through the framework's shared GitHub workflow.

## Toolchain

- `rustup` with the `thumbv8m.main-none-eabi` target — the **soft-float** ABI, to match pico-sdk's
  `-mfloat-abi=softfp` on Cortex-M33; `eabihf` fails at link with a VFP-args mismatch.
- On Windows the Rust **host** toolchain must be MSVC (`stable-x86_64-pc-windows-msvc`); the gnu
  host fails under the build environment (w64devkit's gcc has no `libgcc_eh.a`, and cargo does not
  apply target rustflags to build scripts under `--target`).
- `scripts/light-tools.ps1` puts `~/.cargo/bin` on PATH for a shell that lacks it.

## Versioning and releases

- One version for the whole workspace (`[workspace.package].version`), bumped in the commit a
  release tags. Every crate carries `license = "MIT"` and the workspace version.
- The project derives its version from git, and can require a framework version that has the
  features it uses (`light_project_version()` / `light_require_project_version()`); the floor is the
  higher of the C and CMake needs.
- `scripts/light-release.ps1` performs the version bump and the tag.

## Design decisions and constraints

- **CMake owns the image, Rust is a library.** Keeping the SDK's build as the outer driver means the
  shared script layer, the presets and CI keep working unchanged, and the same shape carries the
  bare-CMSIS STM32 ports where a small C shell stands in for the SDK.
- **Assets compile inside the firmware build**, via a host-built `crush` imported by Corrosion — no
  separate asset build, no checked-in blobs, and editing a design rebuilds exactly the firmware that
  embeds it.
- **The host test surface is the portable crates only**, by construction: the ports and the
  board-linked crates are excluded so a single `cargo test` invocation has one coherent feature set.
