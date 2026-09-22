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
# way the observer must not perturb the thing it is observing. `-Out` keeps one writer open for
# the run and flushes each CHUNK -- not each line, and not once at the end: a capture that is
# interrupted, that times out, or that the board cuts short still holds everything it saw, and a
# long one can be read while it runs.
#
#   The port can vanish mid-capture -- the board reboots, or is replugged -- and .NET throws out
# of the read when it does. That ends the capture; it must not lose it. The transcript stands,
# the reason is reported, and only an empty one is an error. Diagnosed from a board that reset
# itself during a 150 s capture: the exception escaped, the file was never written, and an
# interaction that had worked perfectly looked like a console that saw nothing.
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
# -Board its chip family by serial (rp2, stm32h7, ...). -Port also takes a port that is not a
# framework console at all -- a debug probe's UART bridge, for a board whose USB port is a host.
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
#   -Port names any port, framework console or not: a debug probe's UART bridge carries the
# console of a board whose USB port is busy being a host
$ports = if ($Port) { @([pscustomobject]@{ Device = $Port; Description = $Port; Serial = '' }) } else { @(Find-LightSerialPort -VendorId '2E8A' -ProductId '0009' -Serial $serial) }
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
#   why the port stopped answering, when it did: set by Read-Chunk, reported at the end
$gone = $null
#   one writer for the whole run, flushed per chunk -- see the header
$writer = if ($Out) { [System.IO.StreamWriter]::new($Out, $false) } else { $null }

#   every read goes through here: a device that disappears returns nothing and records why,
# so the loops end on their own terms with the transcript intact
function Read-Chunk {
        param([System.IO.Ports.SerialPort]$From, [ref]$Gone)
        try {
                return $From.ReadExisting()
        } catch {
                $Gone.Value = $_.Exception.Message
                return ''
        }
}

try {
        if ($Send) {
                if (-not $Quiet) { Write-Host "driving $($chosen.Device): $($Send.Count) command(s) (DTR asserted)" }
                #   let the board settle and its boot output land, then drain it so the transcript
                # is just the command replies (use a plain capture if the boot log is what you want)
                Start-Sleep -Milliseconds 700
                [void](Read-Chunk -From $sp -Gone ([ref]$gone))
                foreach ($cmd in $Send) {
                        if ($gone) { break }
                        $sp.WriteLine($cmd)                 # trailing newline = Enter to the line reader
                        if (-not $Quiet) { Write-Host "> $cmd" }
                        $startLen = $sb.Length
                        $lastData = Get-Date
                        $deadline = (Get-Date).AddMilliseconds($SettleMs)
                        while ((Get-Date) -lt $deadline) {
                                $chunk = Read-Chunk -From $sp -Gone ([ref]$gone)
                                if ($gone) { break }
                                if ($chunk.Length -gt 0) {
                                        [void]$sb.Append($chunk)
                                        if ($writer) { $writer.Write($chunk); $writer.Flush() }
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
                        $chunk = Read-Chunk -From $sp -Gone ([ref]$gone)
                        if ($gone) { break }
                        if ($chunk.Length -gt 0) {
                                [void]$sb.Append($chunk)
                                if ($writer) { $writer.Write($chunk); $writer.Flush() }
                                if (-not $Quiet) { Write-Host -NoNewline $chunk }
                                if ($Until -and $sb.ToString() -match $Until) { $matched = $true; break }
                        } else {
                                Start-Sleep -Milliseconds 5
                        }
                }
        }
} finally {
        #   the writer first: whatever was captured reaches the file even if the loop above left
        # by an exception, which is the whole point of streaming it
        if ($writer) { try { $writer.Flush(); $writer.Dispose() } catch { } }
        try { $sp.Close() } catch { }
}

$text = $sb.ToString()
if ($Out -and -not $Quiet) { Write-Host "`nwrote $($text.Length) chars to $Out" }
#   a port that went away is how a capture ENDS, not how it fails: say so and keep the transcript
if ($gone) {
        Write-Warning "$($chosen.Device) stopped answering after $($text.Length) chars -- the board rebooted or was unplugged ($gone)."
}

if ($Until -and -not $matched) {
        $where = if ($Send) { "the replies to: $($Send -join ', ')" } else { "${Seconds}s of output" }
        throw "pattern '$Until' not seen in $where"
}
if ($text.Length -eq 0) {
        Write-Warning "captured nothing. If the board is running but silent, check it is not halted; this script already asserts DTR, which is the usual cause."
}
return $text
