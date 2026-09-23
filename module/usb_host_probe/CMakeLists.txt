#   the framework's USB host-role probe: the smallest firmware that exercises light-rp2's host
# stack, so the role is built (and kept building) by this repository rather than only by the
# products that use it. A Pico 2, its native port hosting instruments, the console on the UART
# through a debug probe -- flashed over SWD.
add_executable(usb_host_probe
        ../light_shell/src/main.c
)

target_link_libraries(usb_host_probe PRIVATE
        pico_stdlib
        pico_multicore
        pico_bootrom
        light_app_usb_host_probe
)

light_shell_configure(usb_host_probe)
