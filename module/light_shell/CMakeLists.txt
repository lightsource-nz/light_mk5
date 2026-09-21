#   the C shell every RP2 board's executable compiles: src/main.c plus the pico-sdk knobs it
# depends on. A function rather than a library because the panic hook is a per-executable
# setting, and the USB-host role links a stack the device role must not.
#
#   light_shell_configure(<target> [USB_HOST]): the default is the device role -- the console is
# the port's own USB device stack (light-rp2's usb-console feature) on core 1, in Rust, and the
# shell has no stdio and links no USB stack. USB_HOST is crossfire's role: the native port hosts
# USB-MIDI instruments through TinyUSB's host stack, running on core 0 under the Rust runtime,
# and the console is the UART -- also Rust's, on core 1. A host-role application supplies
# tusb_config.h on its include path; a device-role one needs none.
function(light_shell_configure TARGET)
        cmake_parse_arguments(SHELL "USB_HOST" "" "" ${ARGN})
        target_compile_definitions(${TARGET} PRIVATE
                #   the SDK's own panics go through the same relay as Rust's
                PICO_PANIC_FUNCTION=light_shell_panic_sdk
                PICO_PANIC_FUNCTION_DOES_NOT_RETURN=1
                #   core 0's stack: the SDK's default is 2 KB, which the widget toolkit's demo
                # overran the moment its module state was built on the stack -- the board
                # enumerated and then went silent, core 1 waiting on a lock core 0 died holding.
                # Module state lives in .bss now; this is headroom for what does not. 4 KB is
                # the ceiling: each core's stack lives in its own 4 KB scratch bank
                PICO_STACK_SIZE=0x1000
                PICO_CORE1_STACK_SIZE=0x1000
                #   an MPU guard region at the bottom of each stack, so an overflow is a hard
                # fault at the instruction that did it rather than core 1's stack (which sits
                # directly below core 0's) being silently overwritten -- which is what the first
                # toolkit build did: the UI kept answering taps while USB went dead
                PICO_USE_STACK_GUARDS=0
        )
        #   no SDK stdio in either role: the console is Rust's, on both its transports
        pico_enable_stdio_usb(${TARGET} 0)
        pico_enable_stdio_uart(${TARGET} 0)
        if(SHELL_USB_HOST)
                target_compile_definitions(${TARGET} PRIVATE LIGHT_SHELL_USB_HOST=1)
                #   the host stack and TinyUSB's BSP (board_init, which the SDK integration
                # calls for the clock); PIO-USB deliberately not -- the host role runs on the
                # native port alone
                target_link_libraries(${TARGET} PRIVATE tinyusb_host tinyusb_board)
        endif()
        pico_add_extra_outputs(${TARGET})
endfunction()
