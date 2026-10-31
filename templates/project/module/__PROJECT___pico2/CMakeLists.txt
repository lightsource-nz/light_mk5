#   __PROJECT__ on a Raspberry Pi Pico 2. The executable is a pico-sdk one in every respect --
# runtime, linker script, UF2 output -- with the Rust staticlib as one more library on its link
# line. Its name matches this directory, which is where the flash and debug scripts look for
# __PROJECT___pico2.uf2 and .elf.
add_executable(__PROJECT___pico2
        ${LIGHT_PATH}/module/light_shell/src/main.c
)

target_link_libraries(__PROJECT___pico2 PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app___PROJECT___pico2
)

#   assets go here, each one call that compiles a file and hands the blob to the crate (see the
# framework's guide, chapter 04). They need crush, which the top-level CMakeLists imports:
#
# light_add_font(__PROJECT___pico2_font
#         FONT <file.ttf> DISPLAY <name> WIDTH <px> HEIGHT <px> DIMENSION <WxH mm>
#         POINT_SIZE <pt> PIXEL_SIZE <px>
#         CRATE light_app___PROJECT___pico2 ENV LIGHT_FONT_LGF)
# light_add_theme(__PROJECT___pico2_theme MONO
#         CRATE light_app___PROJECT___pico2 ENV LIGHT_THEME_LTH)
# light_add_ui(__PROJECT___pico2_ui
#         UI ${CMAKE_CURRENT_LIST_DIR}/design.json
#         CRATE light_app___PROJECT___pico2 ENV LIGHT_UI_LUI)

light_shell_configure(__PROJECT___pico2)
