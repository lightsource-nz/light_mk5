#   the dictaphone on the Waveshare RP2350-Touch-LCD-3.5: the voice recorder over the board's ES8311
# codec, microphone, speaker amplifier and TF slot. The application is light_dictaphone_core with the
# portrait light_app_dictaphone interface; this module is its 3.5 instantiation. Same board and shell
# configuration as ui_demo_touch35, built in the same tree, flashed as its own UF2.
#
#   the executable name matches the module directory: light-flash.ps1 and light-debug.ps1
# resolve module/<target>/<target>.{uf2,elf}
add_executable(dictaphone_touch35
        ../light_shell/src/main.c
)

target_link_libraries(dictaphone_touch35 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_dictaphone_touch35
)

#   200 MHz at 1.25 V, as the board's widget demo runs (see its CMakeLists for why)
target_compile_definitions(dictaphone_touch35 PRIVATE
        SYS_CLK_KHZ=200000
        PLL_SYS_VCO_FREQ_HZ=1200000000
        PLL_SYS_POSTDIV1=6
        PLL_SYS_POSTDIV2=1
        SYS_CLK_VREG_VOLTAGE_AUTO_ADJUST=1
        SYS_CLK_VREG_VOLTAGE_MIN=VREG_VOLTAGE_1_25
)

#   the same font as the board's demo: the 3.5" panel's geometry, 16 px
light_add_font(dictaphone_touch35_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch35 WIDTH 320 HEIGHT 480 DIMENSION 49.0x73.4
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_dictaphone_touch35 ENV LIGHT_FONT_LGF
)

#   the interface's own look: light_app_dictaphone/theme.json when the design has one, else the
# framework default (steel). CREATING one where there was none needs a single reconfigure first.
set(dictaphone_touch35_theme ${CMAKE_CURRENT_LIST_DIR}/../../crates/light_app_dictaphone/theme.json)
if(EXISTS ${dictaphone_touch35_theme})
        light_add_theme(dictaphone_touch35_theme
                THEME ${dictaphone_touch35_theme}
                CRATE light_app_dictaphone_touch35 ENV LIGHT_THEME_LTH
        )
else()
        light_add_theme(dictaphone_touch35_theme
                CRATE light_app_dictaphone_touch35 ENV LIGHT_THEME_LTH
        )
endif()

#   the portrait interface, as data: light_app_dictaphone's design with this panel's size overlaid
light_add_ui(dictaphone_touch35_ui
        UI ${CMAKE_CURRENT_LIST_DIR}/design.json
        CRATE light_app_dictaphone_touch35 ENV LIGHT_UI_LUI
)

light_shell_configure(dictaphone_touch35)
