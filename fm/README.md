# `xyauto_fm`

Host-testable QN8035 core for the single D18 `bengal-hud` vendor daemon. This
directory does not define another daemon or Binder process.

```text
cargo fmt --check
cargo test --offline
cargo clippy --offline -- -D warnings
```

The default feature set is pure Rust. `linux-i2c` enables the real
`/dev/i2c-N` backend, which issues only `I2C_RDWR`; tests use an `I2cBus` mock.
The default-feature service facade mirrors every `IFmRadio` method without
linking Binder. Tune is a two-phase contract: while disabled it only remembers
the channel, and `setEnabled(true)` receives that prepared channel so the
daemon can apply it before the mux/un-mute; while enabled it tunes immediately.

Soong enables `linux-i2c` and the Android-only `binder` adapter, which implements
the generated Rust `IFmRadio` and registers
`vendor.xyauto.fm.IFmRadio/default`. `bengal-hud` remains the only binary and
supplies its composite MCU/DSP/GPIO hardware implementation before joining the
shared Binder thread pool. The exact library/binary split cites
`platform/hardware/xyauto/hud/Android.bp` in this directory's `Android.bp`.

See `docs/research/2026-08-30-fm-4d-daemon.md` for the reuse ledger and the
integration boundary.
