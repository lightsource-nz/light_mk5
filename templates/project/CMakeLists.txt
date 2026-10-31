cmake_minimum_required(VERSION 3.25)

#   __PROJECT__ is built against the light framework. The framework supplies the C shell, the
# portable crates, the ports, the asset compilers and the shared script layer; this repository
# supplies the application, the boards it runs on, and its own version.
#
#   LIGHT_PATH points at the framework checkout: the environment (the script layer sets it), the
# cache, or the location this project was created against.
if(DEFINED ENV{LIGHT_PATH} AND (NOT LIGHT_PATH))
        set(LIGHT_PATH "$ENV{LIGHT_PATH}" CACHE PATH "path to the light framework")
endif()
if(DEFINED ENV{PICO_SDK_PATH} AND (NOT PICO_SDK_PATH))
        set(PICO_SDK_PATH "$ENV{PICO_SDK_PATH}" CACHE PATH "path to the pico-sdk checkout")
endif()
if(NOT LIGHT_PATH)
        set(LIGHT_PATH "__LIGHT__" CACHE PATH "path to the light framework")
endif()
get_filename_component(LIGHT_PATH "${LIGHT_PATH}" REALPATH BASE_DIR "${CMAKE_CURRENT_LIST_DIR}")
if(NOT EXISTS "${LIGHT_PATH}/light_init.cmake")
        message(FATAL_ERROR
                "could not find the light framework at '${LIGHT_PATH}'.\n"
                "Set LIGHT_PATH to the light_mk5 checkout.")
endif()

#   a board of this project's own goes here, before the preinit, which is where the board name
# is first resolved: its header's directory, and (for an RP2350 part) its name --
#   list(APPEND PICO_BOARD_HEADER_DIRS "${CMAKE_CURRENT_LIST_DIR}/boards")
#   list(APPEND LIGHT_RP2350_BOARDS myboard)
include(${LIGHT_PATH}/external/light_preinit.cmake)

#   this project's own version: what its images are stamped with
project(__PROJECT__ VERSION 0.1.0 LANGUAGES C CXX ASM)

if(LIGHT_PLATFORM STREQUAL "TARGET")
        if(LIGHT_SYSTEM STREQUAL "PICO_SDK")
                pico_sdk_init()
        endif()

        #   rustup installs to ~/.cargo/bin without touching PATH, so a plain `cmake --preset`
        # still finds cargo
        list(APPEND CMAKE_PROGRAM_PATH "$ENV{USERPROFILE}/.cargo/bin" "$ENV{HOME}/.cargo/bin")

        include(FetchContent)
        FetchContent_Declare(Corrosion
                GIT_REPOSITORY https://github.com/corrosion-rs/corrosion.git
                GIT_TAG v0.6.1
                GIT_SHALLOW TRUE
        )
        FetchContent_MakeAvailable(Corrosion)

        #   one staticlib per firmware target, from this project's firmware workspace; only the
        # one the configured board's executable links is built. Add each new target's crate here
        corrosion_import_crate(
                MANIFEST_PATH ${CMAKE_CURRENT_LIST_DIR}/firmware/Cargo.toml
                CRATES light_app___PROJECT___pico2
                NO_STD
        )

        #   THE ASSET COMPILERS, for when a target embeds a font, a theme or a UI design. crush is
        # the framework's and is built for the host inside this cross build, which takes a few
        # minutes the first time -- so it stays off until something needs it. Uncomment with the
        # first light_add_font / light_add_theme / light_add_ui call:
        #
        # corrosion_import_crate(MANIFEST_PATH ${LIGHT_PATH}/Cargo.toml CRATES crush)
        # corrosion_set_hostbuild(crush)
        # include(${LIGHT_PATH}/cmake/LightFont.cmake)
        # include(${LIGHT_PATH}/cmake/LightTheme.cmake)
        # include(${LIGHT_PATH}/cmake/LightUi.cmake)

        if(LIGHT_SYSTEM STREQUAL "PICO_SDK")
                #   the C shell is the framework's, built here into this tree
                add_subdirectory(${LIGHT_PATH}/module/light_shell light_shell)
                #   one firmware per tree, chosen by the board the tree was configured for
                if(PICO_BOARD STREQUAL "pico2")
                        add_subdirectory(module/__PROJECT___pico2)
                else()
                        message(FATAL_ERROR "no __PROJECT__ firmware for the board '${PICO_BOARD}'")
                endif()
        endif()
endif()

#   the host tree: one ctest that is `cargo test` over the portable workspace, so scripts/test.ps1
# and CI reach the application's tests the same way every project's are reached
if(LIGHT_PLATFORM STREQUAL "HOST")
        include(CTest)
        find_program(LIGHT_CARGO cargo
                PATHS "$ENV{USERPROFILE}/.cargo/bin" "$ENV{HOME}/.cargo/bin")
        if(NOT LIGHT_CARGO)
                message(FATAL_ERROR "cargo not found -- install rustup")
        endif()
        add_test(NAME cargo_test
                COMMAND ${LIGHT_CARGO} test --workspace
                WORKING_DIRECTORY ${CMAKE_CURRENT_LIST_DIR}
        )
endif()
