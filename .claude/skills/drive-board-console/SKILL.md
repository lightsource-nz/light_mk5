---
name: drive-board-console
description: Send commands to a connected Light Framework board and read its replies over the USB-CDC serial console, non-interactively. Use to hardware-verify firmware, read `stats`, issue CLI commands (backlight, sd, rtc, touch, …), or capture boot/runtime logs from a flashed board — anywhere you need to see what the real hardware does over serial.
---

# Drive a board's serial console

A flashed Light Framework board exposes a CLI and a log stream over USB-CDC. Drive it through the
framework's console script — never hand-roll a `SerialPort`, which comes back silently empty without
the DTR handshake below.

## Send commands and read the replies

From the project root (the firmware repo, e.g. `light_mk5`), in PowerShell:

```
./scripts/console.ps1 -Send "stats"                      # one command, prints its reply
./scripts/console.ps1 -Send "stats","backlight 800"      # several, in order
./scripts/console.ps1 -Send "sd" -Until "boot signature" # stop early once a pattern appears
./scripts/console.ps1 -Send "stats" -Quiet               # suppress live echo; just return the text
```

`-Send` writes each command with a trailing newline (Enter to the firmware's line reader), drains the
boot output once so the transcript is just the replies, and waits for each reply to arrive and go
quiet (capped by `-SettleMs`, default 1500). `-Out <file>` saves the transcript.

## Capture output without sending

```
./scripts/console.ps1 -Seconds 10                # capture 10s of logs
./scripts/console.ps1 -Seconds 30 -Until "ready" # capture until a pattern or timeout
```

## Flash first, if needed

```
./scripts/flash.ps1 -Target <name>   # builds if needed, then flashes (uf2 over BOOTSEL, or swd)
```
Targets and their flash method are in `scripts/project.config.ps1`.

## What to know

- The board must be **running** firmware with a live console — a CDC port at VID `2E8A` PID `0009`.
  In BOOTSEL it has no console; if it halted, its USB stack is gone. The script errors clearly if no
  such port is found.
- The script **asserts DTR** (pico-sdk treats DTR, not port-open, as "connected"); without it a
  connect-wait firmware sits forever and the capture is empty — the usual cause of "dead board".
- Reads are **buffered** so the observer does not stall the firmware's stdout. Do not replace this
  with a per-line reader.
- One board at a time (the finder takes the first CDC port).
- Each log line's **source prefix** names the module that emitted it (e.g. `light_input::module`,
  `light_power_manager::module`, `light_rtc::module`) — useful for confirming which module is running.
- These are the framework scripts; run them from the firmware repo root. See the
  [`refer-to-spec`](../refer-to-spec/SKILL.md) skill for the spec that governs the code.
