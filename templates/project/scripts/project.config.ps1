# Per-project defaults for __PROJECT__, read by the framework's shared light-*.ps1 layer.
#
# The one place names map to build trees: which preset builds a target, which directory that
# preset owns, how an image reaches the board, and which debug configuration belongs to it.
@{
        Name = '__PROJECT__'

        #   preset -> build directory, relative to the project
        Trees = @{
                'conf-__PROJECT__-host'          = 'build-host'
                'conf-__PROJECT__-pico2-debug'   = 'build-pico2'
                'conf-__PROJECT__-pico2-release' = 'build-pico2-release'
        }

        #   target (a directory under module/) -> its preset and how it is flashed.
        #   'uf2' needs no probe: the script reboots the running board into BOOTSEL through the
        # console's 1200-baud reset and copies the image to the volume that appears (a board that
        # has never been flashed, or has halted, needs BOOT held while it is plugged in).
        #   'swd' writes it through a debug probe instead. A product with a bootloader names it
        # here as Bootloader = '<target>', and both routes then write the whole board
        Targets = @{
                '__PROJECT___pico2' = @{ Preset = 'conf-__PROJECT__-pico2-debug'; Flash = 'uf2' }
        }

        #   the OpenOCD configurations are the framework's; Chip is probe-rs's name for the part.
        # Paths are relative to the project
        Debug = @{
                'conf-__PROJECT__-pico2-debug' = @{
                        Config = '__LIGHT_FROM_ROOT__/openocd-rp2350.cfg'
                        Svd    = '__LIGHT_FROM_ROOT__/../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x'
                }
                'conf-__PROJECT__-pico2-release' = @{
                        Config = '__LIGHT_FROM_ROOT__/openocd-rp2350.cfg'
                        Svd    = '__LIGHT_FROM_ROOT__/../pico-sdk/src/rp2350/hardware_regs/RP2350.svd'
                        Chip   = 'RP235x'
                }
        }

        Test = @{
                Preset = 'conf-__PROJECT__-host'
                Ctest  = $true
        }

        DefaultTarget = '__PROJECT___pico2'
}
