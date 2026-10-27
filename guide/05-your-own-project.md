# 05 — Your own project

A product lives in its own repository and **consumes** the framework from the checkout beside it.
The framework supplies the C shell, the portable crates, the ports, the asset compilers and the
shared script layer; your repository supplies the application, the boards it runs on, and
everything that is yours alone — the version, the signing key, the flash map. This chapter is the
skeleton of such a project, file by file.

Specification: [10-build-and-release.md](../documents/10-build-and-release.md) and
[11-secure-boot-and-update.md](../documents/11-secure-boot-and-update.md).

## The layout

```
projects/
    light_mk5/                   the framework
    pico-sdk/                    the SDK
    myproject/                   yours
        CMakeLists.txt
        CMakePresets.json
        Cargo.toml               the portable workspace: the app crate, host tools
        firmware/Cargo.toml      the firmware workspace: board crates, instantiation crates
        crates/
            light_app_myproject/        the portable application (no hardware)
            light-board-myboard/        a board-support crate (if the board is yours)
        module/
            myproject_pico2/            one firmware target: CMakeLists.txt + rust/
        boards/myboard.h         a board header, if the SDK does not know the board
        keys/dev.pem             a development signing key (public, protects nothing)
        scripts/                 thin wrappers over the framework's scripts
        .github/workflows/ci.yml
```

The framework is found through `LIGHT_PATH`: the environment variable the script layer sets, or
the sibling directory `../light_mk5` by default.

## `CMakeLists.txt`

```cmake
cmake_minimum_required(VERSION 3.25)

#   find the framework: the environment, the cache, or the sibling checkout
if(DEFINED ENV{LIGHT_PATH} AND (NOT LIGHT_PATH))
        set(LIGHT_PATH "$ENV{LIGHT_PATH}" CACHE PATH "path to the light framework")
endif()
if(DEFINED ENV{PICO_SDK_PATH} AND (NOT PICO_SDK_PATH))
        set(PICO_SDK_PATH "$ENV{PICO_SDK_PATH}" CACHE PATH "path to the pico-sdk checkout")
endif()
if(NOT LIGHT_PATH)
        set(LIGHT_PATH "${CMAKE_CURRENT_LIST_DIR}/../light_mk5" CACHE PATH "path to the light framework")
endif()
get_filename_component(LIGHT_PATH "${LIGHT_PATH}" REALPATH BASE_DIR "${CMAKE_CURRENT_LIST_DIR}")
if(NOT EXISTS "${LIGHT_PATH}/light_init.cmake")
        message(FATAL_ERROR "could not find the light framework at '${LIGHT_PATH}'; set LIGHT_PATH")
endif()

#   a board of your own: where its header is, and (for an RP2350 part) that it is one.
# Both BEFORE the preinit, which is where the board name is first resolved to a platform
list(APPEND PICO_BOARD_HEADER_DIRS "${CMAKE_CURRENT_LIST_DIR}/boards")
list(APPEND LIGHT_RP2350_BOARDS myboard)
include(${LIGHT_PATH}/external/light_preinit.cmake)

project(myproject VERSION 0.1.0 LANGUAGES C CXX ASM)

if(LIGHT_PLATFORM STREQUAL "TARGET")
        if(LIGHT_SYSTEM STREQUAL "PICO_SDK")
                pico_sdk_init()
        endif()
        list(APPEND CMAKE_PROGRAM_PATH "$ENV{USERPROFILE}/.cargo/bin" "$ENV{HOME}/.cargo/bin")

        include(FetchContent)
        FetchContent_Declare(Corrosion
                GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
                GIT_TAG v0.6.1
                GIT_SHALLOW TRUE)
        FetchContent_MakeAvailable(Corrosion)

        #   one staticlib per board, from YOUR firmware workspace; only the one linked is built
        corrosion_import_crate(
                MANIFEST_PATH ${CMAKE_CURRENT_LIST_DIR}/firmware/Cargo.toml
                CRATES light_app_myproject_pico2 light_app_myproject_myboard
                NO_STD)
        #   crush is the framework's, built for the host inside this cross build
        corrosion_import_crate(MANIFEST_PATH ${LIGHT_PATH}/Cargo.toml CRATES crush)
        corrosion_set_hostbuild(crush)

        include(${LIGHT_PATH}/cmake/LightFont.cmake)
        include(${LIGHT_PATH}/cmake/LightTheme.cmake)
        include(${LIGHT_PATH}/cmake/LightUi.cmake)
        include(${LIGHT_PATH}/cmake/LightAssets.cmake)
        include(${LIGHT_PATH}/cmake/LightSecure.cmake)
        include(${LIGHT_PATH}/cmake/LightBootloader.cmake)

        if(LIGHT_SYSTEM STREQUAL "PICO_SDK")
                add_subdirectory(${LIGHT_PATH}/module/light_shell light_shell)
                if(PICO_BOARD STREQUAL "myboard")
                        add_subdirectory(module/myproject_myboard)
                else()
                        add_subdirectory(module/myproject_pico2)
                endif()
        endif()
endif()

#   the host tree: one ctest that is `cargo test` over the portable workspace
if(LIGHT_PLATFORM STREQUAL "HOST")
        include(CTest)
        find_program(LIGHT_CARGO cargo PATHS "$ENV{USERPROFILE}/.cargo/bin" "$ENV{HOME}/.cargo/bin")
        if(NOT LIGHT_CARGO)
                message(FATAL_ERROR "cargo not found -- install rustup")
        endif()
        add_test(NAME cargo_test
                COMMAND ${LIGHT_CARGO} test --workspace
                WORKING_DIRECTORY ${CMAKE_CURRENT_LIST_DIR})
endif()
```

Three things in there are decisions rather than boilerplate. **Which module is added** is chosen
by the board the tree was configured for (`PICO_BOARD`, or `PICO_PLATFORM` for the RP2040 versus
the RP2350), so one tree holds one firmware. **The C shell is the framework's**, added as a
subdirectory into your tree, so a fix to it reaches you by updating the checkout. And the project
**signs with its own key** when it seals images (below); a shared key would tie your releases to
the framework's.

A firmware module's `CMakeLists.txt` is the one from [Chapter 03](03-building-an-application.md)
with the shell's path spelt through `LIGHT_PATH`:

```cmake
add_executable(myproject_pico2 ${LIGHT_PATH}/module/light_shell/src/main.c)
target_link_libraries(myproject_pico2 PRIVATE pico_stdlib pico_multicore pico_bootrom light_app_myproject_pico2)
light_add_font(...)  light_add_theme(...)  light_add_ui(...)
light_shell_configure(myproject_pico2)
```

## `CMakePresets.json`

Your presets include the framework's and inherit a board preset plus a build-type preset:

```json
{
    "version": 6,
    "include": ["../light_mk5/cmake/light_presets/light_pico_presets.json"],
    "configurePresets": [
        {
            "name": "conf-myproject-pico2-debug",
            "inherits": ["conf-light-pico2", "conf-light-debug"],
            "binaryDir": "${sourceDir}/build-pico2",
            "generator": "Ninja",
            "cacheVariables": { "Rust_CARGO_TARGET": "thumbv8m.main-none-eabi" }
        },
        {
            "name": "conf-myproject-myboard-debug",
            "inherits": ["conf-light-pico-base", "conf-light-debug"],
            "binaryDir": "${sourceDir}/build-myboard",
            "generator": "Ninja",
            "cacheVariables": {
                "LIGHT_PLATFORM": "TARGET",
                "LIGHT_BOARD": "myboard",
                "Rust_CARGO_TARGET": "thumbv8m.main-none-eabi"
            }
        },
        {
            "name": "conf-myproject-host",
            "inherits": ["conf-light-host-debug"],
            "binaryDir": "${sourceDir}/build-host",
            "generator": "Ninja"
        }
    ]
}
```

Board presets the framework provides: `conf-light-pico`, `conf-light-pico2`, `conf-light-pico2-w`,
`conf-light-pico_w`, the `conf-light-waveshare-touch*` family, and `conf-light-pico-base` for a
board of your own named in `LIGHT_BOARD`. Build types: `conf-light-debug`, `conf-light-release`
(full optimisation and fat LTO), `conf-light-trace`. The host preset is `conf-light-host-debug`.
A release build is a separate preset with its own tree, because the two differ in more than a
flag and a shared tree would hold whichever was configured last.

`Rust_CARGO_TARGET` is the cross target: `thumbv8m.main-none-eabi` for the RP2350 (soft-float,
to match pico-sdk), `thumbv6m-none-eabi` for the RP2040, `thumbv7em-none-eabihf` for the STM32s.

## The two cargo workspaces

Your repository mirrors the framework's split. The root `Cargo.toml` is the **portable**
workspace — the application crate and any host tool, everything that tests on the host:

```toml
[workspace]
resolver = "2"
members = ["crates/light_app_myproject"]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"
```

`firmware/Cargo.toml` is the **firmware** workspace — the board-support crates and the
per-board instantiation crates, each selecting one port:

```toml
[workspace]
resolver = "2"
members = [
        "../crates/light-board-myboard",
        "../module/myproject_pico2/rust",
        "../module/myproject_myboard/rust",
]

[workspace.package]
version = "0.1.0"
edition = "2024"
license = "MIT"

[profile.dev]
panic = "abort"
opt-level = 1
debug = true

[profile.release]
panic = "abort"
opt-level = 3
debug = true
lto = "fat"
codegen-units = 1
```

Each firmware crate names that manifest with `workspace = "../../../firmware"` in its own
`Cargo.toml`, so cargo assigns it there rather than to the root. **Carry the profiles**: a
firmware workspace without them builds at opt-level 0, which is several times slower on every
hot path and twice the image, and reads exactly like misconfigured hardware.

Dependencies on the framework are by path across the checkout boundary:

```toml
light-core = { path = "../../../../light_mk5/crates/light-core" }
light-rp2 = { path = "../../../../light_mk5/crates/light-rp2", features = ["rp2350", "usb-console"] }
```

If your host tools use `crush-core`, repeat the framework's `[patch.crates-io]` entry for
`freetype-sys` in your root `Cargo.toml` (a patch applies per workspace).

## The scripts

`scripts/` holds thin wrappers that supply your defaults and call the framework's shared layer
for the logic. `light-tools.ps1` finds the framework:

```powershell
$ErrorActionPreference = 'Stop'
$LightPath = if ($env:LIGHT_PATH) { $env:LIGHT_PATH } else { Join-Path $PSScriptRoot '..' '..' 'light_mk5' }
$LightPath = (Resolve-Path $LightPath).Path
if (-not (Test-Path (Join-Path $LightPath 'light_init.cmake'))) {
        throw "could not find the light framework at '$LightPath'; set LIGHT_PATH for another layout."
}
$env:LIGHT_PATH = $LightPath
$LightScripts = Join-Path $LightPath 'scripts'
```

and each wrapper is a few lines over it — `build.ps1`:

```powershell
param([string]$Target, [string]$Preset, [switch]$Clean)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'light-tools.ps1')
if (-not $Target) { $Target = (& (Join-Path $PSScriptRoot 'project.config.ps1')).DefaultTarget }
& (Join-Path $LightScripts 'light-build.ps1') -Target $Target -Preset $Preset -Clean:$Clean -ProjectRoot $root
```

`flash.ps1`, `debug.ps1`, `console.ps1`, `test.ps1` and `edit-ui.ps1` follow the same pattern;
copy them from the framework's `scripts/` or another project's. The console wrapper is where a
project adds `-FromReset`, since knowing how to reset *its* boards is the project's business.

### `scripts/project.config.ps1`

The one file that maps names to trees:

```powershell
@{
        Name = 'myproject'

        Trees = @{
                'conf-myproject-host'          = 'build-host'
                'conf-myproject-pico2-debug'   = 'build-pico2'
                'conf-myproject-myboard-debug' = 'build-myboard'
        }

        #   Flash: 'uf2' over BOOTSEL via the console's 1200-baud reset, or 'swd' through a probe.
        #   Bootloader: the target whose image carries the flash map, where the product has one
        Targets = @{
                'myproject_pico2'   = @{ Preset = 'conf-myproject-pico2-debug'; Flash = 'swd'; Bootloader = 'myproject_bootloader' }
                'myproject_myboard' = @{ Preset = 'conf-myproject-myboard-debug'; Flash = 'uf2' }
        }

        #   the OpenOCD configurations are the framework's; Chip is probe-rs's name for the part
        Debug = @{
                'conf-myproject-pico2-debug' = @{
                        Config = '../light_mk5/openocd-rp2350.cfg'
                        Svd    = '../../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x'
                }
        }

        Test = @{ Preset = 'conf-myproject-host'; Ctest = $true }

        DefaultTarget = 'myproject_pico2'
}
```

## Your own board

A board the platform SDK does not know needs a header, `boards/myboard.h`, in pico-sdk's board
header format carrying only what the SDK consumes — the chip variant, the flash part and size,
the default UART pins, the default LED. The `CMakeLists.txt` above appends its directory to
`PICO_BOARD_HEADER_DIRS` and, for an RP2350 part, the name to `LIGHT_RP2350_BOARDS`, both before
the preinit; the preset then names it in `LIGHT_BOARD` over `conf-light-pico-base`. The pin map
is not in the header: it is the board-support crate's ([Chapter 08](08-adding-hardware.md)).

## Signed images, a bootloader and updates

A product that wants its firmware verified by the hardware, updated in the field, and able to
fall back to the previous image uses three more things, all RP2350-only:

- `light_seal_image(<target>)` in the module's `CMakeLists.txt`, before `light_shell_configure`,
  signs the image with `LIGHT_SIGNING_KEY` and stamps it with the version. Set the key to yours:
  `set(LIGHT_SIGNING_KEY "${CMAKE_CURRENT_LIST_DIR}/keys/dev.pem" CACHE FILEPATH "" FORCE)`. A
  development key is public and protects nothing; a release is signed with one that never
  appears in the repository.
- `light_add_bootloader(<name> LAYOUT <partitions.json> [SIGN <key>])` builds the small
  bootloader that carries the flash map (an A/B pair of application slots and a data partition),
  picks the slot with the newer version, verifies it, and hands over. Name it as the target's
  `Bootloader` in `project.config.ps1` and the flash scripts write the whole board: bootloader,
  application in its slot, assets in theirs, each at the address read back out of the built
  bootloader.
- `light-update`, the portable crate that stages a new image into the slot that is *not*
  running, over the port's `UpdateTarget`; and a **probation** policy in the application, which
  commits a new image only once it has proved it works, so a bad update reverts at the next reset.

The contract — what is signed, when it is verified, how probation ends — is
[11-secure-boot-and-update.md](../documents/11-secure-boot-and-update.md).

## CI

The framework publishes two **reusable workflows** that check your project out beside
`light_mk5` and `pico-sdk` — the same sibling layout a developer has — and run your own scripts,
so CI has no recipe of its own to drift:

```yaml
name: CI
on: { push: { branches: [main] }, pull_request: }
permissions: { contents: read }

jobs:
  host:
    uses: lightsource-nz/light_mk5/.github/workflows/project-test.yml@main
    with:
      project: myproject

  firmware:
    strategy:
      fail-fast: false
      matrix:
        include:
          - name: pico2
            preset: conf-myproject-pico2-debug
            targets: myproject_pico2 myproject_bootloader
            tree: build-pico2
    name: ${{ matrix.name }}
    uses: lightsource-nz/light_mk5/.github/workflows/project-build.yml@main
    with:
      project: myproject
      preset: ${{ matrix.preset }}
      targets: ${{ matrix.targets }}
      tree: ${{ matrix.tree }}
```

One preset per matrix leg; several targets of one preset share a leg because they share its
tree. The built images are kept as artifacts. A green firmware leg means the tree configures,
compiles, links, seals and signs; it does not mean the board runs.

## Versions

A project has its own version — what its images are stamped with and what a bootloader compares
between two slots — independent of the framework's. The simplest form is the `VERSION` in
`project(...)`. To derive it from git tags instead, and compile the string in,
`include(${LIGHT_PATH}/cmake/light_version.cmake)` and call `light_project_version(PREFIX MYPROJECT)`;
`light_require_project_version(LIGHT <x.y.z>)` then refuses to configure against a framework
checkout older than the one your code needs. The framework's own version is one number across
both of its workspaces, bumped by `light-release.ps1` in the commit it tags.
