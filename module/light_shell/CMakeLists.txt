#   the C shell every RP2 board's executable compiles: src/main.c plus the pico-sdk knobs it
# depends on. A function rather than a library because the panic hook is a per-executable
# setting.
#
#   light_shell_configure(<target>): the same shell for every role. The shell has no stdio and
# links no USB stack: the console (the port's USB device stack with the CDC class, and the UART)
# runs on core 1 in Rust, and a board whose native port hosts instruments runs the port's USB
# host stack on core 0 in Rust -- the two roles differ only in which the port crate is built
# with (usb-console or usb-host).
function(light_shell_configure TARGET)
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
        pico_add_extra_outputs(${TARGET})
endfunction()
