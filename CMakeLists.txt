cmake_minimum_required(VERSION 3.25)

#   THIS IS THE C SHELL'S BUILD. CMake remains the outer driver so the shared light-*.ps1 layer,
# the presets and CI keep working; cargo is invoked from inside it by Corrosion to produce the
# Rust staticlib the firmware links. The Rust crates themselves are a plain cargo workspace and
# are tested with cargo directly on the host (see the HOST branch at the bottom).
#
#   the framework IS this repository: its preinit (external/light_preinit.cmake) resolves
# PICO_SDK_PATH / PICO_PLATFORM / PICO_BOARD from the LIGHT_* preset variables, and its presets
# supply the board configurations. LIGHT_PATH therefore defaults to this repo's own root; it is
# still overridable (env or cache) for an out-of-tree layout.
if(DEFINED ENV{LIGHT_PATH} AND (NOT LIGHT_PATH))
        set(LIGHT_PATH "$ENV{LIGHT_PATH}" CACHE PATH "path to the light framework")
endif()
if(DEFINED ENV{PICO_SDK_PATH} AND (NOT PICO_SDK_PATH))
        set(PICO_SDK_PATH "$ENV{PICO_SDK_PATH}" CACHE PATH "path to the pico-sdk checkout")
endif()
if(NOT LIGHT_PATH)
        set(LIGHT_PATH "${CMAKE_CURRENT_LIST_DIR}" CACHE PATH "path to the light framework")
endif()
get_filename_component(LIGHT_PATH "${LIGHT_PATH}" REALPATH BASE_DIR "${CMAKE_CURRENT_LIST_DIR}")
if(NOT EXISTS "${LIGHT_PATH}/light_init.cmake")
        message(FATAL_ERROR
                "could not find the light framework at '${LIGHT_PATH}'.\n"
                "It normally is this repository itself; set LIGHT_PATH only for an out-of-tree layout.")
endif()
include(${LIGHT_PATH}/external/light_preinit.cmake)

project(light_mk5 C CXX ASM)

#   every STREQUAL literal below is QUOTED. An unquoted word on the right of STREQUAL is
# dereferenced if a variable of that name exists, and pico_sdk_init() defines one called
# PICO_SDK -- so `LIGHT_SYSTEM STREQUAL PICO_SDK` was true before the SDK initialised and
# false after, and the pico trees silently configured with no executable in them
if(LIGHT_PLATFORM STREQUAL "TARGET")
        if(LIGHT_SYSTEM STREQUAL "PICO_SDK")
                pico_sdk_init()
        endif()

        #   rustup installs to ~/.cargo/bin and does not touch PATH (light-tools.ps1 adds it for
        # the script layer); this lets a plain `cmake --preset` from a shell without it still
        # find cargo. HOME is not set on Windows, hence both
        list(APPEND CMAKE_PROGRAM_PATH "$ENV{USERPROFILE}/.cargo/bin" "$ENV{HOME}/.cargo/bin")

        #   Corrosion: cargo-from-CMake. The cross target comes from the preset as
        # Rust_CARGO_TARGET -- thumbv8m.main-none-eabi, the SOFT-float ABI, because pico-sdk
        # compiles Cortex-M33 with -mfloat-abi=softfp; linking eabihf objects against it fails
        # with a VFP register argument mismatch
        include(FetchContent)
        FetchContent_Declare(Corrosion
                GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
                GIT_TAG v0.6.1
                GIT_SHALLOW TRUE
        )
        FetchContent_MakeAvailable(Corrosion)

        #   one staticlib per board application, from the FIRMWARE workspace (the ports and their
        # dependents live there, separate from the portable crates this repo root now holds); only
        # the one the executable links is built
        corrosion_import_crate(
                MANIFEST_PATH ${CMAKE_CURRENT_LIST_DIR}/firmware/Cargo.toml
                CRATES light_app_ui_demo_touch169 light_app_ui_demo_touch28 light_app_ui_demo_touch349 light_app_dictaphone_touch349 light_app_dictaphone_wide_touch349 light_app_lui_touch349 light_app_ui_demo_touch4 light_app_pico2 light_app_usb_host_probe light_app_h7 light_app_f411
                NO_STD
        )
        #   crush is a host tool the firmware build runs, so it is built for the host inside the
        # cross build -- Corrosion's hostbuild does what the C implementation this replaces did
        # with an ExternalProject for its crush, without the second CMake tree
        corrosion_import_crate(
                MANIFEST_PATH ${CMAKE_CURRENT_LIST_DIR}/Cargo.toml
                CRATES crush
        )
        corrosion_set_hostbuild(crush)
        include(${CMAKE_CURRENT_LIST_DIR}/cmake/LightFont.cmake)
        include(${CMAKE_CURRENT_LIST_DIR}/cmake/LightTheme.cmake)
        include(${CMAKE_CURRENT_LIST_DIR}/cmake/LightUi.cmake)
        include(${CMAKE_CURRENT_LIST_DIR}/cmake/LightSecure.cmake)

        if(LIGHT_SYSTEM STREQUAL "PICO_SDK")
                add_subdirectory(module/light_shell)
                #   module/<target>/ because that is where light-flash.ps1 expects <target>.uf2
                # -- the layout every sibling project shares, and worth matching rather than
                # forking the script
                #   the RP2 port crate is one source for both chips behind a feature; the
                # pico2 demo runs on either Pico and forwards it, so the tree's platform
                # settles it here. The other apps name their chip themselves: a hardware
                # module is one tangible board (crossfire_pico IS an RP2040), and the
                # touch169 app runs nowhere but the RP2350
                if(PICO_PLATFORM MATCHES "^rp2040")
                        set(_light_rp2_chip rp2040)
                else()
                        set(_light_rp2_chip rp2350)
                endif()
                corrosion_set_features(light_app_pico2 FEATURES ${_light_rp2_chip})
                if(PICO_BOARD STREQUAL "waveshare_rp2350_touch_lcd_1.69")
                        add_subdirectory(module/ui_demo_touch169)
                elseif(PICO_BOARD STREQUAL "waveshare_rp2350_touch_lcd_2.8")
                        add_subdirectory(module/ui_demo_touch28)
                elseif(PICO_BOARD STREQUAL "waveshare_rp2350_touch_lcd_3.49")
                        #   three applications, one tree: same board, same shell
                        # configuration, each its own executable and UF2 -- flash the one
                        # you mean
                        add_subdirectory(module/ui_demo_touch349)
                        add_subdirectory(module/dictaphone_touch349)
                        add_subdirectory(module/dictaphone_wide_touch349)
                        add_subdirectory(module/light_lui_touch349)
                elseif(PICO_BOARD STREQUAL "waveshare_rp2350_touch_lcd_4")
                        add_subdirectory(module/ui_demo_touch4)
                elseif(PICO_BOARD STREQUAL "pico2" OR PICO_BOARD STREQUAL "pico")
                        #   two firmwares share the Picos: the pico2 demo, and the USB
                        # host-role probe, which puts the native port in its host role so
                        # that role is built by this repository and not only by the products
                        # that use it. One per tree -- they configure the shell differently
                        # -- chosen by the preset
                        set(LIGHT_PICO2_APP "pico2" CACHE STRING "which firmware a Pico tree builds: pico2 (the demo) or usb_host_probe")
                        if(LIGHT_PICO2_APP STREQUAL "usb_host_probe")
                                #   the probe is an RP2350 firmware: the host stack this port
                                # carries exists only on that chip
                                add_subdirectory(module/usb_host_probe)
                        else()
                                add_subdirectory(module/light_pico2)
                        endif()
                endif()
        elseif(LIGHT_SYSTEM STREQUAL "CMSIS")
                #   bare CMSIS: no SDK under the shell at all. The board selects the chip's
                # shell and its application; only the H743 so far
                add_subdirectory(module/light_shell_cmsis)
                if(LIGHT_BOARD STREQUAL "mini_stm32h7")
                        add_subdirectory(module/light_h7)
                elseif(LIGHT_BOARD STREQUAL "blackpill")
                        add_subdirectory(module/light_f411)
                else()
                        message(FATAL_ERROR "no mk5 application for the CMSIS board '${LIGHT_BOARD}' (mini_stm32h7 and blackpill so far)")
                endif()
        endif()
endif()

#   the host tree exists so scripts/test.ps1 -- and CI's project-test workflow -- run the Rust
# tests through the same door as every other project: one ctest that is `cargo test` over the
# PORTABLE workspace (this repo root). The ports and their dependents live in the separate
# firmware workspace (firmware/Cargo.toml) and are never seen here, so there is no port to
# exclude -- which is why the tree is split: cargo unifies features across a workspace, and
# cortex-m's critical section (restore-state-u32) and light-rp2's (restore-state-bool) cannot
# coexist in one test run. Only the two desktop GUI tools are excluded: they are `cargo -p`
# build targets with no tests, and their windowing deps are dead weight for a headless test.
if(LIGHT_PLATFORM STREQUAL "HOST")
        include(CTest)
        find_program(LIGHT_CARGO cargo
                PATHS "$ENV{USERPROFILE}/.cargo/bin" "$ENV{HOME}/.cargo/bin")
        if(NOT LIGHT_CARGO)
                message(FATAL_ERROR "cargo not found -- install rustup (see README.md)")
        endif()
        add_test(NAME cargo_test
                COMMAND ${LIGHT_CARGO} test --workspace --exclude light-host-gui --exclude light-ui-editor
                WORKING_DIRECTORY ${CMAKE_CURRENT_LIST_DIR}
        )
endif()
