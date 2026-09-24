# Secure boot and field update

Firmware is **signed when it is built and verified by the hardware before it runs** — every boot, not
only when it is programmed. An update is delivered as a whole image into a slot the running firmware
is not executing from, and becomes the image that boots only after it has proved itself.

The hardware is the root of trust: it verifies the first thing it runs, against a key it holds in
one-time memory. That first thing is a small **bootloader** the framework supplies, because hardware
that verifies an image does not thereby choose between two of them — a device with an A/B pair needs
something to compare the slots' versions, pick one, and hand over. The bootloader does only that, on
the chip's own facilities, and is itself verified before it runs.

```mermaid
graph TD
    rom{{"hardware: verify vs key hash<br/>in one-time memory"}} -->|valid| boot["bootloader<br/>(signed, carries the flash map)"]
    rom -->|invalid| stop(["refused: nothing runs"])
    boot -->|compares slot versions| pick{"pick a slot"}
    pick --> a["slot A"]
    pick --> b["slot B"]
    a --> chain["verify and hand over"]
    b --> chain
    chain --> probation["the application runs,<br/>on probation"]
    probation -->|commits itself| settled(["the image that boots from now on"])
    probation -->|never commits| previous(["discarded: the previous image boots"])
```

*Immutable hardware verifies the bootloader; the bootloader picks between the slots and verifies
what it hands over to. A new image is on probation until it commits itself, so an image that cannot
run is discarded rather than kept.*

---

## Responsibility

Define what is signed and when it is checked, how an update reaches a device without risking the
image it replaces, how a version is prevented from returning once it is revoked, and where assets
live so that a look-and-feel change is not a firmware change. The signing itself belongs to the
build ([10-build-and-release.md](10-build-and-release.md)); the verification belongs to the port's
chip facility ([07-ports-and-shell.md](07-ports-and-shell.md)); this document is the contract
between them.

## Public surface

- **Build.** `light_seal_image(<target> [VERSION] [ENCRYPT])` signs a target's image and stamps the
  version a device compares between slots; `light_bootloader_map(<bootloader> LAYOUT <json>)` embeds
  the flash map in the bootloader that reads it, so the two are one signed artefact. Both default to
  the development key, overridden for a release.
- **The bootloader** is a firmware target of the framework's, built per chip family: it loads the
  map, picks the better of an A/B pair, and chains to it. An application names no part of it.
- **Assets.** The blob helpers (`light_add_font`, `light_add_theme`, `light_add_ui`) emit into the
  data partition rather than into the firmware image; the port resolves a partition to a
  `&'static [u8]` at runtime, which is what the portable readers already take.
- **Update.** The port offers staging an image into the inactive slot, a reboot that asks the
  hardware to select it, and the **commit** an application calls once it is satisfied with itself.
  An application supplies the self-test that decides whether to commit.

## Behaviour and invariants

- **The root of trust is immutable hardware, never flash.** A public key's hash lives in one-time
  memory; the verifier is the chip's own boot facility. Anything stored in flash can be replaced by
  whoever can write flash, so nothing in flash can be the root.
- **Verification is at every boot, not at programming.** Programming-time-only checking is bypassed
  by anyone who can write the flash directly, and gives an assurance it does not hold.
- **An update never writes the image it is running.** Staging targets the inactive slot, and the
  hardware enforces the permission, so a fault in the update path cannot destroy the working image.
- **A new image runs on probation and must commit itself.** Until it does, a reset returns to the
  previous image. An image that hangs, panics or cannot drive its display is therefore self-
  discarding: the failure mode of a bad update is a device still running the old firmware.
- **Versions do not come back.** Each image carries a version; a counter in one-time memory records
  the oldest version still accepted, so an image whose flaw has been fixed cannot be re-presented.
- **Assets are signed as their own partition.** Fonts, themes and interfaces update independently of
  firmware and carry their own authenticity; a restyle ships without a new firmware image, which is
  what the data-asset formats exist for.
- **Encryption is the application's choice, and it costs RAM.** Authenticity is always on;
  confidentiality is opt-in per application. An encrypted image is decrypted to RAM and executes
  from there, so it spends its own size in RAM — a board whose framebuffer already dominates RAM
  cannot take it, and this is a per-board fact to check before promising it.
- **Development and production keys are different, and the development key is public.** It lives in
  the repository, protects nothing, and exists so that the whole path — sign, verify, update,
  commit — is exercisable by anyone with the source. The production key never leaves its secret
  store.
- **A shipped device has no debugger.** Closing debug access is part of securing a unit, so the
  console, the panic relay and the fault recorder are the only diagnostic channel a field unit has;
  they are not optional comforts.

## Notable design decisions and constraints

- **The bootloader exists because choosing is not verifying.** Hardware verifies the one image it is
  pointed at; it does not compare two slots' versions and elect one. So the framework supplies a
  bootloader that does exactly that and nothing else — load the flash map, pick the better of an A/B
  pair, hand over — on the chip's own facilities, itself signed and verified before it runs. It is
  kept deliberately small and free of application concerns, because it is the one image a device can
  never recover from by an update.
- **The flash map travels inside the bootloader.** The map and the code that reads it are one signed
  artefact, so a device cannot hold a map its bootloader disagrees with, and verifying the
  bootloader verifies the map. The first slot therefore begins after the bootloader, not at the
  start of flash.
- **The bootloader's scratch memory lies outside the memory the chosen image claims.** Handing over
  is not a jump: the chip's boot facilities place the image's own initialised memory before they
  enter it, and an application's memory begins at the bottom of main memory — exactly where a
  bootloader's variables are. Scratch lent to those facilities and left in main memory is therefore
  overwritten part-way through the hand-over by the very image being launched, and what the hardware
  does next depends on how closely it is watching its own bookkeeping. So the bootloader's work area
  goes somewhere no image loads: a peripheral's memory, or whatever region the part's own boot code
  uses for the same purpose.
- **The bench path and the field path are the same mechanism.** An update is delivered through the
  chip's ordinary image-download route, routed to the right partition by the image's family, so a
  developer's flash and a field update differ in who initiates them, not in what happens.
- **Over-the-air delivery is not in scope yet.** The contract above names no transport, so a
  transport is added without revisiting any of it.
- **Making a device secure is irreversible and belongs to manufacture.** Writing the key hash and
  enabling verification cannot be undone; a development board is either left open or given the
  development key, and no bench procedure writes one-time memory.

### Reference implementation — the RP2350 port

The part-specific facts live with the port ([07-ports-and-shell.md](07-ports-and-shell.md)); in
outline: images are signed with **secp256k1/SHA-256**, and the boot ROM verifies what it finds at
the start of flash against a **SHA-256 of the public key held in OTP** once secure boot is enabled
there. **The ROM boots the image at the start of flash and does not search the partitions for one**
— it is the bootloader there, with the map embedded in it, that loads the map, picks the better of
the A/B pair (the ROM offers the comparison as a routine) and chains to the chosen slot, which the
ROM verifies in turn. An image may be marked **try-before-you-buy**, which runs it on probation
until the firmware calls the ROM's buy routine; the ROM also offers partition-permission-checked
flash operations for staging, and a reboot that names the slot to boot. Downloads are routed to a
partition by **UF2 family**, which is what makes the data partition separately updatable. A
rollback counter, a glitch detector and debug-disable all live in the same one-time memory.

Two details of the hand-over are worth naming, because both are silent when they are wrong. The
ROM's scan routines want a work area from the caller, and the bootloader gives them **the USB
controller's packet memory** — the region the ROM uses for its own boot scan, and the reason it can
apply an image's load map (which covers the bottom of main memory) without destroying what it is
still reading. A work area in the bootloader's own variables is overwritten by that load map and the
redundancy coprocessor stops the chip with a non-maskable interrupt, no message and no return code.
And the A/B comparison has two forms: the plain one, and a "during update" wrapper that protects a
pending buy's bookkeeping. The wrapper judges the chosen slot by reading the ROM's work area at
fixed offsets, and outside a flash-update boot that judgement reports no valid image; the bootloader
therefore uses the wrapper only on an update boot, where it is what the wrapper is for.
