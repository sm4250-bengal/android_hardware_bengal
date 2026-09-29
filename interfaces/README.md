# `platform/interfaces` — our own AIDL HAL interfaces (`vendor.xyauto.*`)

New directory, created 2026-08-30 by the Phase-4d FM prep. It is the device-3
counterpart of AOSP's `hardware/interfaces`, and it exists because **D18** says
board hardware lives behind a Rust `/vendor` daemon exposed as AIDL:

> Vendor daemon in Rust (`/vendor`), owning MCU UART (serdev at Phase 5 per D15),
> ADAU1701 DSP, QN8035 tuner, backlight/GPIO; exposed as **AIDL HAL(s)**
> (`vendor.xyauto.*`), standard SELinux, VINTF manifest.
> — `docs/decisions/2026-08-29-D18-hu-service-app-and-rust-aidl-hal.md`

**Nothing here is wired into a product yet.** The FM and HUD contracts now have
`aidl_interface` modules, VINTF declarations and the `xyauto-hud` skeleton as
their intended Rust server, but both remain `frozen: false` and no Android
build has generated their API snapshots. Product wiring and the freeze targets
are Phase-4 build-window work; do not manufacture `aidl_api/` by hand.

Layout mirrors the package name, as `aidl_interface` requires:

```
platform/interfaces/vendor/xyauto/fm/aidl/vendor/xyauto/fm/*.aidl
platform/interfaces/vendor/xyauto/hud/aidl/vendor/xyauto/hud/*.aidl
```

The HUD contract and its host-only desk proof are documented in
`docs/research/2026-08-30-hud-aidl-interface.md`. Run
`platform/scripts/hud-aidl-desk-check.sh` to generate both Java and Rust
backends into ignored scratch and validate the VINTF/D18 wiring without `m`.

Rules we hold ourselves to here:

- **One interface per subsystem, not one per chip.** `IFmRadio`, not `IQn8035`.
  The QN8035 is an implementation detail; the tuner could be replaced and the
  app must not care.
- **No raw register access is ever exposed.** The device-1 bridge already made
  this call and it was right: "No raw address/register/value API is exposed to
  Android" (`apps/mcu-toolbox/bridge/tuner_i2c.go`). A HAL that can write any
  I2C register is a HAL that can brick the DSP's parameter image — that has
  happened once already (`mem:dsp-adau1701-daemon-app`, the EQ incident).
- **The HAL owns ordering, the app owns intent.** "Play FM at 97.5" is one call;
  the MCU frame, the analogue mux and the tuner un-mute are the HAL's business
  and must not be separately callable, or two callers will interleave them.
- **Every version is frozen before it ships.** `RELEASE_AIDL_USE_UNFROZEN` is
  false in this release config (see `platform/device/xyauto/bengal3/device.mk`'s
  note on the clearkey HAL being downgraded V2 → V1), so an unfrozen `V2`
  silently becomes `V1` at `assemble_vintf` time. Freeze deliberately; do not
  discover it.
