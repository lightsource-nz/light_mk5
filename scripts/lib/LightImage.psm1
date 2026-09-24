# What goes on a board, and where.
#
# A product that verifies its firmware is not one image at the start of storage any more. It is a
# BOOTLOADER there, carrying the flash map; an APPLICATION in one of the slots that map describes;
# and, where the assets were taken out of the image, a PACK in the data partition. Writing one of
# the three over the other two's addresses is not a failure that announces itself -- it is a board
# that stops booting, and a morning spent on why.
#
# THE MAP IS NOT RESTATED HERE. It belongs to the bootloader, embedded in it at build time and
# signed with it, which is the whole point of putting it there; a second copy in a script would be
# a second copy to get wrong. So the layout is read back OUT of the built bootloader with picotool,
# the tool that wrote it -- offline, from the file, with no board involved. What a project has to
# say for itself is one thing a script cannot work out: which target is its bootloader.
#
#   Projects with neither a bootloader nor a map are the ordinary case and stay ordinary: their
# image goes where it was linked, which is the start of storage, and none of the above applies.

#   NOT -Force. Re-importing a module a caller has already imported unloads their copy along with
# it, and the caller's next call to a function they imported themselves fails with "not
# recognized" -- from a line that has nothing to do with this file
Import-Module (Join-Path $PSScriptRoot 'LightPlatform.psm1')

#   the execute-in-place window: a map counts from the start of storage, a debugger addresses the
# same bytes through this
$script:XipBase = 0x10000000

#   Where an image was linked, read out of its own ELF.
#
#   NOT ASSUMED, because the answer differs per chip -- one family's flash begins at 0x10000000
# and another's at 0x08000000 -- and the file says so. The program headers are read here rather
# than shelled out to a toolchain tool: it is twenty lines, it needs nothing installed, and it
# gives a number instead of text to parse.
function Get-LightElfLoadAddress {
        param([Parameter(Mandatory)] [string]$Elf)

        $bytes = [System.IO.File]::ReadAllBytes($Elf)
        if ($bytes.Length -lt 52 -or $bytes[0] -ne 0x7f -or $bytes[1] -ne 0x45 -or $bytes[2] -ne 0x4c -or $bytes[3] -ne 0x46) {
                throw "'$Elf' is not an ELF file"
        }
        if ($bytes[4] -ne 1 -or $bytes[5] -ne 1) {
                throw "'$Elf' is not a 32-bit little-endian ELF, which is all this layer reads"
        }
        $phoff = [BitConverter]::ToUInt32($bytes, 0x1c)
        $phentsize = [BitConverter]::ToUInt16($bytes, 0x2a)
        $phnum = [BitConverter]::ToUInt16($bytes, 0x2c)

        $lowest = $null
        for ($i = 0; $i -lt $phnum; $i++) {
                $p = $phoff + $i * $phentsize
                #   PT_LOAD, and carrying bytes: a segment with nothing in the file is .bss, which
                # is not written to flash and whose address says nothing about where the image goes
                if ([BitConverter]::ToUInt32($bytes, $p) -ne 1) { continue }
                if ([BitConverter]::ToUInt32($bytes, $p + 16) -eq 0) { continue }
                #   the PHYSICAL address: a .data segment lives in RAM but is LOADED from flash,
                # and it is the loaded-from address that a programmer writes
                $paddr = [BitConverter]::ToUInt32($bytes, $p + 12)
                if ($null -eq $lowest -or $paddr -lt $lowest) { $lowest = $paddr }
        }
        if ($null -eq $lowest) { throw "'$Elf' has no loadable segments" }
        return $lowest
}

# picotool, which the platform SDK fetches into the build tree; otherwise whatever is on PATH.
function Find-LightPicotool {
        param([Parameter(Mandatory)] [string]$Tree)

        $exe = Get-LightExeSuffix
        return Find-LightTool -Name "picotool$exe" -Candidates @(
                (Join-Path $Tree "_deps/picotool/picotool$exe")
        )
}

#   The flash map, read back out of a built bootloader.
#
#   `picotool info -a` prints the partition table the image carries, resolved -- each partition's
# extent in storage and the download families it accepts. Parsing its output is not lovely, but the
# alternative is resolving a layout JSON here, which means re-implementing the rules the boot ROM
# actually applies, and being subtly wrong about a map that verified fine.
function Get-LightFlashMap {
        param(
                [Parameter(Mandatory)] [string]$Picotool,
                [Parameter(Mandatory)] [string]$BootloaderImage
        )

        $out = & $Picotool info -a $BootloaderImage 2>&1
        if ($LASTEXITCODE -ne 0) {
                throw "picotool could not read '$BootloaderImage': $($out -join "`n")"
        }
        $partitions = @()
        foreach ($line in $out) {
                #   e.g.   partition 2 (A):  00210000->00290000 S(rw) ... "DATA", uf2 { 'data' }, ...
                if ($line -match "^\s*partition\s+(\d+)\s*\([^)]*\):\s*([0-9a-fA-F]+)->([0-9a-fA-F]+).*?`"([^`"]*)`"") {
                        #   read every field out before matching again: $Matches is one variable,
                        # and the families pattern below overwrites it
                        $index = [int]$Matches[1]
                        $start = [Convert]::ToUInt32($Matches[2], 16)
                        $end = [Convert]::ToUInt32($Matches[3], 16)
                        $name = $Matches[4]
                        $families = @()
                        if ($line -match "uf2 \{([^}]*)\}") {
                                $families = @($Matches[1] -split ',' | ForEach-Object { $_.Trim().Trim("'") } | Where-Object { $_ })
                        }
                        $partitions += [pscustomobject]@{
                                Index = $index; Start = $start; End = $end; Name = $name; Families = $families
                        }
                }
        }
        if (-not $partitions) {
                throw "'$BootloaderImage' carries no partition table -- is it the bootloader? (picotool info -a says nothing about partitions)"
        }
        return $partitions
}

#   Where a download of this family lands: the FIRST partition that accepts it.
#
#   First, not "the better of the pair", and deliberately. A device choosing between two slots does
# it by version at boot, which is the right answer in the field and the wrong one on a bench, where
# what you want is the build you just made running now. Writing the first slot is predictable; it
# also leaves the other slot as the older image the bootloader would fall back to, which is a
# useful thing to have while working on the bootloader itself.
function Resolve-LightPartition {
        param(
                [Parameter(Mandatory)] [array]$Map,
                [Parameter(Mandatory)] [string]$Family
        )

        $match = @($Map | Where-Object { $_.Families -contains $Family })
        if (-not $match) {
                $known = ($Map | ForEach-Object { "$($_.Name) { $($_.Families -join ', ') }" }) -join '; '
                throw "this device's flash map has no partition accepting '$Family' downloads. It has: $known"
        }
        return $match[0]
}

#   Everything that has to be on the board for this target to run, in the order it is written:
# the bootloader at the start of storage, the application in its slot, the assets in theirs.
#
#   Each entry carries the file to write, where it goes, and a name for the person watching. A
# project with no bootloader yields exactly one entry -- its image, at the start of storage -- so
# a caller does not branch on whether the product is a verified one.
function Get-LightFlashPlan {
        param(
                [Parameter(Mandatory)] [hashtable]$Config,
                [Parameter(Mandatory)] [string]$Tree,
                [Parameter(Mandatory)] [string]$Target,
                # 'bin' for the raw images a debugger writes, 'uf2' for the packaged ones a
                # download consumes
                [ValidateSet('bin', 'uf2')] [string]$Form = 'bin'
        )

        $moduleDir = Join-Path $Tree "module/$Target"
        if (-not (Test-Path $moduleDir)) { $moduleDir = Join-Path $Tree "test/$Target" }
        $bootloader = $null
        if ($Config.Targets -and $Config.Targets.ContainsKey($Target)) {
                $bootloader = $Config.Targets[$Target].Bootloader
        }

        $app = Join-Path $moduleDir "$Target.$Form"
        if (-not (Test-Path $app)) {
                throw "no $Form for '$Target' at '$app' -- has it been built?"
        }

        #   the plain case: no bootloader, so no map, so the image goes where it was linked --
        # which its own ELF is asked for rather than guessed at
        if (-not $bootloader) {
                $address = ($Form -eq 'bin') ? (Get-LightElfLoadAddress -Elf (Find-LightTargetElf -Tree $Tree -Target $Target)) : 0
                return @([pscustomobject]@{ What = $Target; File = $app; Address = $address; Family = $null })
        }

        $blImage = Join-Path $moduleDir "$bootloader.$Form"
        $blInfo = Join-Path $moduleDir "$bootloader.uf2"
        foreach ($f in @($blImage, $blInfo)) {
                if (-not (Test-Path $f)) {
                        throw "'$Target' boots through '$bootloader', but '$f' is not there -- build the bootloader target too."
                }
        }

        $picotool = Find-LightPicotool -Tree $Tree
        if (-not $picotool) {
                throw "no picotool found: '$Target' boots through a flash map, and picotool is what reads that map back out of '$bootloader'. It is normally fetched into the build tree by the platform SDK."
        }
        $map = Get-LightFlashMap -Picotool $picotool -BootloaderImage $blInfo

        #   the family the application image is downloaded under, which is what decides its slot.
        # picotool knows it; asking is cheaper than deducing it from the chip and the architecture
        $appFamily = (& $picotool info $app 2>&1 | Select-String "family ID '([^']+)'" | ForEach-Object { $_.Matches[0].Groups[1].Value } | Select-Object -First 1)
        if (-not $appFamily) { $appFamily = 'rp2350-arm-s' }

        $plan = @(
                [pscustomobject]@{ What = $bootloader; File = $blImage; Address = $script:XipBase; Family = 'absolute' }
                [pscustomobject]@{ What = $Target; File = $app; Address = ($script:XipBase + (Resolve-LightPartition -Map $map -Family $appFamily).Start); Family = $appFamily }
        )

        #   the assets, if this product keeps them out of its image. One pack per module by
        # construction, so it is found rather than declared
        $packForm = ($Form -eq 'uf2') ? 'uf2' : 'lap'
        $packs = @(Get-ChildItem -Path $moduleDir -Filter "*.$packForm" -File -ErrorAction SilentlyContinue |
                Where-Object { $_.BaseName -ne $Target -and $_.BaseName -ne $bootloader })
        if ($packForm -eq 'uf2') { $packs = @($packs | Where-Object { Test-Path (Join-Path $moduleDir "$($_.BaseName).lap") }) }
        if ($packs.Count -gt 1) {
                throw "'$moduleDir' holds more than one asset pack ($(($packs | ForEach-Object BaseName) -join ', ')); this layer expects one per board."
        }
        if ($packs.Count -eq 1) {
                $plan += [pscustomobject]@{
                        What    = $packs[0].BaseName
                        File    = $packs[0].FullName
                        Address = ($script:XipBase + (Resolve-LightPartition -Map $map -Family 'data').Start)
                        Family  = 'data'
                }
        }
        return $plan
}

#   The built executable, whose suffix depends on the toolchain: the platform SDK's targets carry
# .elf, the bare-CMSIS ones are plain CMake executables with no suffix at all (their .bin and .hex
# are objcopy'd from this file by the post-build hook).
#
#   BOTH 'module/' and 'test/' are searched: consuming projects put their flashable targets under
# module/, while the framework's own demos live under test/.
function Find-LightTargetElf {
        param([Parameter(Mandatory)] [string]$Tree, [Parameter(Mandatory)] [string]$Target)

        $elf = @("module/$Target/$Target.elf", "module/$Target/$Target",
                "test/$Target/$Target.elf", "test/$Target/$Target") |
                ForEach-Object { Join-Path $Tree $_ } |
                Where-Object { Test-Path $_ -PathType Leaf } |
                Select-Object -First 1
        if (-not $elf) {
                throw "no executable found for '$Target' in '$Tree' (looked for $Target.elf and $Target) -- has it been built?"
        }
        return $elf
}

Export-ModuleMember -Function Find-LightPicotool, Get-LightFlashMap, Resolve-LightPartition,
        Get-LightFlashPlan, Get-LightElfLoadAddress, Find-LightTargetElf
