# Opens a connected board's USB-CDC console: capture its output, or drive its CLI with -Send.
# With more than one framework board attached, -Port or -Board says which.
#
# USAGE:  scripts/console.ps1 [-Seconds 30] [-Out <file>] [-Until <regex>] [-Port COM19 | -Board rp2]
#         scripts/console.ps1 -Send "stats"                    # send a command, print its reply
#         scripts/console.ps1 -Send "stats","backlight 500"    # several commands in order
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
. (Join-Path $PSScriptRoot 'light-tools.ps1')

& (Join-Path $LightScripts 'light-console.ps1') -Seconds $Seconds -Out $Out -Until $Until -Send $Send -SettleMs $SettleMs -Quiet:$Quiet -Port $Port -Board $Board
