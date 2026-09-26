# cyw43, vendored

`cyw43` 0.7.0 (MIT OR Apache-2.0, https://github.com/embassy-rs/embassy), the driver for the
combined wireless and short-range radio part that `light-cyw43` hosts. Vendored to carry the change
below; every change is marked `light_mk5:` in the source. `Cargo.toml` is the crate's own manifest,
unchanged.

## Changes

- **`Control::relink()`** (`control.rs`), so a join can be attempted more than once. The commands a
  join issues to set up security are accepted only while the interface is **down**; once it is up
  and configured the part answers those same commands with `BCME_NOTDOWN`, and a refused ioctl is
  fatal in this driver — `Runner` panics on any non-zero status rather than returning it. `up` and
  `down` exist but are private, so from outside the crate the only way to survive a second join was
  not to attempt one. That made a mistyped passphrase a power cycle, and the retry a person
  naturally makes the thing that stopped the board. `relink` takes the interface down and straight
  back up, returning the part to the state a first join finds it in.

  Offered as the pair rather than as two public methods: the order is the whole content of it, and
  half of it is worse than neither — an interface left down is a radio that answers nothing, with
  nothing to say why.

## What was deliberately not changed

- **The panic on a refused ioctl** (`runner.rs`, `Runner::ioctl` — the driver's own `// TODO:
  propagate error instead`). Returning the status instead would mean giving every ioctl call site a
  fallible result, which is most of the crate and all of its public surface; and the failure it
  guards against is a part left in a state this driver cannot reason about, so failing loudly is
  defensible. The change above removes the *reason* the panic was being reached, which is the
  smaller and more durable fix. If a second refusal is ever found on a path `relink` does not cover,
  this is where it will show.
