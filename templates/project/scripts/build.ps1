# Builds a target, configuring its tree first if it does not exist yet.
#
# USAGE:  scripts/build.ps1 [-Target <name>] [-Preset <name>] [-Clean]
param(
        [string]$Target,
        [string]$Preset,
        [switch]$Clean
)

$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
. (Join-Path $PSScriptRoot 'light-tools.ps1')

if (-not $Target) { $Target = (& (Join-Path $PSScriptRoot 'project.config.ps1')).DefaultTarget }

& (Join-Path $LightScripts 'light-build.ps1') -Target $Target -Preset $Preset -Clean:$Clean -ProjectRoot $root
