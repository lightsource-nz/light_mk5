# Opens a connected board's USB serial console: capture its output, or drive its CLI with -Send.
# With more than one board attached, -Port or -Board says which.
#
# USAGE:  scripts/console.ps1 [-Seconds 30] [-Out <file>] [-Until <regex>] [-Port COM19 | -Board rp2]
#         scripts/console.ps1 -Send "stats"                    # send a command, print its reply
#         scripts/console.ps1 -Send "help","led off"           # several commands in order
#         scripts/console.ps1 -FromReset -Target <t>           # reset over a probe and catch the boot
param(
        [int]$Seconds = 30,
        [string]$Out,
        [string]$Until,
        [string[]]$Send,
        [int]$SettleMs = 1500,
        [switch]$Quiet,
        [string]$Port,
        [string]$Board,
        #   reset the board once the console is open, so the capture begins at its boot. Use this
        # for any question about starting up: a console opened afterwards has missed it all.
        # Needs a debug probe on the board
        [switch]$FromReset,
        [string]$Target,
        [string]$Preset
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'light-tools.ps1')

$onOpen = $null
if ($FromReset) {
        $debug = Join-Path $PSScriptRoot 'debug.ps1'
        $onOpen = {
                & $debug -Target $Target -Preset $Preset -NoBuild -Attach -Batch -Ex "monitor reset run" *>$null
        }.GetNewClosure()
}

& (Join-Path $LightScripts 'light-console.ps1') -Seconds $Seconds -Out $Out -Until $Until -Send $Send -SettleMs $SettleMs -Quiet:$Quiet -Port $Port -Board $Board -OnOpen $onOpen
