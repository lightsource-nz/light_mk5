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
        cmake_parse_arguments(P "BLUETOOTH" "HEADER;BLUETOOTH_HEADER;NVRAM_HEADER" "" ${ARGN})
        light_asset_destination(light_add_radio_firmware "${NAME}" "" "")

        if(NOT DEFINED P_HEADER)
                if(NOT DEFINED PICO_SDK_PATH)
                        message(FATAL_ERROR "light_add_radio_firmware(${NAME}): no HEADER given and this platform ships none -- name the vendor's combined firmware header")
                endif()
                #   TWO DROPS OF THE SAME VERSION, and which one is right depends on whether the
                # short-range radio is wanted: the plain one carries wireless alone, the other
                # carries both. They behave identically up to and including the regulatory blob,
                # which is the same file in both, so a product that asks for BLUETOOTH is simply
                # given the larger image -- there is nothing else to choose and nothing to get
                # wrong by choosing it separately.
                if(P_BLUETOOTH)
                        set(P_HEADER "${PICO_SDK_PATH}/lib/cyw43-driver/firmware/wb43439A0_7_95_49_00_combined.h")
                else()
                        set(P_HEADER "${PICO_SDK_PATH}/lib/cyw43-driver/firmware/w43439A0_7_95_49_00_combined.h")
                endif()
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

        #   THE MODULE'S OWN SETTINGS, which the driver asks for beside the image: calibration and
        # identity -- the band it works in, what its antenna looks like, the address block it answers
        # on. Not optional and not part of either image; a radio given the wrong ones associates and
        # then performs badly, which is the least diagnosable kind of wrong. A fourth asset target,
        # `<name>_nvram`, and half a kilobyte.
        #   NAMED FOR THE PART, and it has to be: the vendor drop carries settings for several
        # modules side by side, they differ in exactly the fields that describe the radio's board,
        # and nothing checks that the file matches the silicon. Picking a neighbouring module's
        # file costs a week -- the radio comes up, reports its address, hears every network in the
        # building, associates, and is then thrown off by the access point when the keys are
        # exchanged, because reception needs no calibration and transmission does. Confirm the
        # part before changing this, and confirm it against what the radio reports as its chip ID
        # rather than against the name of the board it is soldered to.
        if(NOT DEFINED P_NVRAM_HEADER)
                set(P_NVRAM_HEADER "${PICO_SDK_PATH}/lib/cyw43-driver/firmware/wifi_nvram_43439.h")
        endif()
        if(NOT EXISTS "${P_NVRAM_HEADER}")
                message(FATAL_ERROR "light_add_radio_firmware(${NAME}): '${P_NVRAM_HEADER}' is not there -- the platform's vendor drop may not be checked out")
        endif()
        set(nvram "${CMAKE_CURRENT_BINARY_DIR}/${NAME}_nvram.bin")
        add_custom_command(
                OUTPUT "${nvram}"
                COMMAND $<TARGET_FILE:crush> radio nvram "${P_NVRAM_HEADER}" --nvram "${nvram}"
                DEPENDS crush "${P_NVRAM_HEADER}"
                COMMENT "crush: radio nvram ${NAME} -> settings"
                VERBATIM
        )
        add_custom_target(${NAME}_nvram DEPENDS "${nvram}")
        light_asset_declare(${NAME}_nvram "${nvram}" "" "")

        if(NOT P_BLUETOOTH)
                return()
        endif()
        #   THE SHORT-RANGE RADIO IS A SECOND IMAGE IN A SECOND HEADER, not a part of the one
        # above: the combined drop carries it in the silicon's own firmware, but the host still
        # uploads a separate patch for it. A third asset target, `<name>_bluetooth`, to be named
        # in the same pack -- seven kilobytes beside the other two hundred and twenty, and
        # protected by the same digest.
        if(NOT DEFINED P_BLUETOOTH_HEADER)
                set(P_BLUETOOTH_HEADER "${PICO_SDK_PATH}/lib/cyw43-driver/firmware/cyw43_btfw_43439.h")
        endif()
        if(NOT EXISTS "${P_BLUETOOTH_HEADER}")
                message(FATAL_ERROR "light_add_radio_firmware(${NAME}): BLUETOOTH asked for but '${P_BLUETOOTH_HEADER}' is not there -- the platform's vendor drop may not be checked out")
        endif()
        set(bt "${CMAKE_CURRENT_BINARY_DIR}/${NAME}_bluetooth.bin")
        add_custom_command(
                OUTPUT "${bt}"
                COMMAND $<TARGET_FILE:crush> radio bluetooth "${P_BLUETOOTH_HEADER}" --firmware "${bt}"
                DEPENDS crush "${P_BLUETOOTH_HEADER}"
                COMMENT "crush: radio bluetooth ${NAME} -> image"
                VERBATIM
        )
        add_custom_target(${NAME}_bluetooth DEPENDS "${bt}")
        light_asset_declare(${NAME}_bluetooth "${bt}" "" "")
endfunction()
