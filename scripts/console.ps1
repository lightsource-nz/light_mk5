# Opens a connected board's USB-CDC console: capture its output, or drive its CLI with -Send.
# With more than one framework board attached, -Port or -Board says which.
#
# USAGE:  scripts/console.ps1 [-Seconds 30] [-Out <file>] [-Until <regex>] [-Port COM19 | -Board rp2]
#         scripts/console.ps1 -Send "stats"                    # send a command, print its reply
#         scripts/console.ps1 -Send "stats","backlight 500"    # several commands in order
#         scripts/console.ps1 -FromReset -Target <t>            # reset the board and catch its boot
param(
        [int]$Seconds = 30,
        [string]$Out,
        [string]$Until,
        [string[]]$Send,
        [int]$SettleMs = 1500,
        [switch]$Quiet,
        [string]$Port,
        [string]$Board,
        #   reset the board once the console is open, and capture what it says about coming up.
        #
        #   USE THIS FOR ANY QUESTION ABOUT STARTING UP. Everything a board says about coming up it
        # says in its first few milliseconds -- the clock, the assets, which image the hardware
        # chose, and any complaint that stops it before the runtime turns over. Opening the console
        # afterwards is too late: the stream is empty, and on a board that then stops that reads
        # exactly like hardware which never came up. That wrong conclusion is easy to reach and
        # expensive to keep.
        [switch]$FromReset,
        #   which board to reset, for -FromReset: the same pair the debug and flash scripts take
        [string]$Target,
        [string]$Preset
)

$ErrorActionPreference = 'Stop'
. (Join-Path $PSScriptRoot 'light-tools.ps1')

#   the reset runs over the debugger while the console holds the serial port -- two different
# devices, so they do not contend. Built here rather than in the shared script because knowing how
# to reset THIS project's boards is the project's business.
$onOpen = $null
if ($FromReset) {
        $debug = Join-Path $PSScriptRoot 'debug.ps1'
        $onOpen = {
                & $debug -Target $Target -Preset $Preset -NoBuild -Attach -Batch -Ex "monitor reset run" *>$null
        }.GetNewClosure()
}

& (Join-Path $LightScripts 'light-console.ps1') -Seconds $Seconds -Out $Out -Until $Until -Send $Send -SettleMs $SettleMs -Quiet:$Quiet -Port $Port -Board $Board -OnOpen $onOpen
