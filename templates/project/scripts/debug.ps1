# Starts OpenOCD, and optionally gdb, against a built target, through a debug probe.
#
# USAGE:  scripts/debug.ps1 [-Target <name>] [-Preset <name>] [-ServerOnly] [-Attach] [-NoBuild]
#                           [-Ex <cmd>[,<cmd>...]] [-Batch] [-ProbeRs]
#
#     scripts/debug.ps1                                  # flash, then a gdb prompt
#     scripts/debug.ps1 -Batch -Ex 'monitor reset run'   # flash and leave it running
#     scripts/debug.ps1 -ProbeRs                         # probe-rs: flash, reset, running
param(
        [string]$Target,
        [string]$Preset,
        [switch]$ServerOnly,
        [switch]$Attach,
        [switch]$NoBuild,
        [string[]]$Ex,
        [switch]$Batch,
        [switch]$ProbeRs
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'light-tools.ps1')

if (-not $Target) { $Target = (& (Join-Path $PSScriptRoot 'project.config.ps1')).DefaultTarget }

& (Join-Path $LightScripts 'light-debug.ps1') -Target $Target -Preset $Preset `
        -ServerOnly:$ServerOnly -Attach:$Attach -NoBuild:$NoBuild `
        -Ex $Ex -Batch:$Batch -ProbeRs:$ProbeRs -ProjectRoot $root
