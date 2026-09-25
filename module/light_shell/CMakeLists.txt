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
                # Module state lives in .bss now; this is headroom for what does not.
                #   8 KB, which is BOTH scratch banks: core 1 runs on the shell's own array in
                # ordinary RAM (src/main.c), so the bank the platform reserves for it is set
                # aside for no one, and the two are adjacent. Zero here is how the platform is
                # told the user provides core 1's stack; script_include/memory_scratch.incl
                # hands the freed bank to core 0 and says why at length.
                #   IT IS NOT MERELY HEADROOM. Four kilobytes was less than a single frame in
                # the radio's path, so that path ran off the end of the stack on every use and
                # survived only because the bank below happened to be idle.
                PICO_STACK_SIZE=0x2000
                PICO_CORE1_STACK_SIZE=0
                #   NO MPU stack guards: the SDK's guard faulted core 1 at boot when it was tried
                # (it died holding the log lock and core 0 spun at its first log call), and it
                # would guard the linker's stack symbols, not the shell's array, anyway. So a
                # stack overflow here is SILENT -- which is why core 1's array is painted before
                # launch and its headroom reported on the console after boot (see src/main.c)
                PICO_USE_STACK_GUARDS=0
        )
        #   the one linker fragment the shell replaces, by the platform's own mechanism for it
        # rather than by forking the whole memory map: a fork would have to be kept in step with
        # every change made upstream, and all that differs is two region declarations
        pico_add_linker_script_override_path(${TARGET} ${CMAKE_CURRENT_FUNCTION_LIST_DIR}/script_include FILES memory_scratch.incl)
        #   no SDK stdio in either role: the console is Rust's, on both its transports
        pico_enable_stdio_usb(${TARGET} 0)
        pico_enable_stdio_uart(${TARGET} 0)
        pico_add_extra_outputs(${TARGET})
endfunction()
