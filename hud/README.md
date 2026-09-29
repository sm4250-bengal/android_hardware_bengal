# `bengal-hud` — the device-3 vendor daemon (D18)

**The daemon is built into bengal3 and starts in `class hal`.** Its hardware
core also builds and tests at the desk, offline, in about a second — that is
the point of the layout.

```
cargo build --offline          # the core, zero dependencies
cargo test  --offline          # host tests, no device needed
tools/frame-gate.py <artefact> # refuse an MCU frame the allowlist does not name
../../scripts/hud-binder-desk-test.sh # compile/run Binder death/failure controls with stubs
```

## What this is

D18: *"Vendor daemon in Rust (`/vendor`), owning MCU UART, ADAU1701 DSP, QN8035
tuner, backlight/GPIO; exposed as AIDL HAL(s) (`vendor.xyauto.*`), standard
SELinux, VINTF manifest."* One process, because the subsystems are not
independent: selecting FM is an MCU frame, then the analogue mux, then a tuner
un-mute, **in that order**. Two processes doing halves of that is
`mem:dsp-arm-token-is-global` happening again.

## Layout, and why it is split this way

| path | builds with | why |
|---|---|---|
| `src/lib.rs` + `src/{i2c,fm,mcu,dsp,gpio,usb,service}.rs` | `cargo` **and** Soong | the hardware/service core: zero dependencies, so the desk can iterate on it in a second instead of a build window |
| `src/binder.rs` + Binder half of `src/main.rs` | **Soong only**, behind `binder` | needs `libbinder_rs` and the Soong-*generated* `vendor.xyauto.hud-V1-rust` crate |
| `Cargo.toml` | desk only | leaves `binder` off by default; Cargo compiles an inert main and cannot reach Android or board hardware |
| `Android.bp` | Soong only | `rust_library` + `rust_test_host` + `rust_binary` |
| `src/can/` + `can/profiles/*.toml` | `cargo` **and** Soong | Default-off CAN RX thread after the CPU-latency watcher. `persist.vendor.bengal.hud.can.enable` absent/false opens no tty or capture. Production requires positive SE5 MMIO `0x4a94000` identity and exact `/dev/ttyHS3`; its descriptor is readonly with symlinks refused. Exact CAN SELinux type, 0440 system:system DAC, read-only grant and TCGETS/TCSETS only. Installed XinPu data profile stays unverified. Decoded wheel keys trigger nothing unless `persist.vendor.bengal.hud.can.keys` is true; then `src/can/keys.rs` taps VOLUME_UP/VOLUME_DOWN only (DOWN once, LONG bounded repeat, UP never) through the bezel path's `ro.vendor.bengal.hud.inject_keys` gate (parity plan §4.1 step A). |
| `src/bin/bengal-can-{capture,decode}.rs` | desk only (Cargo-only; Soong never sees `src/bin/`) | the same `select_can_path → open_readonly → Reader::run` chain over a pty or a file, and the hex decoder. `--dev-dir` moves the accepted-fallback shape rule off `/dev` so a harness exercises it instead of bypassing it. Exit codes: 2 usage, 3 port selection REFUSED, 4 open failed, 5 read loop failed |
| `tools/oem-can-decoder-agreement.py` | desk only | differential-tests `bengal-can-decode` against the OEM `canbox_rx_uart()`, lifted by name out of the decompiled `CanbusMonitor.java` into a temporary Java harness (needs `javac`/`java`). Five synthetic streams agree; one pins a recovered OEM bug — after a frame the Java loop advances by the length *and* increments, so it misses a second frame whose header is immediately adjacent, and we accept both. It builds and runs the binary in the target directory cargo actually writes to, honouring `CARGO_TARGET_DIR`, and prints the resolved path + sha256 + mtime. `python3 tools/oem-can-decoder-agreement.py` |
| `platform/scripts/hud-can-desk-test.sh` | build preflight, custom scratch | 13 controls: runtime Rust tests plus the 12 existing socat PTY/replay controls, including zero TX with a planted-byte control. `check-bengal3-can-tty.sh` checks the product contract and executes exact-label precedence with Android host `checkfc` in both file orders; its companion has 24 controls. |

**Nothing in `src/can/` has ever seen a byte from a decoder box.** Every vector is
synthetic and derived from the decompiled OEM plugin; every profile field is
`verified = false`; whether device 3 even has a box wired is not on record. The
decoder is proven; the car is not.
| `tools/frame-gate.py` | either | D15 §10.5, ported from `build-initramfs.sh` |
| `platform/scripts/hud-binder-desk-test.sh` | host | compiles the real crate against a generated AIDL stub (from the frozen aidl_api snapshot the Android.bp pin names) plus an ABI-shaped libbinder_rs stub at --extern boundaries, under the Soong vendor lint set, and executes the Binder death/failure controls and cross-checks Lineage's `IBinderInternal`; `m bengal-hud` remains the final Android compile |

Runtime retries discovery/open failures at most every 30 seconds and reports
changed failures once. Disable or ACC-off stops RX through the existing one-second
tty read timeout. The JSONL file under `/data/vendor/bengal-hud/can-capture` is the
evidence sink; kmsg/liblog only carry summaries. One capture per process is bounded
to 8 MiB across reconnects and enable toggles. Exhaustion or write/sync failure
reports `canuart: capture stopped: ...; RX continues` once and closes capture;
old user files are never deleted. Restarting the daemon starts a new budget, so
this is a per-process bound, not global retention. Failed capture creation also
disables capture for that process. No DT enable, product property default, CAN
ACK, transmit API, key injection or vehicle action is introduced.

The dependency boundary is explicit: default-feature Cargo builds the complete
trait-based `HudService`, while Soong turns on `binder` for the generated-AIDL
adapter and daemon entry point.

`IHud` V1 has one callback slot. The adapter links the incoming remote Binder's
death recipient before publishing it, tags both queued notifications and death
handlers with an identity/generation pair, and calls remote methods only after
releasing the core and registry locks. A stale death or failed delivery can
therefore remove only its own callback, never a later replacement.

## The rules the code enforces, not just documents

Each of these is a test, because a comment can be deleted without anyone
noticing:

- **`i2c::adapter`** resolves a bus by name, never by number. The tests use the
  two mappings this board really produced — `i2c-fm` was `i2c-0` on D3-0015 and
  `i2c-1` on D3-0043 — and one test asserts what a hard-coded `1` *would* have
  picked on the older boot (`i2c-aux`).
- **`i2c::rdwr`** issues `I2C_RDWR` and nothing else, and asserts the
  `struct i2c_msg` size so a padding mistake cannot silently address the wrong
  chip. `I2C_SLAVE` is absent by construction; sepolicy's `allowxperm` grants
  `0x0707` alone.
- **`mcu`** permits only the fixed version query and two ACC lifecycle replies, and a test fails
  if that set grows without updating `tools/frame-gate.py`. The denylist is on
  the command byte, not the whole frame, so padding cannot walk around it. The
  SE0 session gates at its single write site; PTY tests cover exact Go-fixture
  bytes/state transitions and prove every forbidden shape writes zero bytes.
- **`gpio`** carries device 3's own pin map (mux A = TLMM 36, B = 37, DAC enable
  83, amp 93, ADAU 3V3 106, reset 107 — read out of device 3's stock DT, not
  carried across from device 1) and refuses the panel, backlight and touch
  lines outright.
- **`dsp`** writes nothing. The EQ addresses are recorded and disarmed; the
  incident that made that necessary is in `mem:dsp-adau1701-daemon-app`.

## CPU wake-up latency hold (D3-0120)

`src/cpu_latency.rs` (cfg-free, 14 host tests) + the `xyauto-cpu-latency` thread
in `src/binder.rs`: while the DRM connector's `dpms` says the panel is on and the
MCU has not confirmed ACC off, the daemon keeps one fd open on
`/dev/cpu_dma_latency` at `vendor.bengal.hud.cpu_latency_us` (default 500 µs,
`0` = off, re-read every second), which keeps the two `cpu-sleep` idle states
(exit latency 621/666 µs) out of the frame pipeline's cross-CPU hops. Measured
reason and the kernel citations (why an fd still open across suspend changes
nothing about standby): `docs/research/2026-09-05-cpu-latency-hold-design.md`.
Node label `vendor_bengal3_latency_device` (`cpu_latency_types.te`), mode from
`ueventd.bengal3.rc`. One kmsg line per transition (`cpu-latency: hold|release`),
never per tick.

## What it does not do yet

**CORRECTED 2026-09-02 13:52 CEST (agent `mcu-bridge-prep`, from the D3-0098 flight).**
This section used to say the daemon "has deliberately not been compiled with Soong or started"
and that `bengal-hud.rc` "still ships `disabled` with no trigger". **Both are out of date, and
all three start gates below are now met.** Measured on
`kernel/out/probes/D3-0098/logs/adb-live-boot1/`:

* it is built and installed — `PRODUCT_PACKAGES += bengal-hud`
  (`platform/device/xyauto/bengal3/device.mk:758`);
* it **runs** — `ps.txt:299` `system 654 1 … S bengal-hud`; `getprop.txt:195`
  `[init.svc.vendor.bengal-hud]: [running]`; started at 52.13 s of boot (`getprop.txt:471`);
* `bengal-hud.rc` has **no `disabled` line** — it is `class hal`, `user system`, `oneshot`.

The three gates, as measured on that boot:

1. ~~`/dev/i2c-N` exists~~ — **met.** `i2c_dev` is module 63 of the second-stage load
   (`dmesg.txt:725-727`, `i2c_dev: i2c /dev entries driver`). Four adapters enumerate:
   `i2c-touch`, `i2c-fm`, `i2c-aux`, `Qualcomm-CCI` (`i2c-names.txt`).
2. ~~the device types are in a shipped policy~~ — **met.** `vendor_bengal3_mcu_device` is
   declared (`phase4d_types.te:34`), `/dev/ttyHS[0-9]+` is labelled with it
   (`file_contexts:123`), and this daemon's domain holds `rw_file_perms` on it
   (`bengal_hud.te:175`).
3. ~~the interfaces are frozen at V1~~ — **met.** `vendor.xyauto.fm` (`8b43c4f`) and
   `vendor.xyauto.hud` (`7da90a8`).

**This gap is now implemented but remains desk-only until the next vendor
build:** the Binder adapter starts one reader thread, requires one delimited
MCU SE0 MMIO identity and opens it at raw 115200 8N1. Ambiguous matches and a
fallback positively identified as Bluetooth SE3 are refused. The rc-selected
`/dev/ttyHS1` is only a loud fallback when sysfs proves nothing; a changing tty
suffix can never override an MMIO result. The daemon sends the fixed version
query and ACC-on answer through the sole allowlisted TX site, and logs every
complete receive frame to both logcat and kmsg with monotonic milliseconds,
checksum verdict, decoded event and raw hex. Read failures close and retry the
one owner after two seconds.

The same thread contains the D15 uinput path for HOME, BACK, VOLUMEUP and
VOLUMEDOWN; Power is absent by construction. The device-3 map is explicitly
`PROVISIONAL`, all four raw-code slots are empty, and
`ro.vendor.bengal.hud.inject_keys=0`. Setting the property alone cannot inject:
the daemon refuses until Capture B supplies the map and the source confirmation
bit is reviewed. `IHudCallback.onMcuKeyEvent` remains the Android policy path.

One caveat on the sepolicy line above: every Lineage boot so far has run
`androidboot.selinux=permissive` (`getprop.txt:373`), so "no denial" is not yet evidence of a
correct policy.

Full write-up, including the build graph and what waits for the Android build:
`docs/research/2026-08-30-hud-daemon-skeleton.md`.
The typed vehicle/USB/key/GPIO contract and its desk proof are in
`docs/research/2026-08-30-hud-aidl-interface.md`.
