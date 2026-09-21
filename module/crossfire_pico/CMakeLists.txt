#   crossfire on the product board: a stock Pico (RP2040) whose native USB port HOSTS
# USB-MIDI instruments (through an OTG adapter, one device or a hub), forwarding every
# instrument's MIDI to every other; the Pico-OLED-1.3 display board and its keys appear
# when one is fitted. The application is the hardware-independent light_app_crossfire
# crate; this module is its RP2040 instantiation -- see rust/src/board.rs.
#   the executable name matches the module directory: light-flash.ps1 and light-debug.ps1
# resolve module/<target>/<target>.{uf2,elf}
add_executable(crossfire_pico
        ../light_shell/src/main.c
)

target_link_libraries(crossfire_pico PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_crossfire_pico
)

#   the status font: 16 px on the Pico-OLED-1.3's real geometry -- two lines
# of ten cells across the rotated 128-pixel row
light_add_font(crossfire_pico_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY po13 WIDTH 64 HEIGHT 128 DIMENSION 17.2x32.3
        POINT_SIZE 12 PIXEL_SIZE 16
        CRATE light_app_crossfire_pico ENV LIGHT_FONT_LGF
)

#   the look-and-feel and the interface as data, shared with the Pico 2 crossfire: the framework
# MONO default with this OLED's rounding, and the crossfire status page -- both live beside the
# application crate, embedded by it through the two env vars
light_add_theme(crossfire_pico_theme MONO
        THEME ${CMAKE_CURRENT_LIST_DIR}/../../crates/light_app_crossfire/theme.json
        CRATE light_app_crossfire_pico ENV LIGHT_THEME_LTH
)
light_add_ui(crossfire_pico_ui
        UI ${CMAKE_CURRENT_LIST_DIR}/../../crates/light_app_crossfire/design.json
        CRATE light_app_crossfire_pico ENV LIGHT_UI_LUI
)

light_shell_configure(crossfire_pico)
