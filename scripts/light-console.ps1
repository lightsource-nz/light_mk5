# Opens a board's USB-CDC console and captures its output.
#
# WHY THIS EXISTS: two details make the difference between this working and appearing to be a
# dead board, and neither is discoverable from the symptom.
#
#   DtrEnable. A CDC console's notion of "connected" is conventionally the DTR line -- not the
# port being open -- and .NET's SerialPort leaves DtrEnable false, so opening the port satisfies
# Windows and nothing else. The framework's own console does not gate on DTR, but firmware that
# does (an SDK's stdio, a bootloader) sits waiting forever, and the capture comes back completely
# empty: no error, no partial output, not even a created file. That reads as a dead board or a
# bad flash, and it is neither. Asserting DTR costs nothing and removes the question.
#
#   Buffered reads. Appending to a file per line reopens it every line, which is slow enough to
# stop draining the CDC FIFO. A console that waits for space then stalls with it (measured on an
# SDK console: 13.2 s against a per-line writer, 1.7 s against a buffered one); the framework's
# console drops lines instead, so a slow reader loses log rather than stalling the board. Either
# way the observer must not perturb the thing it is observing.
#
#   Sending commands. `-Send` writes one or more console commands and captures each reply, so a
# script (or an agent) can drive the board's CLI non-interactively -- the same open/DTR/buffered
# discipline as a capture, plus: the boot output is drained once before the first command so the
# transcript is just the replies, and after each command the reader waits for the reply to arrive
# and go quiet (a short idle gap) rather than a fixed sleep, capped at -SettleMs. The command is
# sent with a trailing newline, which the firmware's line reader takes as Enter.
#
#   Which board. Every framework board presents the same console device, so with more than one
# attached the script refuses to guess: -Port names the one to open (COM19, /dev/ttyACM0), or
# -Board its chip family by serial (rp2, stm32h7, ...).
#
# USAGE:  light-console.ps1 [-Seconds 30] [-Out <file>] [-Until <regex>] [-Quiet] [-Port COM19 | -Board rp2]
#         light-console.ps1 -Send "stats"                     # one command, print its reply
#         light-console.ps1 -Send "stats","backlight 500"     # several, in order
#         light-console.ps1 -Send "sd" -Until "boot signature" # stop once a pattern is seen
param(
        [int]$Seconds = 30,
        [string]$Out,
        [string]$Until,
        [string[]]$Send,
        [int]$SettleMs = 1500,
        [switch]$Quiet,
        [string]$Port,
        [string]$Board
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightPlatform.psm1') -Force

$serial = if ($Board) { "LIGHT-$Board*" } else { '*' }
$ports = @(Find-LightSerialPort -VendorId '2E8A' -ProductId '0009' -Serial $serial)
if ($Port) { $ports = @($ports | Where-Object { $_.Device -eq $Port }) }
if (-not $ports) {
        $hint = if ($IsWindows) { '' } else { " On Linux the port also has to be readable -- if it exists but is not listed, check group membership (dialout/uucp)." }
        $which = if ($Port) { " at $Port" } elseif ($Board) { " with serial $serial" } else { '' }
        throw "no board found: no CDC port with VID_2E8A&PID_0009$which. If it is in BOOTSEL it has no console; if it has halted, its USB stack is gone.$hint"
}
if ($ports.Count -gt 1) {
        $list = ($ports | ForEach-Object { "$($_.Device) ($($_.Serial))" }) -join ', '
        throw "several framework boards are attached: $list. Say which with -Port or -Board."
}
# (not $port or $board: those are the [string] parameters -- names are case-insensitive --
# and the object would be flattened into one)
$chosen = $ports[0]

$sp = New-Object System.IO.Ports.SerialPort $chosen.Device, 115200, None, 8, one
$sp.DtrEnable = $true          # see the header -- without this the board never boots
$sp.ReadTimeout = 50
$sp.ReadBufferSize = 131072
$sp.Open()
$sp.DtrEnable = $true          # again after Open(), as some drivers reset it on open

$sb = New-Object System.Text.StringBuilder
$matched = $false
try {
        if ($Send) {
                if (-not $Quiet) { Write-Host "driving $($chosen.Device): $($Send.Count) command(s) (DTR asserted)" }
                #   let the board settle and its boot output land, then drain it so the transcript
                # is just the command replies (use a plain capture if the boot log is what you want)
                Start-Sleep -Milliseconds 700
                [void]$sp.ReadExisting()
                foreach ($cmd in $Send) {
                        $sp.WriteLine($cmd)                 # trailing newline = Enter to the line reader
                        if (-not $Quiet) { Write-Host "> $cmd" }
                        $startLen = $sb.Length
                        $lastData = Get-Date
                        $deadline = (Get-Date).AddMilliseconds($SettleMs)
                        while ((Get-Date) -lt $deadline) {
                                $chunk = $sp.ReadExisting()
                                if ($chunk.Length -gt 0) {
                                        [void]$sb.Append($chunk)
                                        if (-not $Quiet) { Write-Host -NoNewline $chunk }
                                        $lastData = Get-Date
                                        if ($Until -and $sb.ToString() -match $Until) { $matched = $true; break }
                                } elseif ($sb.Length -gt $startLen -and ((Get-Date) - $lastData).TotalMilliseconds -gt 300) {
                                        break               # this command's reply has arrived and gone quiet
                                } else {
                                        Start-Sleep -Milliseconds 5
                                }
                        }
                        if ($matched) { break }
                }
        } else {
                if (-not $Quiet) { Write-Host "capturing $($chosen.Device) for ${Seconds}s (DTR asserted)" }
                $deadline = (Get-Date).AddSeconds($Seconds)
                while ((Get-Date) -lt $deadline) {
                        $chunk = $sp.ReadExisting()
                        if ($chunk.Length -gt 0) {
                                [void]$sb.Append($chunk)
                                if (-not $Quiet) { Write-Host -NoNewline $chunk }
                                if ($Until -and $sb.ToString() -match $Until) { $matched = $true; break }
                        } else {
                                Start-Sleep -Milliseconds 5
                        }
                }
        }
} finally {
        try { $sp.Close() } catch { }
}

$text = $sb.ToString()
if ($Out) {
        [System.IO.File]::WriteAllText($Out, $text)
        if (-not $Quiet) { Write-Host "`nwrote $($text.Length) chars to $Out" }
}

if ($Until -and -not $matched) {
        $where = if ($Send) { "the replies to: $($Send -join ', ')" } else { "${Seconds}s of output" }
        throw "pattern '$Until' not seen in $where"
}
if ($text.Length -eq 0) {
        Write-Warning "captured nothing. If the board is running but silent, check it is not halted; this script already asserts DTR, which is the usual cause."
}
return $text
