#   light_add_theme(<name> [THEME <file.json>] [MONO] [CRATE <rust crate> ENV <VAR>])
#
#   Without CRATE/ENV the blob is compiled but not embedded, for light_add_asset_pack() to gather
# into a pack the device reads from storage.
#
#   Compiles a JSON look-and-feel with crush into an LTH blob and hands its path to a Rust
# crate as an environment variable, for `include_bytes!(env!("<VAR>"))` -- the same
# assets-as-data arrangement as light_add_font, sharing its ordering trick: the crate's
# cargo-prebuild target depends on the compile, and cargo tracks the blob through
# include_bytes!, so editing the theme recompiles it and rebuilds the crate. Restyling an
# interface is a data change: a theme file and this one call, no UI source touched.
#
#   THEME is optional: without it the board takes the FRAMEWORK'S DEFAULT -- steel, or
# mono under the MONO flag, which a board declares when its panel has no color (steel's
# focused fill collapses illegibly at 1 bpp). A board that wants more than the default
# provides a THEME file extending `"default"` -- the alias crush resolves to the same
# per-board default -- and overrides what it must, e.g. a measured screen_radius.
#
#   A theme may also `"extends"` a framework theme by name; those live in themes/ at the
# repo root, and the compile depends on every file there so editing a base rebuilds every
# consumer. (The dependency list is globbed at configure time: a brand-new base file wants
# one reconfigure before edits to it retrigger builds.)

include(${CMAKE_CURRENT_LIST_DIR}/LightAssets.cmake)

#   captured at include time, when CMAKE_CURRENT_LIST_DIR is this file's directory
set(LIGHT_THEMES_DIR "${CMAKE_CURRENT_LIST_DIR}/../themes" CACHE INTERNAL "framework theme directory")

function(light_add_theme NAME)
        set(opts MONO)
        set(one THEME CRATE ENV)
        cmake_parse_arguments(T "${opts}" "${one}" "" ${ARGN})
        light_asset_destination(light_add_theme ${NAME} "${T_CRATE}" "${T_ENV}")

        if(T_MONO)
                set(default_theme mono)
        else()
                set(default_theme steel)
        endif()
        if(NOT DEFINED T_THEME)
                set(T_THEME "${LIGHT_THEMES_DIR}/${default_theme}.json")
        endif()
        get_filename_component(theme_abs "${T_THEME}" ABSOLUTE)
        file(GLOB base_themes "${LIGHT_THEMES_DIR}/*.json")
        set(lth "${CMAKE_CURRENT_BINARY_DIR}/${NAME}.lth")
        add_custom_command(
                OUTPUT "${lth}"
                COMMAND $<TARGET_FILE:crush> theme compile "${theme_abs}" "${lth}" --themes "${LIGHT_THEMES_DIR}" --default "${default_theme}"
                DEPENDS crush "${theme_abs}" ${base_themes}
                COMMENT "crush: theme ${NAME} -> LTH"
                VERBATIM
        )
        add_custom_target(${NAME} DEPENDS "${lth}")
        light_asset_declare(${NAME} "${lth}" "${T_CRATE}" "${T_ENV}")
endfunction()
