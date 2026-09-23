#   light_add_bootloader(<name> LAYOUT <json> [SIGN <key>])
#
#   Build the framework's bootloader with a product's flash map embedded in it. The source is the
# framework's (module/light_bootloader) and identical everywhere; what differs per device is the
# map, which is why this is a function a product calls rather than a target the framework ships
# ready-made: the map and the code that reads it are one signed artefact (see documents/11).
#
#   THE FIRST SLOT MUST START AFTER THIS IMAGE. The bootloader occupies the start of flash; a
# layout whose first partition overlaps it describes a device where the bootloader and the
# application it loads are the same bytes. The layout's first partition therefore carries an
# explicit `start` clear of it -- 64K is the conventional room.

set(LIGHT_BOOTLOADER_DIR "${CMAKE_CURRENT_LIST_DIR}/../module/light_bootloader" CACHE INTERNAL "the framework's bootloader source")

function(light_add_bootloader NAME)
        cmake_parse_arguments(BL "" "LAYOUT;SIGN" "" ${ARGN})
        if(NOT DEFINED BL_LAYOUT)
                message(FATAL_ERROR "light_add_bootloader(${NAME}) needs LAYOUT <json>: the flash map this device carries")
        endif()

        add_executable(${NAME} "${LIGHT_BOOTLOADER_DIR}/src/main.c")
        #   pico_bootrom alone: every line of it is a ROM call, and a bootloader that links more
        # than it uses is a bootloader with more to go wrong in it
        #   the runtime and the ROM surface, and no console: stdio is turned off on both
        # transports so a bootloader cannot acquire a formatter by accident. What is left after
        # the linker drops the unreferenced is a few hundred bytes of ROM calls
        target_link_libraries(${NAME} PRIVATE pico_stdlib pico_bootrom)
        pico_enable_stdio_usb(${NAME} 0)
        pico_enable_stdio_uart(${NAME} 0)

        set(sign_args)
        if(DEFINED BL_SIGN)
                set(sign_args SIGN "${BL_SIGN}")
        endif()
        #   before the extra outputs, not after: the SDK's post-processing refuses to be
        # configured once the outputs that consume it have been declared
        light_bootloader_map(${NAME} LAYOUT "${BL_LAYOUT}" ${sign_args})
        pico_add_extra_outputs(${NAME})
endfunction()
