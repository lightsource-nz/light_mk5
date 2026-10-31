# Templates

`project/` is the starting point for a new project built on the framework. Do not copy it by
hand: `scripts/light-new-project.ps1` creates a project from it.

```
scripts/light-new-project.ps1 -Name my_device                 # beside this checkout
scripts/light-new-project.ps1 -Name my_device -Path D:/work   # somewhere else on the same volume
```

What the new project holds: a portable application crate with host tests, a firmware target for
a Raspberry Pi Pico 2 that runs it, both cargo workspaces, the CMake build and presets, wrappers
over the framework's scripts, a CI workflow over the framework's reusable ones, and a README. It
builds, flashes and passes its tests as created.

## Placeholders

The script copies every file under `project/` and replaces three tokens, in file contents and in
file and directory names:

| Token | Becomes |
| --- | --- |
| `__PROJECT__` | the project name (`my_device`): the CMake project, the preset and target names, the crate names `light_app_my_device` and `light_app_my_device_pico2` |
| `__LIGHT__` | the path to the framework checkout, relative to the directory of the file it appears in |
| `__LIGHT_FROM_ROOT__` | the path to the framework checkout, relative to the new project's root (for settings the scripts resolve against the root) |

A file added under `project/` is picked up with no change to the script. Keep the template
building: `light-new-project.ps1 -Name template_check -Path <scratch>` followed by the new
project's `scripts/build.ps1` and `scripts/test.ps1` is the check.
