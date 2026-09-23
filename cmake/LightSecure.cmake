#   light_partition_table(<name> LAYOUT <json> [SIGN <key>])
#   light_seal_image(<target> [SIGN <key>] [FAMILY <id>] [ROLLBACK <n>] [ENCRYPT <aes_key> IV <iv_salt>])
#
#   Image authenticity for the chips whose boot ROM verifies it (see documents/11). A build
# produces two signed artefacts: the device's PARTITION MAP, which says where images may be
# written and which download families reach which partition, and the IMAGE itself, stamped with
# the version the release is derived from. The ROM checks both against a public key whose hash is
# held in one-time memory; nothing in flash is trusted to verify anything else in flash.
#
#   THE A/B PAIR IS TWO SLOTS FOR ONE IMAGE. An update is written to whichever slot is not
# running and selected on the next boot, so a failed write cannot damage the image it replaces.
# Both slots hold the SAME image, linked to the same address: the ROM maps the chosen partition to
# the start of the execute-in-place window, so neither slot needs its own build.
#
#   SIGNING IS NOT A RELEASE-ONLY STEP. Every build signs, with the development key unless the
# build is told otherwise (LIGHT_SIGNING_KEY), because a path exercised only at release is a path
# discovered at release. The development key is public and protects nothing -- see keys/README.md.

set(LIGHT_SECURE_DIR "${CMAKE_CURRENT_LIST_DIR}" CACHE INTERNAL "light secure-boot helpers")
set(LIGHT_SIGNING_KEY "${CMAKE_CURRENT_LIST_DIR}/../keys/dev.pem" CACHE FILEPATH
        "the key images and partition tables are signed with; the development key by default")

#   the image version the ROM compares between slots, taken from the version the build already
# derives from the repository so a release cannot ship a version its own metadata contradicts
#   VERSION beats the derived one, which beats nothing at all. A device with two slots picks
# between them BY VERSION, so an image with no version is one the ROM cannot prefer: every build
# stamping 0.0 makes the choice of slot arbitrary and rollback protection meaningless. The
# fallback keeps a board building, but it says so rather than going quietly.
function(_light_image_version given out_major out_minor)
        set(_v "${given}")
        if(NOT _v)
                set(_v "${LIGHT_VERSION_STRING}")
        endif()
        if(_v MATCHES "^v?([0-9]+)\\.([0-9]+)")
                set(${out_major} "${CMAKE_MATCH_1}" PARENT_SCOPE)
                set(${out_minor} "${CMAKE_MATCH_2}" PARENT_SCOPE)
        else()
                if(NOT LIGHT_IMAGE_VERSION_WARNED)
                        message(WARNING "no image version available: images are stamped 0.0, so the boot ROM cannot prefer one slot over the other, and rollback protection has nothing to count. Pass VERSION to light_seal_image(), or give this project a version to derive.")
                        set(LIGHT_IMAGE_VERSION_WARNED TRUE CACHE INTERNAL "")
                endif()
                set(${out_major} 0 PARENT_SCOPE)
                set(${out_minor} 0 PARENT_SCOPE)
        endif()
endfunction()

function(light_partition_table NAME)
        cmake_parse_arguments(P "" "LAYOUT;SIGN;FOR" "" ${ARGN})
        if(NOT DEFINED P_LAYOUT)
                message(FATAL_ERROR "light_partition_table(${NAME}) needs LAYOUT <json>")
        endif()
        if(NOT DEFINED P_SIGN)
                set(P_SIGN "${LIGHT_SIGNING_KEY}")
        endif()
        get_filename_component(layout_abs "${P_LAYOUT}" ABSOLUTE)
        set(pt "${CMAKE_CURRENT_BINARY_DIR}/${NAME}.uf2")
        #   --singleton: this table IS the device's map, not one of several a loader might choose
        # between, which is what lets the ROM trust it as the only description of the flash
        #   picotool is an IMPORTED target, so naming it in DEPENDS asks ninja for a FILE it has
        # no rule to make; the dependency has to be on the target that builds it
        add_custom_command(
                OUTPUT "${pt}"
                COMMAND picotool partition create --singleton --sign "${P_SIGN}" "${layout_abs}" "${pt}"
                DEPENDS "${layout_abs}" "${P_SIGN}" ${picotool_BUILD_TARGET}
                COMMENT "partition table ${NAME} (signed) -> UF2"
                VERBATIM
        )
        add_custom_target(${NAME} ALL DEPENDS "${pt}")
        set_property(TARGET ${NAME} PROPERTY LIGHT_PARTITION_TABLE "${pt}")
        #   FOR ties the map to the firmware it describes, so asking for that target produces
        # both: a device needs the map and the image together, and a build that yields only the
        # image invites flashing one without the other
        if(DEFINED P_FOR)
                add_dependencies(${P_FOR} ${NAME})
        endif()
endfunction()

function(light_seal_image TARGET)
        cmake_parse_arguments(S "" "SIGN;FAMILY;ROLLBACK;ENCRYPT;IV;VERSION" "" ${ARGN})
        if(NOT TARGET ${TARGET})
                message(FATAL_ERROR "light_seal_image(${TARGET}): no such target")
        endif()
        if(NOT DEFINED S_SIGN)
                set(S_SIGN "${LIGHT_SIGNING_KEY}")
        endif()
        if(NOT DEFINED S_FAMILY)
                set(S_FAMILY "rp2350-arm-s")
        endif()
        _light_image_version("${S_VERSION}" major minor)

        set(sealed "$<TARGET_FILE_DIR:${TARGET}>/${TARGET}-signed.elf")
        set(signed_uf2 "$<TARGET_FILE_DIR:${TARGET}>/${TARGET}-signed.uf2")
        set(version_args --major ${major} --minor ${minor})
        if(DEFINED S_ROLLBACK)
                #   the oldest version this image will let the device accept afterwards: an image
                # whose flaw is fixed cannot be presented again once a newer one has been bought
                list(APPEND version_args --rollback ${S_ROLLBACK})
        endif()

        if(DEFINED S_ENCRYPT)
                #   confidentiality as well as authenticity: the application asked for it, and pays
                # for it in RAM -- an encrypted image is decrypted into RAM and runs from there, so
                # it spends its own size twice on a board whose RAM is already committed
                if(NOT DEFINED S_IV)
                        message(FATAL_ERROR "light_seal_image(${TARGET}): ENCRYPT needs IV <iv_salt>")
                endif()
                add_custom_command(TARGET ${TARGET} POST_BUILD
                        COMMAND picotool encrypt --hash --sign "$<TARGET_FILE:${TARGET}>" "${sealed}"
                                "${S_ENCRYPT}" "${S_IV}" "${S_SIGN}" ${version_args}
                        COMMENT "sealing ${TARGET}: encrypted, signed ${major}.${minor}"
                        VERBATIM)
        else()
                #   seal writes the same file type it reads, so the signature lands on an ELF and
                # the loadable UF2 is made from the sealed ELF rather than the bare one
                add_custom_command(TARGET ${TARGET} POST_BUILD
                        COMMAND picotool seal --sign "$<TARGET_FILE:${TARGET}>" "${sealed}" "${S_SIGN}" ${version_args}
                        COMMENT "sealing ${TARGET}: signed ${major}.${minor}"
                        VERBATIM)
        endif()
        add_custom_command(TARGET ${TARGET} POST_BUILD
                COMMAND picotool uf2 convert "${sealed}" "${signed_uf2}" --family ${S_FAMILY}
                COMMENT "signed UF2 for ${TARGET} (family ${S_FAMILY})"
                VERBATIM)
endfunction()
