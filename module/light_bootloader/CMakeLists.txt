#   the framework's own build of the bootloader, against a reference flash map: an A/B pair for
# the application and a data partition for the assets, the first slot starting clear of this
# image. It exists so the bootloader is built and exercised here; a product supplies its own map
# and builds its own (light_add_bootloader), because the map is the device's, not the framework's.
light_add_bootloader(light_bootloader
        LAYOUT ${CMAKE_CURRENT_LIST_DIR}/partitions.json
)
