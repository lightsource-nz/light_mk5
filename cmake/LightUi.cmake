#   light_add_ui(<name> UI <design.json> [CRATE <rust crate> ENV <VAR>])
#
#   Compiles a JSON UI design with crush into an LUI blob and hands its path to a Rust crate as an
# environment variable, for `include_bytes!(env!("<VAR>"))` -- the same assets-as-data arrangement
# as light_add_font and light_add_theme, sharing their ordering trick: the crate's
# cargo-prebuild target depends on the compile, and cargo tracks the blob through include_bytes!, so
# editing the design recompiles it and rebuilds the crate. A UI is data: a design file and this one
# call, no firmware source touched. light-ui reads the blob with its `lui` module.
#
#   WITHOUT CRATE/ENV the blob is compiled but not embedded: the target records where it landed,
# and light_add_asset_pack() gathers it into a pack the device reads from storage instead. Which
# of the two a board uses is the board's choice; the blob is the same either way.
#
#   A design may `"extends"` a parent design and override it -- named either by the CRATE whose
# design.json is the parent (found under crates/), or by a relative path to the parent file. So one
# shared design takes per-board overrides (device size, titles, touch-target metrics) the way a theme
# extends a base. The compile depends on every crates/*/design.json, so editing a parent rebuilds
# every consumer. (The dependency list is globbed at configure time: a brand-new parent design wants
# one reconfigure before edits to it retrigger builds.)

include(${CMAKE_CURRENT_LIST_DIR}/LightAssets.cmake)

#   captured at include time, when CMAKE_CURRENT_LIST_DIR is this file's directory
set(LIGHT_CRATES_DIR "${CMAKE_CURRENT_LIST_DIR}/../crates" CACHE INTERNAL "framework crates directory")

function(light_add_ui NAME)
        set(one UI CRATE ENV)
        cmake_parse_arguments(U "" "${one}" "" ${ARGN})
        if(NOT DEFINED U_UI)
                message(FATAL_ERROR "light_add_ui(${NAME}) needs UI")
        endif()
        light_asset_destination(light_add_ui ${NAME} "${U_CRATE}" "${U_ENV}")
        if(NOT TARGET crush)
                message(FATAL_ERROR "light_add_ui(${NAME}) needs the crush target: import the crush crate with corrosion_set_hostbuild first")
        endif()

        get_filename_component(design_abs "${U_UI}" ABSOLUTE)
        file(GLOB parent_designs "${LIGHT_CRATES_DIR}/*/design.json")
        set(lui "${CMAKE_CURRENT_BINARY_DIR}/${NAME}.lui")
        add_custom_command(
                OUTPUT "${lui}"
                COMMAND $<TARGET_FILE:crush> ui compile "${design_abs}" "${lui}" --crates "${LIGHT_CRATES_DIR}"
                DEPENDS crush "${design_abs}" ${parent_designs}
                COMMENT "crush: ui ${NAME} -> LUI"
                VERBATIM
        )
        add_custom_target(${NAME} DEPENDS "${lui}")
        light_asset_declare(${NAME} "${lui}" "${U_CRATE}" "${U_ENV}")
endfunction()
