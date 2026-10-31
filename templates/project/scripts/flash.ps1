# Builds a target if needed, then puts it on the board: over BOOTSEL for a 'uf2' target, through
# a debug probe for an 'swd' one (see scripts/project.config.ps1).
#
# USAGE:  scripts/flash.ps1 [-Target <name>] [-Preset <name>] [-NoBuild]
param(
        [string]$Target,
        [string]$Preset,
        [switch]$NoBuild
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'light-tools.ps1')

if (-not $Target) { $Target = (& (Join-Path $PSScriptRoot 'project.config.ps1')).DefaultTarget }

& (Join-Path $LightScripts 'light-flash.ps1') -Target $Target -Preset $Preset -NoBuild:$NoBuild -ProjectRoot $root
