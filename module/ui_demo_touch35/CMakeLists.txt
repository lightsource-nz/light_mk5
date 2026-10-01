#   the Waveshare RP2350-Touch-LCD-3.5: the widget demo on the 320x480 ST7796S panel with the
# FT6336U touch controller -- this firmware is those parts' bring-up.
add_executable(ui_demo_touch35
        ../light_shell/src/main.c
)

target_link_libraries(ui_demo_touch35 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_ui_demo_touch35
)

#   the demo's font: the 3.5" panel's geometry (320x480 over 49.0x73.4 mm active area), 16 px
light_add_font(ui_demo_touch35_font
        FONT ${CMAKE_CURRENT_LIST_DIR}/../../tools/crush/tests/resources/fonts/TypeLightSans.ttf
        DISPLAY ws_touch35 WIDTH 320 HEIGHT 480 DIMENSION 49.0x73.4
        POINT_SIZE 14 PIXEL_SIZE 16
        CRATE light_app_ui_demo_touch35 ENV LIGHT_FONT_LGF
)

#   200 MHz, the clock the board's vendor runs it at: the camera's XCLK (a PWM output) is a whole
# fraction of it, and the display's SPI reaches 100 MHz (clk_peri / 2). The core voltage is raised
# to the vendor's 1.25 V for it -- the RP2350 SDK does not adjust it on its own.
target_compile_definitions(ui_demo_touch35 PRIVATE
        SYS_CLK_KHZ=200000
        # 12 MHz XOSC * 100 = 1200 MHz VCO, / 6 / 1 = 200 MHz
        PLL_SYS_VCO_FREQ_HZ=1200000000
        PLL_SYS_POSTDIV1=6
        PLL_SYS_POSTDIV2=1
        SYS_CLK_VREG_VOLTAGE_AUTO_ADJUST=1
        SYS_CLK_VREG_VOLTAGE_MIN=VREG_VOLTAGE_1_25
)

#   the framework's default look-and-feel for a color board (steel)
light_add_theme(ui_demo_touch35_theme
        CRATE light_app_ui_demo_touch35 ENV LIGHT_THEME_LTH
)

#   the demo interface as data: the app crate's design.json extends light_app_ui_demo's shared
# design (overriding just the title and size) and compiles to an LUI blob.
light_add_ui(ui_demo_touch35_ui
        UI ${CMAKE_CURRENT_LIST_DIR}/../../crates/light_app_ui_demo_touch35/design.json
        CRATE light_app_ui_demo_touch35 ENV LIGHT_UI_LUI
)

light_shell_configure(ui_demo_touch35)
