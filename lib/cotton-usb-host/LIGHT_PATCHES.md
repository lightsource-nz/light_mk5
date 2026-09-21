# cotton-usb-host, vendored

`cotton-usb-host` 0.3.0 (CC0-1.0, https://github.com/pdh11/cotton), the async USB host stack the
RP2 port's host role runs on, behind `light_midi::Host` in `light_rp2::usb_host`. Vendored to
carry the changes below; every change is marked `light_mk5:` in the source. `Cargo.toml` is the
crate's own manifest, unchanged.

## Changes

- **The control endpoint's pending path no longer clears the completion flags**
  (`host/rp2040.rs`, `Rp2040ControlEndpoint::poll`). The flags were read, found clear, and then
  written clear again; a transfer completing between the read and the write was wiped before
  anyone saw it, and the control transfer -- and the whole device-event stream queued behind it,
  hub status pipes included -- never resolved. Woken from the interrupt the window is rarely hit;
  polled from a runtime loop it was hit within a few enumerations (a hub replug, then a device
  behind it silently unplugged with no event). The flags are cleared at the start of a transfer
  and when its completion is consumed, which is enough.
- **An RP2350 host controller** (`host/rp2350.rs`, feature `rp2350`): the RP2040 module
  retargeted at `rp235x-pac` -- the controller is the same block with the same registers -- plus
  the one bit the RP2350 adds: `MAIN_CTRL.PHY_ISO` resets set and a `modify` keeps it so; it is
  cleared, or a fully configured controller never sees the bus.
- **`Topology::parent_of(device)`** (`topology.rs`): the parent array was private and the hub
  address and port a device sits on are what a status display shows.
