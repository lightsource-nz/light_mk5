# Finds the light framework and sets $LightScripts for the wrappers in this directory.
#
# This project is not the framework: the shared light-*.ps1 layer lives in the framework
# checkout, found through LIGHT_PATH (the environment, or the location this project was created
# against). The wrappers here supply only this project's defaults and call that layer for the logic.
$ErrorActionPreference = 'Stop'

$LightPath = if ($env:LIGHT_PATH) { $env:LIGHT_PATH } else { [System.IO.Path]::GetFullPath([System.IO.Path]::Combine($PSScriptRoot, '__LIGHT__')) }
if (-not (Test-Path (Join-Path $LightPath 'light_init.cmake'))) {
        throw "could not find the light framework at '$LightPath' -- set LIGHT_PATH to the light_mk5 checkout."
}
$LightPath = (Resolve-Path $LightPath).Path
$env:LIGHT_PATH = $LightPath
$LightScripts = Join-Path $LightPath 'scripts'
