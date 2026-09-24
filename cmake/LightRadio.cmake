#   light_add_radio_firmware(<name> [HEADER <vendor header>])
#
#   Turns the firmware a wireless part is uploaded at power-up into two ASSETS, so it can travel in
# the data partition beside the fonts rather than inside the firmware image.
#
#   WHY IT IS NOT SIMPLY LINKED IN. A radio of this kind holds no firmware of its own: the host
# uploads a quarter of a megabyte into its RAM every time it is powered, and a small blob of
# regulatory limits after it. The vendor ships both as one C array in a header, for a C driver to
# link into its image -- which would put a quarter of a megabyte into every slot of an A/B pair,
# twice over, and re-sign and re-ship it whenever anything else changed. As assets they are written
# once, shared by both slots, and replaced on their own.
#
#   This produces two asset targets, `<name>_image` and `<name>_limits`, to be named in the ENTRIES
# of a light_add_asset_pack() like any other blob. The digest that pack carries is what protects
# them: a radio image is a thing a device executes, so substituting one has to mean substituting a
# digest inside a signed firmware image, and that is exactly what the pack already arranges.
#
#   HEADER defaults to the platform's own copy of the vendor drop, which is where it already is on
# a tree that can build for this part at all; name one only for a part the platform does not ship.

include_guard(GLOBAL)

include(${CMAKE_CURRENT_LIST_DIR}/LightAssets.cmake)

function(light_add_radio_firmware NAME)
        cmake_parse_arguments(P "" "HEADER" "" ${ARGN})
        light_asset_destination(light_add_radio_firmware "${NAME}" "" "")

        if(NOT DEFINED P_HEADER)
                if(NOT DEFINED PICO_SDK_PATH)
                        message(FATAL_ERROR "light_add_radio_firmware(${NAME}): no HEADER given and this platform ships none -- name the vendor's combined firmware header")
                endif()
                #   the wireless-only build. The platform ships a second, larger drop of the same
                # version beside it whose image also carries the short-range radio; this one is
                # what a product that wants wireless alone should upload, and the two behave
                # identically up to and including the regulatory blob, which is the same file in
                # both. Name the other explicitly if a product ever wants the short-range radio
                set(P_HEADER "${PICO_SDK_PATH}/lib/cyw43-driver/firmware/w43439A0_7_95_49_00_combined.h")
        endif()
        if(NOT EXISTS "${P_HEADER}")
                message(FATAL_ERROR "light_add_radio_firmware(${NAME}): '${P_HEADER}' is not there -- the platform's vendor drop may not be checked out")
        endif()

        set(image "${CMAKE_CURRENT_BINARY_DIR}/${NAME}_image.bin")
        set(limits "${CMAKE_CURRENT_BINARY_DIR}/${NAME}_limits.bin")
        #   one command, two outputs: the split reads the lengths out of the header and cuts both
        # blobs from the one array, so cutting them separately would mean parsing it twice
        add_custom_command(
                OUTPUT "${image}" "${limits}"
                COMMAND $<TARGET_FILE:crush> radio firmware "${P_HEADER}" --firmware "${image}" --clm "${limits}"
                DEPENDS crush "${P_HEADER}"
                COMMENT "crush: radio firmware ${NAME} -> image + limits"
                VERBATIM
        )

        add_custom_target(${NAME}_image DEPENDS "${image}")
        add_custom_target(${NAME}_limits DEPENDS "${limits}")
        light_asset_declare(${NAME}_image "${image}" "" "")
        light_asset_declare(${NAME}_limits "${limits}" "" "")
endfunction()
