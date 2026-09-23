#   light_seal_image(<target> [SIGN <key>] [VERSION <x.y>] [ROLLBACK <n>] [ENCRYPT <aes> IV <iv>])
#   light_bootloader_map(<target> LAYOUT <json> [SIGN <key>])
#
#   Image authenticity for the chips whose boot ROM verifies it (see documents/11). The hardware
# verifies the image it finds at the start of flash against a key hash held in one-time memory;
# nothing in flash is trusted to verify anything else in flash.
#
#   THE HARDWARE VERIFIES; THE BOOTLOADER CHOOSES. Verifying an image is not the same as electing
# one of two, so a device with an A/B pair carries a small bootloader at the start of flash that
# loads the flash map, compares the slots and hands over to the better one -- itself signed, and
# verified before it runs. The map is EMBEDDED IN that bootloader rather than written beside it,
# so the map and the code that reads it are one signed artefact and a device cannot hold a map its
# bootloader disagrees with. The first slot therefore starts after the bootloader.
#
#   SIGNING IS NOT A RELEASE-ONLY STEP. Every build signs, with the development key unless the
# build is told otherwise (LIGHT_SIGNING_KEY), because a path exercised only at release is a path
# discovered at release. The development key is public and protects nothing -- see keys/README.md.
#
#   These sit on the platform SDK's own signing helpers rather than driving its tool by hand: the
# SDK already owns the order of seal/encrypt/package around the link, and doing it again here was
# a second implementation of the same sequence.

set(LIGHT_SIGNING_KEY "${CMAKE_CURRENT_LIST_DIR}/../keys/dev.pem" CACHE FILEPATH
        "the key images and bootloaders are signed with; the development key by default")

#   VERSION beats the derived one, which beats nothing at all. A device with two slots picks
# between them BY VERSION, so an image with no version is one the bootloader cannot prefer: every
# build stamping 0.0 makes the choice of slot arbitrary and rollback protection meaningless. The
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
                        message(WARNING "no image version available: images are stamped 0.0, so the bootloader cannot prefer one slot over the other, and rollback protection has nothing to count. Pass VERSION to light_seal_image(), or give this project a version to derive.")
                        set(LIGHT_IMAGE_VERSION_WARNED TRUE CACHE INTERNAL "")
                endif()
                set(${out_major} 0 PARENT_SCOPE)
                set(${out_minor} 0 PARENT_SCOPE)
        endif()
endfunction()

#   Sign an application image and stamp the version a device compares between slots. The image is
# packaged for download into a partition, which is what routes it to the inactive slot rather than
# over the bootloader.
#
#   CALL THIS BEFORE light_shell_configure(): the platform SDK refuses to configure its
# post-processing once the outputs that consume it have been declared, and that call declares
# them. The error it raises says so, but it says it about a function you did not call.
function(light_seal_image TARGET)
        cmake_parse_arguments(S "" "SIGN;VERSION;ROLLBACK;ENCRYPT;IV" "" ${ARGN})
        if(NOT TARGET ${TARGET})
                message(FATAL_ERROR "light_seal_image(${TARGET}): no such target")
        endif()
        if(NOT DEFINED S_SIGN)
                set(S_SIGN "${LIGHT_SIGNING_KEY}")
        endif()
        _light_image_version("${S_VERSION}" major minor)

        if(DEFINED S_ROLLBACK)
                #   the rollback version is the oldest a device will accept afterwards: an image
                # whose flaw is fixed cannot be presented again once a newer one has been bought
                pico_set_binary_version(${TARGET} MAJOR ${major} MINOR ${minor} ROLLBACK ${S_ROLLBACK})
        else()
                pico_set_binary_version(${TARGET} MAJOR ${major} MINOR ${minor})
        endif()
        if(DEFINED S_ENCRYPT)
                #   confidentiality as well as authenticity: the application asked for it, and pays
                # for it in RAM -- an encrypted image is decrypted into RAM and runs from there, so
                # it spends its own size on a board whose RAM is already committed
                if(NOT DEFINED S_IV)
                        message(FATAL_ERROR "light_seal_image(${TARGET}): ENCRYPT needs IV <iv_salt>")
                endif()
                pico_encrypt_binary(${TARGET} "${S_ENCRYPT}" "${S_IV}" SIGFILE "${S_SIGN}")
        else()
                pico_sign_binary(${TARGET} "${S_SIGN}")
        endif()
        #   packaged, so the download lands in the partition its family names rather than at the
        # start of flash where the bootloader lives
        pico_package_uf2_output(${TARGET})
endfunction()

#   Build a bootloader: sign it and embed the flash map it reads. The map's first slot must begin
# after this image, which is the layout's business -- an overlap is a map that describes a device
# where the bootloader and the application it loads are the same bytes.
function(light_bootloader_map TARGET)
        cmake_parse_arguments(B "" "LAYOUT;SIGN" "" ${ARGN})
        if(NOT DEFINED B_LAYOUT)
                message(FATAL_ERROR "light_bootloader_map(${TARGET}) needs LAYOUT <json>")
        endif()
        if(NOT DEFINED B_SIGN)
                set(B_SIGN "${LIGHT_SIGNING_KEY}")
        endif()
        get_filename_component(layout_abs "${B_LAYOUT}" ABSOLUTE)
        pico_sign_binary(${TARGET} "${B_SIGN}")
        pico_embed_pt_in_binary(${TARGET} "${layout_abs}")
        #   the absolute family: this image is flashed AT the start of flash, not routed into a
        # partition like the applications it goes on to load
        pico_set_uf2_family(${TARGET} "absolute")
        pico_package_uf2_output(${TARGET})
endfunction()
