# Gives a running board new firmware over its console: the wired route of the A/B update.
#
# WHAT IT IS FOR: a board whose product boots through the framework's bootloader has two
# application slots, and can be handed an image for the slot it is not running while it keeps
# running -- then starts it on approval, and keeps it only once the image has shown it works. A
# radio can deliver that image; so can this, over the cable the board is already plugged in by,
# with no debugger, no BOOTSEL and no button. The board must already be running firmware that
# answers `receive` (the application's console command over light_update::wire).
#
# WHAT HAPPENS, in order:
#   1. the board is asked which slot it is running (`receive`);
#   2. it is told an image of N bytes with a CRC-32 is coming (`receive <N> <crc32>`), and erases
#      the other slot -- seconds, during which the console is quiet;
#   3. the image goes over a line at a time (`rx <offset> <base64>`), EACH LINE WAITING FOR ITS
#      ANSWER. The console has no flow control: the board drops what does not fit in its buffer,
#      and reads nothing at all while it is writing storage. One line in flight is what fits;
#   4. with the last line the board checks the CRC, reads the slot back and starts the image on
#      approval. Its USB port goes away and comes back;
#   5. the new image checks itself and keeps itself or not, and this reports which, and which slot
#      the board is running now.
#
#   A LOST LINE IS NOT A FAILED UPDATE. A line that loses a byte does not parse; a reply can be
# dropped by a busy log. Either way this asks the board where it is (`receive`) and carries on from
# there -- the board acknowledges a piece it already has rather than writing it twice, so a resend
# is always safe. Only a run of failures in a row gives up.
#
# USAGE:  light-update.ps1 [-Target <name>] [-Preset <name>] [-NoBuild] [-Image <file>] [-Port COM21 | -Board rp2]
param(
        [string]$Target,
        [string]$Preset,
        [switch]$NoBuild,
        #   send this file rather than the target's built image -- an older build kept aside, or
        # one built elsewhere. It must be an image for the slot (the target's .bin), not a UF2
        [string]$Image,
        [string]$Port,
        [string]$Board,
        [string]$ProjectRoot
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightProject.psm1') -Force
Import-Module (Join-Path $PSScriptRoot 'lib/LightPlatform.psm1') -Force

$config = Get-LightProjectConfig -ProjectRoot ($ProjectRoot ? $ProjectRoot : (Get-LightProjectRoot))
if (-not $Target) { $Target = Resolve-LightDefaultTarget -Config $config }
if (-not $Preset) { $Preset = Resolve-LightTargetPreset -Config $config -Target $Target }

if (-not $Image) {
        #   a product with no bootloader has no second slot, and nothing on the board answers
        # `receive` -- saying so here beats a minute of timeouts
        if (-not ($config.Targets -and $config.Targets.ContainsKey($Target) -and $config.Targets[$Target].Bootloader)) {
                throw "'$Target' does not boot through a bootloader, so it has no second slot to update into. Use flash.ps1."
        }
        $tree = Resolve-LightTree -Config $config -Preset $Preset
        if (-not $NoBuild) {
                & (Join-Path $PSScriptRoot 'light-build.ps1') -Target $Target -Preset $Preset -ProjectRoot $config.Root
        }
        #   THE SLOT'S CONTENTS, which is the .bin: the signed image exactly as a debugger writes it
        # at the slot's address. A UF2 is the same bytes wrapped for a download, and would be taken
        # for an image and refused by the hardware
        $Image = Join-Path $tree "module/$Target/$Target.bin"
}
if (-not (Test-Path $Image -PathType Leaf)) {
        throw "no image at '$Image' -- has '$Target' been built?"
}
$bytes = [System.IO.File]::ReadAllBytes((Resolve-Path $Image).Path)

#   the CRC-32 everyone means by the name; the board computes the same over what it takes, and
# checks it before starting anything
if (-not ('LightCrc32' -as [type])) {
        Add-Type -TypeDefinition @'
public static class LightCrc32 {
        public static uint Compute(byte[] data) {
                uint crc = 0xffffffff;
                foreach (byte b in data) {
                        crc ^= b;
                        for (int i = 0; i < 8; i++) {
                                crc = (crc & 1) != 0 ? (crc >> 1) ^ 0xedb88320 : crc >> 1;
                        }
                }
                return ~crc;
        }
}
'@
}
$crc = [LightCrc32]::Compute($bytes)
$crcText = $crc.ToString('x8')
#   light_update::wire::CHUNK: the most one line carries, which the board's console is sized for
$chunk = 96

# --- the console ----------------------------------------------------------------------------

function Find-Console {
        if ($Port) { return $Port }
        $serial = if ($Board) { "LIGHT-$Board*" } else { '*' }
        $ports = @(Find-LightSerialPort -VendorId '2E8A' -ProductId '0009' -Serial $serial)
        if ($ports.Count -gt 1) {
                $list = ($ports | ForEach-Object { "$($_.Device) ($($_.Serial))" }) -join ', '
                throw "several framework boards are attached: $list. Say which with -Port or -Board."
        }
        if ($ports.Count -eq 1) { return $ports[0].Device }
        return $null
}

function Open-Console {
        param([string]$Device)
        $sp = New-Object System.IO.Ports.SerialPort $Device, 115200, None, 8, one
        #   see light-console.ps1: DTR is what a CDC console calls "connected"
        $sp.DtrEnable = $true
        $sp.NewLine = "`n"
        $sp.ReadTimeout = 100
        $sp.ReadBufferSize = 131072
        $sp.Open()
        $sp.DtrEnable = $true
        return $sp
}

#   the next line the board says, or $null once $Ms have passed with nothing -- or once the port
# has gone, which is how the board's reboot is seen, and which sets $script:gone
$script:gone = $false
function Read-Line {
        param($Sp, [int]$Ms)
        $deadline = [DateTime]::UtcNow.AddMilliseconds($Ms)
        while ([DateTime]::UtcNow -lt $deadline) {
                try {
                        return $Sp.ReadLine().TrimEnd("`r")
                } catch [System.TimeoutException] {
                } catch {
                        $script:gone = $true
                        return $null
                }
        }
        return $null
}

#   wait for the board's answer to the last thing sent: the offset it wants next, or a failure.
# Lines that are neither -- the echo of the command, anything else being logged -- go past
function Wait-Answer {
        param($Sp, [int]$Ms)
        $deadline = [DateTime]::UtcNow.AddMilliseconds($Ms)
        while ([DateTime]::UtcNow -lt $deadline) {
                $left = [int]($deadline - [DateTime]::UtcNow).TotalMilliseconds
                $line = Read-Line -Sp $Sp -Ms ([Math]::Max($left, 1))
                if ($null -eq $line) { break }
                if ($line -match 'receive: next (\d+) of (\d+)') { return @{ Next = [int]$Matches[1]; Total = [int]$Matches[2] } }
                if ($line -match 'receive: (failed.*)') { throw "the board refused the image: $($Matches[1])" }
                if ($line -match 'starting it') { return @{ Starting = $true } }
                if ($line -match 'receive: idle') { return @{ Idle = $line } }
                if ($line -match "unknown command.*receive|usage: receive") {
                        throw "the board does not take an image over its console (it answered: $line). Flash firmware that has the wired update once with flash.ps1; from then on this route works."
                }
        }
        return $null
}

#   after a silence, ask where the board is. Any late answer to what was sent before is drained
# first, so that what comes back is the answer to THIS question, and only one line is ever in
# flight -- the whole of what the board's buffer is sized for
function Get-Position {
        param($Sp)
        while ($null -ne (Read-Line -Sp $Sp -Ms 200)) { }
        $Sp.WriteLine('receive')
        return Wait-Answer -Sp $Sp -Ms 3000
}

$device = Find-Console
if (-not $device) {
        throw "no board found: no CDC port with VID_2E8A&PID_0009. The board has to be running for this route; if it is not, use flash.ps1."
}

$sp = Open-Console -Device $device
$started = Get-Date
$runningBefore = $null
try {
        Start-Sleep -Milliseconds 300
        $null = $sp.ReadExisting()

        #   1. which slot, so the end can say whether it changed
        $sp.WriteLine('receive')
        $first = Wait-Answer -Sp $sp -Ms 3000
        if (-not $first) { throw "the board on $device did not answer 'receive'. Is it running firmware that has the wired update?" }
        if ($first.Idle -and $first.Idle -match 'running partition (\S+)') { $runningBefore = $Matches[1] }
        Write-Host "updating $device with $(Split-Path $Image -Leaf): $($bytes.Length) bytes, CRC-32 $crcText (running partition $runningBefore)"

        #   2. announce it; the erase is seconds of silence
        $sp.WriteLine("receive $($bytes.Length) $crcText")
        $answer = Wait-Answer -Sp $sp -Ms 60000
        if (-not $answer -or $answer.Next -ne 0) { throw "the board did not open a session for the image (answered: $($answer | ConvertTo-Json -Compress))" }

        #   3. a line at a time, each one answered
        $at = 0
        $misses = 0
        $lastShown = 0
        $sendStart = Get-Date
        while ($true) {
                $len = [Math]::Min($chunk, $bytes.Length - $at)
                $sp.WriteLine("rx $at $([Convert]::ToBase64String($bytes, $at, $len))")
                $answer = Wait-Answer -Sp $sp -Ms 2000
                if ($script:gone) {
                        #   the board went away. After the last line that is the hand-over, which can
                        # beat its own announcement out of the door; before it, it is a board that reset
                        if ($at + $len -eq $bytes.Length) { break }
                        throw "the board went away at $at of $($bytes.Length) bytes -- it reset or was unplugged. Nothing was started; run this again."
                }
                if (-not $answer) {
                        $misses++
                        if ($misses -ge 10) { throw "the board stopped answering at $at of $($bytes.Length) bytes" }
                        $answer = Get-Position -Sp $sp
                        if (-not $answer) { continue }
                }
                if ($answer.Starting) { break }
                if ($answer.Idle) { throw "the board has no session open any more (it answered: $($answer.Idle)) -- it may have reset" }
                $misses = 0
                $at = $answer.Next
                $pct = [int](100 * $at / $bytes.Length)
                if ($pct -ge $lastShown + 10) {
                        $lastShown = $pct - ($pct % 10)
                        $rate = $at / [Math]::Max(((Get-Date) - $sendStart).TotalSeconds, 0.001) / 1024
                        Write-Host ("  {0,3}%  {1} of {2} bytes  {3:N1} KiB/s" -f $pct, $at, $bytes.Length, $rate)
                }
        }
        $secs = ((Get-Date) - $started).TotalSeconds
        Write-Host ("staged and read back in {0:N1} s; the board is starting it" -f $secs)
} finally {
        try { $sp.Close() } catch { }
}

#   4/5. the board reboots into the new image on approval; watch it decide.
#
#   THE OLD PORT IS STILL LISTED FOR A MOMENT after the board has let go of it, and a port opened
# then is one that closes under the reader. So first wait for it to go, then for it to come back
# -- and reconnect whenever it goes again, because an image that is not kept is replaced by a
# second reboot, and that reboot is part of the answer
$goneBy = (Get-Date).AddSeconds(5)
while ((Get-Date) -lt $goneBy) {
        $still = $null
        try { $still = Find-Console } catch { }
        if (-not $still) { break }
        Start-Sleep -Milliseconds 100
}

$verdict = $null
$now = $null
$sp = $null
#   the image has seconds to keep itself, and says so when it does; past the hardware's own
# watchdog an image that never committed has already been replaced by the old one
$watch = (Get-Date).AddSeconds(30)
try {
        while ((Get-Date) -lt $watch) {
                if (-not $sp) {
                        $device = $null
                        try { $device = Find-Console } catch { }
                        if ($device) {
                                try { $sp = Open-Console -Device $device; $script:gone = $false } catch { $sp = $null }
                        }
                        if (-not $sp) { Start-Sleep -Milliseconds 250; continue }
                }
                $line = Read-Line -Sp $sp -Ms 500
                if ($script:gone) {
                        Write-Host "  (the board went away again)"
                        try { $sp.Close() } catch { }
                        $sp = $null
                        continue
                }
                if ($null -eq $line) {
                        if ($verdict) { break }
                        continue
                }
                #   what the new image says about itself while it decides is the evidence either way
                if ($line -match '\] (info|warn|error) ') { Write-Host "  | $line" }
                if ($line -match 'probation: committed') { $verdict = 'kept' }
                elseif ($line -match 'probation: (not keeping.*|the commit failed.*)') { $verdict = "not kept: $($Matches[1])" }
        }
        if (-not $sp) {
                $device = Find-Console
                if ($device) { $sp = Open-Console -Device $device; $script:gone = $false }
        }
        if ($sp) { $now = Get-Position -Sp $sp }
} finally {
        if ($sp) { try { $sp.Close() } catch { } }
}

$runningAfter = if ($now -and $now.Idle -and $now.Idle -match 'running partition (\S+)') { $Matches[1] } else { '?' }
if ($verdict -eq 'kept') {
        Write-Host "the new firmware committed itself: running partition $runningAfter (was $runningBefore)"
} elseif ($verdict) {
        throw "the new firmware did not keep itself ($verdict); the board is back on partition $runningAfter"
} elseif ($runningAfter -ne $runningBefore -and $runningAfter -ne '?') {
        #   the commit line was said before the console was open; the slot it is still on after
        # the hardware's watchdog has had its chance is the answer
        Write-Host "running partition $runningAfter (was $runningBefore) past the probation window: the new firmware is kept"
} else {
        throw "the board is on partition $runningAfter, as before: the new firmware was not kept. console.ps1 -FromReset shows why."
}
