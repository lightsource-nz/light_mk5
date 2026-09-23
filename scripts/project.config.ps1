# Per-project defaults for light_mk5.
#
# Three trees: the touch169 firmware (BOOTSEL-flashed, the board has no SWD pads), a Pico 2
# with the Waveshare Pico-OLED-1.3 display board (flashed over SWD by debug.ps1 -Batch), and a
# host tree whose only job is to run `cargo test` under ctest so test.ps1 and CI need no
# Rust-specific path.
@{
        Name = 'light_mk5'

        Trees = @{
                'conf-light_mk5-host-debug'     = 'build-host'
                'conf-light_mk5-touch169-debug' = 'build-touch169'
                'conf-light_mk5-touch28-debug'  = 'build-touch28'
                'conf-light_mk5-touch349-debug' = 'build-touch349'
                'conf-light_mk5-touch4-debug'   = 'build-touch4'
                'conf-light_mk5-pico2-debug'    = 'build-pico2'
                # the release profile (opt-level s, fat LTO), as separate trees so the debug
                # ones stay warm: build/flash with -Preset conf-light_mk5-<board>-release
                'conf-light_mk5-touch169-release' = 'build-touch169-release'
                'conf-light_mk5-pico2-release'    = 'build-pico2-release'
                # the USB host-role probe: a Pico 2 with its native port hosting, own tree
                'conf-light_mk5-usb-host-probe-debug' = 'build-usb-host-probe'
                # the RP2040 Pico runs the pico2 demo; build/flash with -Preset
                'conf-light_mk5-pico-debug'           = 'build-pico'
                # the same boards on the RP2350's Hazard3 cores: build/flash with -Preset
                'conf-light_mk5-pico2-riscv-debug'    = 'build-pico2-riscv'
                'conf-light_mk5-touch169-riscv-debug' = 'build-touch169-riscv'
                # the MiniSTM32H7 on bare CMSIS, flashed over its ST-Link
                'conf-light_mk5-mini-stm32h7-debug'   = 'build-mini-stm32h7'
                'conf-light_mk5-blackpill-debug'      = 'build-blackpill'
        }

        Targets = @{
                'light_h7'   = @{ Preset = 'conf-light_mk5-mini-stm32h7-debug'; Flash = 'swd' }
                'light_f411' = @{ Preset = 'conf-light_mk5-blackpill-debug'; Flash = 'swd' }
                # the host-role probe: the smallest firmware that exercises the port's USB host
                # stack, so the role is built here and not only by the products that use it
                'usb_host_probe' = @{ Preset = 'conf-light_mk5-usb-host-probe-debug'; Flash = 'swd' }
                # the bootloader: the image at the start of flash that picks between the
                # application slots. Built in the Pico 2 trees beside whatever they build
                'light_bootloader' = @{ Preset = 'conf-light_mk5-pico2-debug'; Flash = 'swd' }
                # uf2 because the 1.69 exposes no SWD pads
                'ui_demo_touch169' = @{ Preset = 'conf-light_mk5-touch169-debug'; Flash = 'uf2' }
                'ui_demo_touch28'  = @{ Preset = 'conf-light_mk5-touch28-debug'; Flash = 'uf2' }
                'ui_demo_touch349' = @{ Preset = 'conf-light_mk5-touch349-debug'; Flash = 'uf2' }
                # the dictaphone: more applications on the 3.49, same board and tree --
                # the portrait interface and the landscape one over the same engine
                'dictaphone_touch349' = @{ Preset = 'conf-light_mk5-touch349-debug'; Flash = 'uf2' }
                'dictaphone_wide_touch349' = @{ Preset = 'conf-light_mk5-touch349-debug'; Flash = 'uf2' }
                'light_lui_touch349' = @{ Preset = 'conf-light_mk5-touch349-debug'; Flash = 'uf2' }
                'ui_demo_touch4'   = @{ Preset = 'conf-light_mk5-touch4-debug'; Flash = 'uf2' }
                # the pico2 target -- a Pico 2 with the Waveshare Pico-OLED-1.3 display board
                # -- flashes over SWD; Flash='swd' records that light-flash.ps1's BOOTSEL path
                # is not how an image reaches it
                'light_pico2'    = @{ Preset = 'conf-light_mk5-pico2-debug'; Flash = 'swd' }
        }

        Expect = @{
                'conf-light_mk5-host-debug'     = @{ LIGHT_PLATFORM = 'HOST'; LIGHT_SYSTEM = 'HOST_OS' }
                'conf-light_mk5-touch169-debug' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_1.69'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
                'conf-light_mk5-touch28-debug'  = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_2.8'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
                'conf-light_mk5-touch349-debug' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_3.49'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
                'conf-light_mk5-touch4-debug' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_4'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
                'conf-light_mk5-pico2-debug'    = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'pico2'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                }
                'conf-light_mk5-touch169-release' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_1.69'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                        CMAKE_BUILD_TYPE  = 'Release'
                }
                'conf-light_mk5-pico2-release'    = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'pico2'
                        PICO_PLATFORM     = 'rp2350-arm-s'
                        Rust_CARGO_TARGET = 'thumbv8m.main-none-eabi'
                        CMAKE_BUILD_TYPE  = 'Release'
                }
                'conf-light_mk5-usb-host-probe-debug' = @{
                        LIGHT_PLATFORM      = 'TARGET'
                        LIGHT_BOARD         = 'pico2'
                        PICO_PLATFORM       = 'rp2350-arm-s'
                        Rust_CARGO_TARGET   = 'thumbv8m.main-none-eabi'
                        LIGHT_PICO2_APP = 'usb_host_probe'
                }
                'conf-light_mk5-pico2-riscv-debug'    = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'pico2'
                        PICO_PLATFORM     = 'rp2350-riscv'
                        Rust_CARGO_TARGET = 'riscv32imac-unknown-none-elf'
                }
                'conf-light_mk5-pico-debug'           = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'pico'
                        PICO_PLATFORM     = 'rp2040'
                        Rust_CARGO_TARGET = 'thumbv6m-none-eabi'
                }
                'conf-light_mk5-touch169-riscv-debug' = @{
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'waveshare_rp2350_touch_lcd_1.69'
                        PICO_PLATFORM     = 'rp2350-riscv'
                        Rust_CARGO_TARGET = 'riscv32imac-unknown-none-elf'
                }
                'conf-light_mk5-mini-stm32h7-debug'   = @{
                        LIGHT_SYSTEM      = 'CMSIS'
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'mini_stm32h7'
                        Rust_CARGO_TARGET = 'thumbv7em-none-eabihf'
                }
                'conf-light_mk5-blackpill-debug'      = @{
                        LIGHT_SYSTEM      = 'CMSIS'
                        LIGHT_PLATFORM    = 'TARGET'
                        LIGHT_BOARD       = 'blackpill'
                        Rust_CARGO_TARGET = 'thumbv7em-none-eabihf'
                }
        }

        # which OpenOCD config and SVD belong to which board -- getting this pairing wrong
        # misbehaves rather than erroring, so it is worth stating explicitly
        Debug = @{
                # Chip is probe-rs's name for the part, for debug.ps1 -ProbeRs (fast
                # flash-and-run; no openocd, no spinlock-31 contamination)
                'conf-light_mk5-pico2-debug' = @{
                        Config = 'openocd-rp2350.cfg'
                        Svd    = '../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x'
                }
                'conf-light_mk5-pico2-release' = @{
                        Config = 'openocd-rp2350.cfg'
                        Svd    = '../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x'
                }
                'conf-light_mk5-usb-host-probe-debug' = @{
                        Config = 'openocd-rp2350.cfg'
                        Svd    = '../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x'
                }
                'conf-light_mk5-pico2-riscv-debug' = @{
                        Config = 'openocd-rp2350.cfg'
                        Svd    = '../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x_riscv'
                }
                'conf-light_mk5-pico-debug' = @{
                        Config = 'openocd-rp2040.cfg'
                        Svd    = '../pico-sdk/src/rp2040/hardware_regs/RP2040.svd'
                        Chip   = 'RP2040'
                }
                # over the ST-Link; no SVD vendored for this part
                'conf-light_mk5-mini-stm32h7-debug' = @{
                        Config = 'openocd-stm32h7.cfg'
                        Chip   = 'STM32H743VI'
                }
                'conf-light_mk5-blackpill-debug' = @{
                        Config = 'openocd-stm32f4.cfg'
                        Chip   = 'STM32F411CE'
                }
        }

        DefaultTarget = 'ui_demo_touch169'

        Test = @{
                Preset = 'conf-light_mk5-host-debug'
                Ctest  = $true
        }
}
