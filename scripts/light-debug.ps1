# Starts an OpenOCD server, and optionally gdb, against a built target.
#
# WHY THIS EXISTS: the debug setup was reachable only through VS Code's cortex-debug extension,
# and its launch.json leaned on ${command:cmake.launchTargetPath} -- a variable only that
# extension can resolve. So there was no way to start a debug session from a shell, and no way
# for a script to do it either.
#
# It also encodes which OpenOCD config and SVD belong to which board, because getting that wrong
# is not a clean failure: attaching an rp2040 configuration to an rp2350 image produces confusing
# misbehaviour rather than an error, and screen-test's launch.json named the rp2040 SVD for every
# configuration including the RP2350 ones.
#
# HARDWARE-VERIFIED on a Pico 2 through a CMSIS-DAP probe, all three paths: -ServerOnly reaches a
# live OpenOCD on 3333; the default path writes every image the board needs, reads each back, and
# starts the result under gdb; and -Batch runs to completion and exits 0, or nonzero when a
# command fails.
#
# IMAGES ARE WRITTEN WHERE THE BOARD'S MAP PUTS THEM, not where they were linked -- see the plan
# below and lib/LightImage.psm1. This is the difference between a board that boots afterwards and
# one whose bootloader has just been overwritten by the application.
#
# USAGE:  light-debug.ps1 [-Target <name>] [-Preset <name>] [-ServerOnly] [-Attach] [-NoBuild]
#                         [-Ex <cmd>[,<cmd>...]] [-Batch] [-ProbeRs]
#
#   -Ex appends gdb commands after the standard ones, in order. COMMA-SEPARATED for more than
# one: it is a PowerShell array parameter, so `-Ex 'a' -Ex 'b'` is an error rather than two
# commands.
#   -Batch runs gdb non-interactively -- it executes the commands and exits, and this script then
# exits with gdb's status instead of handing over a prompt.
#
#   the two together are what make this usable from CI:
#
#     light-debug.ps1 -Batch                              # load the image over SWD and stop
#     light-debug.ps1 -Batch -Ex 'monitor reset run'      # load it and leave the board running
#     light-debug.ps1 -Ex 'break main'                    # interactive, stopped at main
#
#   -ProbeRs is the fast flash-and-run path: probe-rs downloads the image and resets the board,
# no OpenOCD, no gdb, always batch. It needs `Chip` in the preset's Debug entry (probe-rs's own
# chip name: RP235x, RP235x_riscv, RP2040, STM32H743VI, ...), and it sidesteps two OpenOCD
# behaviours this layer otherwise has to manage by hand on the RP2350 -- the flash-probe ROM
# stub run over a halted core, and SIO spinlock 31 left held by the debugger's own reads. The
# gdb path stays for interactive work and for `monitor` commands; this one is for "get the new
# image running":
#
#     light-debug.ps1 -ProbeRs                            # flash, reset, running -- done
param(
        [string]$Target,
        [string]$Preset,
        [switch]$ServerOnly,
        [switch]$Attach,
        [switch]$NoBuild,
        [string[]]$Ex,
        [switch]$Batch,
        [switch]$ProbeRs,
        [string]$ProjectRoot
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'lib/LightPlatform.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'lib/LightImage.psm1') -Force
. (Join-Path $PSScriptRoot 'light-env.ps1') -Quiet

$config = Get-LightProjectConfig -ProjectRoot ($ProjectRoot ? $ProjectRoot : (Get-LightProjectRoot))
#   defaults to the project's DefaultTarget, so a bare invocation does the obvious thing from
# a terminal and CI need not encode a target name it would have to keep in step
if (-not $Target) { $Target = Resolve-LightDefaultTarget -Config $config }
if (-not $Preset) { $Preset = Resolve-LightTargetPreset -Config $config -Target $Target }
$tree = Resolve-LightTree -Config $config -Preset $Preset

if (-not $config.Debug -or -not $config.Debug.ContainsKey($Preset)) {
        throw "no Debug entry for preset '$Preset' in scripts/project.config.ps1 -- it must name the OpenOCD config and SVD for this board"
}
$debug = $config.Debug[$Preset]

if (-not $NoBuild) {
        & (Join-Path $PSScriptRoot 'light-build.ps1') -Target $Target -Preset $Preset -ProjectRoot $config.Root
}

#   the built executable, which is gdb's symbols. Its suffix depends on the toolchain and it lives
# under module/ or test/ depending on whose target it is -- see Find-LightTargetElf, which the
# plan also uses to ask an image where it was linked
$elf = Find-LightTargetElf -Tree $tree -Target $Target

#   the probe-rs path: download, reset, done. Deliberately not a server and not a debugger --
# probe-rs's gdb server exists, but the OpenOCD path below already owns interactive debugging,
# and two half-configured ways to do one thing is how launch.json got into the state that made
# this script necessary
#   WHAT GOES ON THE BOARD AND WHERE, and why it is not `load` any more.
#
#   Two things made `load` wrong. A product that verifies its firmware is not one image: a
# bootloader holds the start of storage and the application lives in a slot the bootloader's map
# describes, so writing the application at its LINK address overwrites the bootloader with it and
# the board stops booting. And a signed image is one gdb will not load at all -- sealing rewrites
# the ELF so that the section before the data leaves no gap before it, which reads as an overlap
# and aborts the write. The second is the louder failure and the first is the expensive one.
#
#   So images are written from their .bin at an address that is known rather than inferred: out of
# the flash map for a product that has one, and out of the ELF's own program headers for one that
# does not -- which is what makes this right on a chip whose flash does not start where this one's
# does. The ELF stays, as gdb's symbols.
#
#   Not resolved for the paths that write nothing: -Attach looks at what is already running, and
# -ServerOnly hands the board to someone else's debugger. Neither should fail because an image
# they were never going to write has not been built.
$plan = ($Attach -or $ServerOnly) ? $null : (Get-LightFlashPlan -Config $config -Tree $tree -Target $Target -Form bin)
$mapped = @($plan | Where-Object { $_.Family })

if ($ProbeRs) {
        if ($ServerOnly -or $Attach -or $Ex) {
                throw "-ProbeRs flashes and resets, nothing else; -ServerOnly, -Attach and -Ex belong to the gdb path (drop -ProbeRs to use them)"
        }
        #   probe-rs downloads an ELF at its own addresses, which is the wrong place on a board
        # whose application lives in a partition. Rather than teach this path the plan as well,
        # it says so: the default path already writes the right thing to the right place
        if ($mapped) {
                throw "'$Target' boots through a flash map, and -ProbeRs writes an image at its link address -- which is where the bootloader lives. Use the default path, which writes each image where the map puts it."
        }
        if (-not $debug.Chip) {
                throw "no Chip for preset '$Preset' in scripts/project.config.ps1 -- the probe-rs path needs the chip's probe-rs name (RP235x, RP2040, STM32H743VI, ...) beside the Debug entry's Config"
        }
        $exeSuffix = Get-LightExeSuffix
        #   the official installer and `cargo install` both land in ~/.cargo/bin, which
        # light-env.ps1 does not put on PATH for the C projects -- so it is searched first
        $probeRsTool = Find-LightTool -Name "probe-rs$exeSuffix" -Candidates @(
                (Join-Path $HOME ".cargo/bin/probe-rs$exeSuffix")
        )
        if (-not $probeRsTool) { throw "no probe-rs found: install it (probe.rs/docs/getting-started/installation) or put it on PATH" }
        Write-Host "probe-rs: $probeRsTool"
        Write-Host "chip:     $($debug.Chip)"
        Write-Host "elf:      $elf"
        #   --verify ALWAYS, because a silent misprogram is real: on an RP2040 (w25q16jv) the
        # plain download reported success, `probe-rs verify` then said the flash did not match,
        # and the core sat in the bootrom with nothing to boot. With --verify the same download
        # programs correctly and a failure would be an error instead of a mystery. Costs a
        # couple of seconds of readback
        & $probeRsTool download --chip $debug.Chip --verify $elf
        if ($LASTEXITCODE -ne 0) { throw "probe-rs download failed with code $LASTEXITCODE" }
        & $probeRsTool reset --chip $debug.Chip
        if ($LASTEXITCODE -ne 0) { throw "probe-rs reset failed with code $LASTEXITCODE" }
        return
}

$ocdConfig = Join-Path $config.Root $debug.Config
if (-not (Test-Path $ocdConfig)) { throw "no OpenOCD config at '$ocdConfig'" }

$exe = Get-LightExeSuffix
$tools = Get-LightToolRoot
#   named in project.config.ps1 if it is somewhere unusual; otherwise an unpacked xpack build
# under the tools root, otherwise whatever is on PATH -- which is the normal case on Linux,
# where openocd is a distro package
$openocd = Find-LightTool -Name "openocd$exe" -Candidates @(
        $debug.Server,
        "$tools/xpack-openocd-0.12.0-7/bin/openocd$exe",
        "$tools/xpack-openocd/bin/openocd$exe"
)
if (-not $openocd) { throw "no openocd found: not named in project.config.ps1 Debug.Server, not under '$tools', and not on PATH" }

#   check a probe is actually present, so "openocd exits with a transport error" becomes
# something that names the real problem. Note VID_2E8A&PID_0009 is the RP2 board's own CDC
# interface and is NOT a debug port -- only PID_000C is a CMSIS-DAP probe. ST-Link is a
# different vendor entirely (VID_0483), which is what the STM32 board is debugged through.
#
#   the two platforms enumerate USB completely differently, so this is one of the few places
# that genuinely branches rather than calling a shared helper: WMI has no Linux equivalent, and
# sysfs exposes idVendor/idProduct per device with no vendor string to match on
$probeNames = @()
if ($IsWindows) {
        $probes = Get-CimInstance Win32_PnPEntity -ErrorAction SilentlyContinue | Where-Object {
                $_.DeviceID -like '*VID_2E8A&PID_000C*' -or   # Raspberry Pi CMSIS-DAP / debugprobe
                $_.DeviceID -like '*VID_0483&PID_37*' -or      # ST-Link V2.1 / V3
                $_.DeviceID -like '*VID_1366*'                 # SEGGER J-Link
        }
        if ($probes) {
                # these enumerate as composite devices, so prefer a child whose name actually says
                # what it is over the generic "USB Composite Device" parent
                $named = $probes | Where-Object { $_.Name -notmatch 'Composite' } | Select-Object -First 1
                if (-not $named) { $named = $probes | Select-Object -First 1 }
                $probeNames = @($named.Name)
        }
} else {
        # (vendor, product-prefix, label); an empty product prefix matches the whole vendor
        $known = @(
                @('2e8a', '000c', 'Raspberry Pi CMSIS-DAP'),
                @('0483', '37',   'ST-Link V2.1/V3'),
                @('1366', '',     'SEGGER J-Link')
        )
        foreach ($d in @(Get-ChildItem /sys/bus/usb/devices -ErrorAction SilentlyContinue)) {
                $vf = Join-Path $d.FullName 'idVendor'
                $pf = Join-Path $d.FullName 'idProduct'
                if (-not (Test-Path $vf) -or -not (Test-Path $pf)) { continue }
                $v = (Get-Content $vf -Raw).Trim(); $p = (Get-Content $pf -Raw).Trim()
                foreach ($k in $known) {
                        if ($v -eq $k[0] -and $p.StartsWith($k[1])) {
                                $pn = Join-Path $d.FullName 'product'
                                $label = if (Test-Path $pn) { (Get-Content $pn -Raw).Trim() } else { $k[2] }
                                $probeNames += "$label ($v`:$p)"
                        }
                }
        }
}
if ($probeNames) {
        Write-Host "probe:   $($probeNames | Select-Object -First 1)"
} else {
        Write-Warning "no debug probe found (looked for CMSIS-DAP, ST-Link and J-Link). OpenOCD will fail to connect."
}

#   OpenOCD's own script library, which its board/target configs `source` by relative name.
# An unpacked xpack build keeps it at <prefix>/openocd/scripts; a distro package puts it at
# <prefix>/share/openocd/scripts. Passing -s for a directory that does not exist makes every
# `source` fail with a message about the inner file, never about the search path, so the
# candidates are probed and -s is simply omitted when neither is found -- a packaged openocd
# already knows its own default
$prefix = Split-Path (Split-Path $openocd -Parent) -Parent
$searchDir = @(
        (Join-Path $prefix 'openocd/scripts'),
        (Join-Path $prefix 'share/openocd/scripts')
) | Where-Object { Test-Path $_ } | Select-Object -First 1

$ocdArgs = @()
if ($searchDir) { $ocdArgs += @('-s', $searchDir) }
$ocdArgs += @('-f', $ocdConfig)
#   ATTACHING MUST NOT PROBE THE FLASH. On connect gdb asks for the memory map, and OpenOCD
# answers by probing the flash banks -- on an RP2 that means calling the boot ROM to take the
# flash out of XIP for a moment, with only core 0 halted: core 1, still running its console from
# flash, fetches garbage and locks up. A dead console after every "-Attach" look was that. Nothing
# is programmed on an attach, so the map is not wanted; the default path keeps it
if ($Attach) { $ocdArgs += @('-c', '"gdb memory_map disable"') }

Write-Host "openocd: $openocd"
Write-Host "config:  $ocdConfig"
Write-Host "elf:     $elf"
if ($debug.Svd) { Write-Host "svd:     $(Join-Path $config.Root $debug.Svd)" }

if ($ServerOnly) {
        Write-Host "`nstarting OpenOCD in the foreground -- attach a debugger to localhost:3333, Ctrl-C to stop"
        & $openocd @ocdArgs
        return
}

#   PROGRAMMING IS ITS OWN RUN, and not a `monitor` command inside the debug session, for a reason
# the old path got wrong quietly: a failed `monitor` command is a line of text, not a status, so
# gdb still exits 0 and the script still reports success. Here OpenOCD's own exit status is the
# answer, and a refused write stops the session before a debugger is pointed at an image that is
# not on the board.
#
#   Read back after every write. A silent misprogram is real -- on one board a download reported
# success, a verify then disagreed, and the core sat in the boot ROM with nothing to run. The
# readback costs a second or two against a morning.
if ($plan) {
        $programArgs = $ocdArgs + @('-c', 'init', '-c', 'reset halt')
        foreach ($item in $plan) {
                $where = $item.Address
                $file = ($item.File -replace '\\', '/')
                Write-Host ("write:   {0,-28} 0x{1:x8}  {2}" -f $item.What, $where, (Split-Path $item.File -Leaf))
                $programArgs += @('-c', "flash write_image erase `"$file`" $where bin")
                $programArgs += @('-c', "verify_image `"$file`" $where bin")
        }
        $programArgs += @('-c', 'reset init', '-c', 'shutdown')
        & $openocd @programArgs
        if ($LASTEXITCODE -ne 0) { throw "programming failed with code $LASTEXITCODE -- nothing was started" }
}

$server = Start-Process -FilePath $openocd -ArgumentList $ocdArgs -PassThru -NoNewWindow
try {
        Start-Sleep -Seconds 2
        if ($server.HasExited) { throw "OpenOCD exited immediately (code $($server.ExitCode)) -- is a probe connected and not already in use?" }

        #   LIGHT_ARM_GDB_DIR wins, then an unpacked toolchain under the tools root, then PATH.
        # On Linux gdb-multiarch is a common substitute where arm-none-eabi-gdb is not packaged
        $gdb = Find-LightTool -Name "arm-none-eabi-gdb$exe" -Candidates @(
                ($env:LIGHT_ARM_GDB_DIR ? (Join-Path $env:LIGHT_ARM_GDB_DIR "arm-none-eabi-gdb$exe") : $null),
                "$tools/arm-gnu-toolchain-14.2.rel1-mingw-w64-x86_64-arm-none-eabi/bin/arm-none-eabi-gdb$exe",
                "$tools/arm-gnu-toolchain/bin/arm-none-eabi-gdb$exe"
        )
        if (-not $gdb) { $gdb = Find-LightTool -Name "gdb-multiarch$exe" }
        if (-not $gdb) { throw "no arm-none-eabi-gdb (or gdb-multiarch) found: set LIGHT_ARM_GDB_DIR, unpack a toolchain under '$tools', or install one on PATH" }

        $gdbArgs = @()
        # -batch executes the commands and exits, with gdb's status. It must come before them
        if ($Batch) { $gdbArgs += '-batch' }
        $gdbArgs += @('-ex', 'target extended-remote localhost:3333')

        #   -Attach leaves the image on the board alone, which is what you want when debugging
        # something already running. Otherwise the images were written above, and this starts the
        # one that was written the way the hardware would -- from the reset vector, so the stack
        # pointer comes from the new image's vector table rather than being whatever the previous
        # image left behind. (Continuing from a loaded entry point instead mostly works, and
        # occasionally corrupts memory in a way that looks like a bug in whatever you were about
        # to debug.)
        #
        #   Where a map placed the images, the ELF is gdb's SYMBOLS and nothing else: it is
        # passed below and never `load`ed. That is also what stops gdb refusing an image whose
        # sections it dislikes -- one marker section with a bogus size is enough for that, and it
        # says nothing about whether the image is good.
        if (-not $Attach) {
                $gdbArgs += @('-ex', 'monitor reset init')
        }

        # caller's commands last, so they can rely on the image being loaded and the target halted
        foreach ($c in @($Ex)) { if ($c) { $gdbArgs += @('-ex', $c) } }
        $gdbArgs += $elf

        & $gdb @gdbArgs
        $gdbExit = $LASTEXITCODE
} finally {
        if (-not $server.HasExited) {
                Write-Host "stopping OpenOCD"
                Stop-Process -Id $server.Id -Force -ErrorAction SilentlyContinue
        }
}

#   gdb's status becomes this script's, but ONLY under -Batch. Interactively it is meaningless --
# quitting a session with a breakpoint still set, or Ctrl-C'ing out, exits nonzero and says
# nothing about whether anything went wrong. A CI caller passes -Batch and gets an answer it can
# branch on; a human gets one it would only have to ignore.
if ($Batch -and $gdbExit -ne 0) {
        throw "gdb exited with code $gdbExit"
}
