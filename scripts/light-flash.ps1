# Builds and flashes an RP2 target over USB, without touching the board.
#
# EVERYTHING THE BOARD NEEDS, not only the application: a product that verifies its firmware also
# carries a bootloader and, where its assets were taken out of the image, an asset pack. Each is a
# separate download routed to its own partition by family, and a board given one of the three does
# not run. A completed download reboots the board, so each one means entering BOOTSEL again -- by
# the 1200-baud reset where the console is on that port, and by the BOOT button where it is not.
#
# WHY THIS EXISTS: this loop was run by hand dozens of times in a single session, and it has
# four ways of failing quietly. Each is handled below and each cost real debugging time before
# it was understood:
#
#   1. matching the USB VID alone selects a CMSIS-DAP probe instead of the board, so the reset
#      goes to the wrong device and nothing happens -- which looks exactly like the board
#      ignoring it. Match the PID too.
#   2. SerialPort.Open() at 1200 baud THROWS while successfully triggering the reset. Treating
#      that exception as failure aborts a flash that was working.
#   3. Get-Volume can return a volume object with no DriveLetter. Building a destination path
#      from it yields nonsense, Copy-Item fails, and a script that reports success anyway will
#      happily leave you testing a stale image. This is not hypothetical -- it happened, and the
#      wrong firmware then looked like a regression in the code under test.
#   4. once a board has panicked or halted, the 1200-baud reset is gone with it: that route is
#      served by the firmware's own USB stack. Only the physical BOOT button can recover it, and
#      saying so plainly beats retrying.
#
# STM32 targets are not handled -- those flash over SWD from a .bin/.hex and have no UF2 path.
#
# USAGE:  light-flash.ps1 [-Target <name>] [-Preset <name>] [-NoBuild] [-TimeoutSeconds 15]
param(
        [string]$Target,
        [string]$Preset,
        [switch]$NoBuild,
        [int]$TimeoutSeconds = 15,
        [string]$ProjectRoot
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'lib/LightPlatform.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'lib/LightImage.psm1') -Force

$config = Get-LightProjectConfig -ProjectRoot ($ProjectRoot ? $ProjectRoot : (Get-LightProjectRoot))
#   defaults to the project's DefaultTarget, so a bare invocation does the obvious thing from
# a terminal and CI need not encode a target name it would have to keep in step
if (-not $Target) { $Target = Resolve-LightDefaultTarget -Config $config }
if (-not $Preset) { $Preset = Resolve-LightTargetPreset -Config $config -Target $Target }
$tree = Resolve-LightTree -Config $config -Preset $Preset

if (-not $NoBuild) {
        & (Join-Path $PSScriptRoot 'light-build.ps1') -Target $Target -Preset $Preset -ProjectRoot $config.Root
}

#   A BOARD THAT SAYS IT FLASHES OVER SWD IS NOT ASKED TO DO IT OVER USB. The download route
# needs the board back in BOOTSEL for each image it is given, and the only way in without touching
# it is the 1200-baud reset -- which is served by the firmware's own USB stack, and a board whose
# USB port is a host has none. Such a board declares Flash = 'swd', and the debug path already
# writes every image where the map puts it and starts the result.
if ($config.Targets -and $config.Targets.ContainsKey($Target) -and $config.Targets[$Target].Flash -eq 'swd') {
        Write-Host "$Target flashes over SWD"
        & (Join-Path $PSScriptRoot 'light-debug.ps1') -Target $Target -Preset $Preset -NoBuild -Batch `
                -Ex 'monitor reset run' -ProjectRoot $config.Root
        return
}

#   EVERYTHING THE BOARD NEEDS, not just the application. A product that verifies its firmware is
# a bootloader, an application in one of the slots its map describes, and -- where the assets were
# taken out of the image -- a pack in the data partition. Each is downloaded separately, routed to
# its own partition by the family it was packaged with, and a board given only one of the three
# does not run. A project with no bootloader yields a one-entry plan: its own UF2, as before.
$plan = Get-LightFlashPlan -Config $config -Tree $tree -Target $Target -Form uf2
if (-not $plan) {
        throw "nothing to flash for '$Target' -- is it an RP2 target? (STM32 targets flash over SWD and are not supported here)"
}

#   a mounted BOOTSEL volume, or nothing. Get-LightBootselVolume can also report the volume as
# present but UNMOUNTED, which only happens on Linux and is worth its own message: the board is
# in the right state and the desktop simply has not mounted it, so telling the user to hold BOOT
# would send them to fix something that is not broken
function Get-BootselTarget {
        $v = Get-LightBootselVolume
        if ($v -and $v.Path) { return $v }
        return $null
}
function Get-BootselUnmounted {
        $v = Get-LightBootselVolume
        if ($v -and -not $v.Path) { return $v }
        return $null
}

function Wait-For {
        param([scriptblock]$Condition, [int]$Seconds, [string]$What)

        $deadline = (Get-Date).AddSeconds($Seconds)
        while ((Get-Date) -lt $deadline) {
                $result = & $Condition
                if ($result) { return $result }
                Start-Sleep -Milliseconds 250
        }
        return $null
}

#   Get the board into BOOTSEL and answer with the volume to copy to. Called once per image,
# because a completed download reboots the board out of BOOTSEL -- so writing three images means
# entering it three times, and where the 1200-baud reset is not available (a board whose USB port
# is a host, so the console is not on it) that means the BOOT button, three times, and saying so.
function Enter-Bootsel {
        param(
                #   set for every image after the first: the board has just rebooted out of
                # BOOTSEL and is re-enumerating, and some come straight back on their own -- a
                # bootloader with nothing to boot hands itself to the host's. Waiting a moment
                # first is the difference between that and "no board found"
                [switch]$Settle
        )

        $volume = Get-BootselTarget
        if (-not $volume -and $Settle) {
                $volume = Wait-For -Seconds 5 -Condition { Get-BootselTarget }
        }
        if ($volume) {
                Write-Host "board already in BOOTSEL at $($volume.Path)"
        } else {
                #   VID_2E8A&PID_0009 is the framework's console device on every chip; only an RP2
                # board answers the 1200-baud reset with BOOTSEL, and its serial says which it is
                # (LIGHT-RP2, or LIGHT on older firmware). An STM32 board with the same VID/PID takes the
                # line coding and does nothing, which looks exactly like a board that ignored it. PID_000C
                # is a CMSIS-DAP probe, which shares the VID and behaves the same way
                $ports = @(Find-LightSerialPort -VendorId '2E8A' -ProductId '0009' | Where-Object { $_.Serial -in 'LIGHT', 'LIGHT-RP2' })
                if ($ports.Count -gt 1) {
                        $list = ($ports | ForEach-Object { "$($_.Device) ($($_.Serial))" }) -join ', '
                        throw "several RP2 boards are attached ($list); leave one connected to flash it."
                }
                $port = $ports | Select-Object -First 1
                if (-not $port) {
                        $stuck = Get-BootselUnmounted
                        if ($stuck) {
                                throw "the BOOTSEL volume is present ($($stuck.Device)) but not mounted, so there is nowhere to copy to. Mount it and re-run with -NoBuild -- e.g. 'udisksctl mount -b $($stuck.Device)'."
                        }
                        $others = @(Find-LightSerialPort -VendorId '2E8A' -ProductId '0009')
                        if ($others) {
                                $list = ($others | ForEach-Object { "$($_.Device) ($($_.Serial))" }) -join ', '
                                throw "no RP2 board found: the framework console(s) attached are $list, which flash over SWD, not BOOTSEL."
                        }
                        throw "no board found: no CDC port with VID_2E8A&PID_0009, and no BOOTSEL volume. Is it connected? If it has panicked or halted, its USB stack is gone -- hold BOOT and re-plug."
                }

                Write-Host "resetting $($port.Device) into BOOTSEL"
                $sp = New-Object System.IO.Ports.SerialPort $port.Device, 1200, None, 8, one
                try { $sp.Open(); $sp.Close() } catch {
                        # expected on Windows: "A device which does not exist was specified". The reset
                        # still happened. Linux usually returns cleanly, so this is tolerated, not required
                }

                $volume = Wait-For -Seconds $TimeoutSeconds -Condition { Get-BootselTarget }
                if (-not $volume) {
                        $stuck = Get-BootselUnmounted
                        if ($stuck) {
                                throw "the board entered BOOTSEL ($($stuck.Device)) but nothing mounted it, so there is nowhere to copy to. Mount it and re-run with -NoBuild -- e.g. 'udisksctl mount -b $($stuck.Device)'."
                        }
                        throw "board did not enter BOOTSEL within ${TimeoutSeconds}s. If it is halted (a panic, or a build that faulted early) the 1200-baud reset is served by firmware that is no longer running -- hold the BOOT button and re-plug it, then re-run with -NoBuild."
                }
        }
        return $volume
}

if ($plan.Count -gt 1) {
        Write-Host "$Target needs $($plan.Count) images: $(($plan | ForEach-Object What) -join ', ')"
}

for ($i = 0; $i -lt $plan.Count; $i++) {
        $item = $plan[$i]
        #   a board that cannot be got back into BOOTSEL part-way through is a board holding
        # some of what it needs and none of the rest. Say which, because "no board found" on its
        # own reads as though nothing happened
        try {
                $volume = Enter-Bootsel -Settle:($i -gt 0)
        } catch {
                if ($i -eq 0) { throw }
                $done = ($plan[0..($i - 1)] | ForEach-Object What) -join ', '
                $left = ($plan[$i..($plan.Count - 1)] | ForEach-Object What) -join ', '
                throw "$($_.Exception.Message)`n`nThis board has $done on it and still needs $left. Each download reboots the board, so put it back in BOOTSEL and re-run with -NoBuild -- already-written images are simply written again."
        }
        $dest = $volume.Path
        Write-Host "copying $(Split-Path $item.File -Leaf) -> $dest"
        Copy-Item $item.File -Destination $dest -Force -ErrorAction Stop

        #   the volume disappearing IS the reboot. It does not always follow: a download of data
        # has nothing to run afterwards, so the board can stay where it is -- which is where the
        # next image wants it anyway. So this is only worth a word when there is nothing left to
        # write and the board should have gone back to running
        $gone = Wait-For -Seconds $TimeoutSeconds -Condition { if (-not (Get-BootselTarget)) { $true } }
        if (-not $gone -and $i -eq $plan.Count - 1) {
                Write-Warning "BOOTSEL volume still present after ${TimeoutSeconds}s -- the board may not have rebooted"
        }
}
Write-Host "flashed $Target"
