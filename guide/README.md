# Light Framework — User Guide

This is the how-to documentation for the **Light Framework** by lightsource aotearoa: what you
need to install, how to build and flash the example firmware, how an application is put together,
how to author its interface as data, how to start a project of your own on the framework, and how
to run and test an application with no hardware at all.

It is written for someone building a device on the framework. It assumes you can read Rust and
CMake, and nothing about the framework itself.

## How this relates to the specification

The framework has two document sets:

- **`documents/`** — the [specification](../documents/00-overview.md). It is normative: it defines
  every contract, public surface and invariant, and the code follows it. When you need the exact
  rule, read it there.
- **`guide/`** — this guide. It is derived from the specification and the source code, explains
  rather than defines, and leads with worked examples. When the two disagree, the specification is
  right and the guide has a defect.

Each chapter links to the specification document that covers its subject in full.

## Chapters

| Chapter | What it covers |
| --- | --- |
| [01 — Getting started](01-getting-started.md) | The toolchain, the checkout layout, and building, flashing and talking to the example firmware. |
| [02 — How an application runs](02-how-an-application-runs.md) | The execution model: the C shell, modules and the runtime, the event bus, logging, the console. |
| [03 — Building an application](03-building-an-application.md) | A complete small firmware from scratch, then how it grows into the portable-app / board / instantiation shape. |
| [04 — The interface as data](04-the-interface-as-data.md) | Fonts, themes and UI designs: the files you author, the CMake calls that compile them, and the Rust that drives the result. |
| [05 — Your own project](05-your-own-project.md) | Consuming the framework from a separate repository: layout, CMake, presets, scripts, CI, your own board. |
| [06 — Running without hardware](06-running-without-hardware.md) | Host tests, the simulated board, and the desktop preview and editor. |
| [07 — The scripts](07-scripts.md) | A reference for every script: what it does and what it takes. |
| [08 — Adding hardware](08-adding-hardware.md) | Where a new panel, touch controller, I2C part, board or chip plugs in. |

## The shortest path

```
git clone <light_mk5>  projects/light_mk5
git clone <pico-sdk>   projects/pico-sdk          # beside it
cd projects/light_mk5
scripts/build.ps1 -Target light_pico2               # a Pico 2 firmware
scripts/flash.ps1 -Target light_pico2               # over a debug probe
scripts/console.ps1                                 # watch it talk
scripts/test.ps1                                    # the host tests
```

[Chapter 01](01-getting-started.md) explains each of those lines. To start a product of your own
rather than build the framework's examples:

```
scripts/light-new-project.ps1 -Name my_device       # a working project beside the framework
```

[Chapter 05](05-your-own-project.md) explains what it creates.
