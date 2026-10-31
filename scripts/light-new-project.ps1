# Creates a new project built on the light framework, from templates/project.
#
# WHY THIS EXISTS: a project that consumes the framework needs a dozen files that agree with each
# other -- two cargo workspaces whose members name each other, crate names that must not collide
# with CMake target names, presets that include the framework's, a script configuration that maps
# targets to trees, and paths back to the framework from four different directories. Assembling
# that by hand from another project means editing every one of those and missing one, and the
# miss rarely fails loudly: a firmware workspace copied without its profiles builds unoptimised
# and looks like broken hardware. The template holds the agreement once, and this fills it in.
#
#   Every file under templates/project is copied. Three tokens are replaced, in contents and in
# file and directory names: __PROJECT__ (the name), __LIGHT__ (the framework, relative to the
# directory of the file it appears in), and __LIGHT_FROM_ROOT__ (the framework, relative to the
# project root). The paths are RELATIVE so the two checkouts can move together -- the usual
# layout is siblings -- and LIGHT_PATH overrides them anyway.
#
#   The result is a git repository with nothing committed, so the first commit is yours.
#
# USAGE:  light-new-project.ps1 -Name <name> [-Path <parent directory>] [-NoGit]
#
#   -Name   lower case letters, digits and underscores, starting with a letter: it becomes crate
#           names, CMake targets and preset names, which all accept exactly that
#   -Path   the directory to create the project IN (default: the one holding this framework
#           checkout, so the new project is its sibling). Must exist, and be on the same volume
#           as the framework so the paths between them can be relative
#   -NoGit  do not run `git init`
param(
        [Parameter(Mandatory)] [string]$Name,
        [string]$Path,
        [switch]$NoGit
)

$ErrorActionPreference = 'Stop'

$framework = (Resolve-Path (Join-Path $PSScriptRoot '..')).Path
$template = Join-Path $framework 'templates' 'project'
if (-not (Test-Path $template)) { throw "no project template at '$template'" }

if ($Name -cnotmatch '^[a-z][a-z0-9_]*$') {
        throw "'$Name' is not a usable project name: lower case letters, digits and underscores, starting with a letter"
}
if ($Name.EndsWith('_') -or $Name.Contains('__')) {
        throw "'$Name' is not a usable project name: no trailing or doubled underscores (the template's tokens are spelt with them)"
}

if (-not $Path) { $Path = Split-Path $framework -Parent }
if (-not (Test-Path $Path -PathType Container)) { throw "'$Path' is not an existing directory" }
$parent = (Resolve-Path $Path).Path
$dest = Join-Path $parent $Name
if (Test-Path $dest) { throw "'$dest' already exists; choose another name or -Path" }

#   relative paths need a common root. [IO.Path]::GetRelativePath answers an ABSOLUTE path across
# volumes, which every consumer below would then glue onto its own directory and get wrong
if ([System.IO.Path]::GetPathRoot($parent) -ne [System.IO.Path]::GetPathRoot($framework)) {
        throw "'$parent' is on a different volume from the framework at '$framework'; create the project on the same volume, so the paths between them can be relative"
}

function Get-RelativeForwardPath([string]$From, [string]$To) {
        ([System.IO.Path]::GetRelativePath($From, $To)) -replace '\\', '/'
}

$fromRoot = Get-RelativeForwardPath $dest $framework
$utf8 = [System.Text.UTF8Encoding]::new($false)
$files = Get-ChildItem -Path $template -Recurse -File -Force

try {
        foreach ($file in $files) {
                $rel = [System.IO.Path]::GetRelativePath($template, $file.FullName)
                $out = Join-Path $dest ($rel.Replace('__PROJECT__', $Name))
                $outDir = Split-Path $out -Parent
                New-Item -ItemType Directory -Force -Path $outDir | Out-Null

                $text = [System.IO.File]::ReadAllText($file.FullName, $utf8)
                $text = $text.Replace('__PROJECT__', $Name)
                $text = $text.Replace('__LIGHT_FROM_ROOT__', $fromRoot)
                $text = $text.Replace('__LIGHT__', (Get-RelativeForwardPath $outDir $framework))
                [System.IO.File]::WriteAllText($out, $text, $utf8)
        }
} catch {
        #   nothing half-made is left behind: the directory did not exist before this run, so
        # everything in it is this run's
        if (Test-Path $dest) { Remove-Item -Recurse -Force $dest }
        throw
}

Write-Host "created $dest ($($files.Count) files) against the framework at $fromRoot"

if (-not $NoGit) {
        if (Get-Command git -ErrorAction SilentlyContinue) {
                git -C $dest init -q -b main
                if ($LASTEXITCODE -ne 0) { throw "git init failed with exit code $LASTEXITCODE" }
                Write-Host "initialised a git repository (nothing committed)"
        } else {
                Write-Warning "git not found; the project is not a repository yet"
        }
}

Write-Host ""
Write-Host "next:"
Write-Host "  cd $dest"
Write-Host "  scripts/build.ps1                 # the Pico 2 firmware, ${Name}_pico2"
Write-Host "  scripts/flash.ps1                 # over BOOTSEL"
Write-Host "  scripts/console.ps1 -Send help    # talk to it"
Write-Host "  scripts/test.ps1                  # the host tests"
