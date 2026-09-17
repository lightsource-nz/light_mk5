# Opens a board's USB-CDC console and captures its output.
#
# WHY THIS EXISTS: two details make the difference between this working and appearing to be a
# dead board, and neither is discoverable from the symptom.
#
#   DtrEnable. pico-sdk's notion of "connected" is tud_cdc_connected(), which is the DTR line --
# not the port being open. .NET's SerialPort leaves DtrEnable false, so opening the port
# satisfies Windows and nothing else. Firmware built with PICO_STDIO_USB_CONNECT_WAIT_TIMEOUT_MS
# set to -1 (which these apps are) then sits in its connect-wait loop forever, and the capture
# comes back completely empty: no error, no partial output, not even a created file. That reads
# as a dead board or a bad flash, and it is neither. Note the 1200-baud BOOTSEL reset is
# unaffected, because it only needs the baud change -- so flashing keeps working perfectly while
# every capture is silent, which is what makes it confusing.
#
#   Buffered reads. Appending to a file per line reopens it every line, which is slow enough to
# stop draining the CDC FIFO. The firmware then blocks inside stdio_usb_out_chars() waiting for
# space -- up to PICO_STDIO_USB_STDOUT_TIMEOUT_US per write -- and the device stalls. Measured:
# the same firmware stalled 13.2 s against a per-line writer and 1.7 s against a buffered one.
# The observer must not perturb the thing it is observing.
#
#   Sending commands. `-Send` writes one or more console commands and captures each reply, so a
# script (or an agent) can drive the board's CLI non-interactively -- the same open/DTR/buffered
# discipline as a capture, plus: the boot output is drained once before the first command so the
# transcript is just the replies, and after each command the reader waits for the reply to arrive
# and go quiet (a short idle gap) rather than a fixed sleep, capped at -SettleMs. The command is
# sent with a trailing newline, which the firmware's line reader takes as Enter.
#
# USAGE:  light-console.ps1 [-Seconds 30] [-Out <file>] [-Until <regex>] [-Quiet]
#         light-console.ps1 -Send "stats"                     # one command, print its reply
#         light-console.ps1 -Send "stats","backlight 500"     # several, in order
#         light-console.ps1 -Send "sd" -Until "boot signature" # stop once a pattern is seen
param(
        [int]$Seconds = 30,
        [string]$Out,
        [string]$Until,
        [string[]]$Send,
        [int]$SettleMs = 1500,
        [switch]$Quiet
)

$ErrorActionPreference = 'Stop'
Import-Module (Join-Path $PSScriptRoot 'lib/LightPlatform.psm1') -Force

$port = Find-LightSerialPort -VendorId '2E8A' -ProductId '0009' | Select-Object -First 1
if (-not $port) {
        $hint = if ($IsWindows) { '' } else { " On Linux the port also has to be readable -- if it exists but is not listed, check group membership (dialout/uucp)." }
        throw "no board found: no CDC port with VID_2E8A&PID_0009. If it is in BOOTSEL it has no console; if it has halted, its USB stack is gone.$hint"
}

$sp = New-Object System.IO.Ports.SerialPort $port.Device, 115200, None, 8, one
$sp.DtrEnable = $true          # see the header -- without this the board never boots
$sp.ReadTimeout = 50
$sp.ReadBufferSize = 131072
$sp.Open()
$sp.DtrEnable = $true          # again after Open(), as some drivers reset it on open

$sb = New-Object System.Text.StringBuilder
$matched = $false
try {
        if ($Send) {
                if (-not $Quiet) { Write-Host "driving $($port.Device): $($Send.Count) command(s) (DTR asserted)" }
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
                if (-not $Quiet) { Write-Host "capturing $($port.Device) for ${Seconds}s (DTR asserted)" }
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
