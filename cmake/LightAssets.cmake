#   light_add_asset_pack(<name>
#           ENTRIES <entry>=<asset target> ...
#           CRATE <rust crate> ENV <VAR>
#           [FAMILY <family>])
#
#   Gathers compiled assets into ONE PACK the device reads from storage, instead of embedding each
# of them in the firmware image. The assets are the same blobs light_add_font, light_add_theme and
# light_add_ui already produce -- called without CRATE/ENV they compile the blob and stop, and this
# collects them.
#
#   WHY A PRODUCT WOULD: assets are the part of a product most likely to change and least likely to
# need the scrutiny firmware gets. Kept apart they can be replaced without rebuilding, re-signing
# and re-shipping the application, they stop crowding the slot the application has to fit in, and
# on a device with an A/B pair they are not written twice. Kept apart they are also no longer
# covered by whatever verified the image, which is what the digest below is for.
#
#   THE DIGEST IS THE JOINT. crush writes the pack and, beside it, the SHA-256 that identifies it;
# the application is built with that digest (ENV names the variable holding the file, which the
# crate reads with `include_bytes!`) and checks the pack against it at startup. So the pack is
# covered by the image's own signature at one remove: substituting assets means substituting a
# digest inside a signed image. A device whose pack is missing, half-written, left over from
# another product or edited says so and stops -- an interface with no font is not an interface.
#
#   FAMILY, where the platform routes a download by family, packages the pack for delivery to the
# partition that accepts that family, so writing assets and writing firmware are the same gesture
# with different files.

include_guard(GLOBAL)

#   Shared by the three asset helpers: a blob is either embedded in a crate or gathered into a
# pack, and half an answer is a blob that silently goes nowhere.
function(light_asset_destination CALLER NAME CRATE ENV)
        if(CRATE AND NOT ENV)
                message(FATAL_ERROR "${CALLER}(${NAME}): CRATE without ENV -- name the environment variable the crate reads the blob through")
        endif()
        if(ENV AND NOT CRATE)
                message(FATAL_ERROR "${CALLER}(${NAME}): ENV without CRATE -- name the crate the variable is set for")
        endif()
        if(NOT TARGET crush)
                message(FATAL_ERROR "${CALLER}(${NAME}) needs the crush target: import the crush crate with corrosion_set_hostbuild first")
        endif()
endfunction()

#   Record where an asset target's blob landed, and embed it if this one is embedded. The property
# is how light_add_asset_pack() finds the blob without every caller repeating the path.
function(light_asset_declare NAME BLOB CRATE ENV)
        set_target_properties(${NAME} PROPERTIES LIGHT_ASSET_BLOB "${BLOB}")
        if(CRATE)
                corrosion_set_env_vars(${CRATE} "${ENV}=${BLOB}")
                add_dependencies(cargo-prebuild_${CRATE} ${NAME})
        endif()
endfunction()

function(light_add_asset_pack NAME)
        cmake_parse_arguments(P "" "CRATE;ENV;FAMILY" "ENTRIES" ${ARGN})
        foreach(req ENTRIES CRATE ENV)
                if(NOT DEFINED P_${req})
                        message(FATAL_ERROR "light_add_asset_pack(${NAME}) needs ${req}")
                endif()
        endforeach()
        if(NOT TARGET crush)
                message(FATAL_ERROR "light_add_asset_pack(${NAME}) needs the crush target: import the crush crate with corrosion_set_hostbuild first")
        endif()

        set(entry_args)
        set(blobs)
        foreach(entry IN LISTS P_ENTRIES)
                if(NOT entry MATCHES "^([^=]+)=(.+)$")
                        message(FATAL_ERROR "light_add_asset_pack(${NAME}): entry '${entry}' is not <name>=<asset target>")
                endif()
                set(entry_name "${CMAKE_MATCH_1}")
                set(entry_target "${CMAKE_MATCH_2}")
                if(NOT TARGET ${entry_target})
                        message(FATAL_ERROR "light_add_asset_pack(${NAME}): '${entry_target}' is not a target -- declare the asset before the pack that gathers it")
                endif()
                get_target_property(blob ${entry_target} LIGHT_ASSET_BLOB)
                if(NOT blob)
                        message(FATAL_ERROR "light_add_asset_pack(${NAME}): '${entry_target}' is not an asset target (no blob to gather)")
                endif()
                list(APPEND entry_args --entry "${entry_name}=${blob}")
                list(APPEND blobs "${blob}")
        endforeach()

        set(lap "${CMAKE_CURRENT_BINARY_DIR}/${NAME}.lap")
        set(digest "${CMAKE_CURRENT_BINARY_DIR}/${NAME}.sha256")
        add_custom_command(
                OUTPUT "${lap}" "${digest}"
                COMMAND $<TARGET_FILE:crush> pack build "${lap}" ${entry_args} --digest "${digest}"
                DEPENDS crush ${blobs}
                COMMENT "crush: pack ${NAME} -> LAP"
                VERBATIM
        )
        set(outputs "${lap}" "${digest}")

        if(DEFINED P_FAMILY)
                pico_init_picotool()
                set(uf2 "${CMAKE_CURRENT_BINARY_DIR}/${NAME}.uf2")
                #   the pack is raw bytes, so the conversion is told so and told where the
                # partition's contents begin. A family the map routes means the download lands in
                # the data partition rather than over an image
                add_custom_command(
                        OUTPUT "${uf2}"
                        COMMAND picotool uf2 convert "${lap}" -t bin "${uf2}" -o 0x10000000 --family ${P_FAMILY}
                        DEPENDS "${lap}" ${picotool_BUILD_TARGET}
                        COMMENT "picotool: pack ${NAME} -> UF2 (family ${P_FAMILY})"
                        VERBATIM
                )
                list(APPEND outputs "${uf2}")
        endif()

        #   ALL, because nothing links the pack: a build that produced an image and no assets to go
        # with it has produced half a device
        add_custom_target(${NAME} ALL DEPENDS ${outputs})
        #   the crate is built with the digest, not the pack -- that is the whole point
        corrosion_set_env_vars(${P_CRATE} "${P_ENV}=${digest}")
        add_dependencies(cargo-prebuild_${P_CRATE} ${NAME})
endfunction()
