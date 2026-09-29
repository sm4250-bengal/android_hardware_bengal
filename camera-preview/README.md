# Bengal native camera bridge

Root-owned V4L2 bridge for Camera Diagnostics and a protected reverse view.
Not a Camera2 HAL or an enforcing-production service. Uses the measured capture
tool: TP2855 input3 / CVBS IN 1 → CSIPHY0 → CSID0 VC3 → VFE0 RDI0.
Requires the paired camera-enabled boot/DT and measured CAMSS/TP2855 modules.

Build from a committed checkout:

```
bash platform/scripts/build-camera-preview.sh COMMITTED_SOURCE_SHA
```

Lead deployment resolves `com.meinte.hucamera`'s UID from PackageManager,
checks the binary hash and current device identity, then starts the binary
with `--uid APP_UID --policy-uid BENGAL_POWER_UID`. Resolve both UIDs; do not
guess them or expose a TCP listener. `--reverse-line 42` selects ONLY the OEM-
derived candidate on the 114-line 500000.pinctrl chip, refusing used/output
lines. It requests input/edge events without changing bias; no sweep, output,
or wake configuration. The old OEM global1209 is not a new-kernel line number.
Physical trigger correspondence still needs an observed wire OFF/ON pair.
The abstract local socket `bengal-camera-preview-v1` requires that exact
SO_PEERCRED UID and an explicit BCP1 (manual) or BCR1 (reverse) handshake.
Capture additionally requires the authenticated ACC permission below.
The app verifies the server's root UID and checks its own CAMERA permission
and AppOp, including Android's camera privacy restriction. This diagnostic
trusts the installed app; it is not a general-purpose camera broker.

Capture1280×720 UYVY25fps, first8 frames skipped; everysecond frame becomes
640×360 little-endian RGB565. Fixed-length network-order headers, bounded
pixel allocations, two-second socket timeout. No audio, encoding or video
file storage. Stops/STREAMOFF on disconnect and idles with no camera FDs.
SIGTERM also cleans up. Client STOP and Activity pause disconnect. The old
ten-minute diagnostic cutoff is replaced by coordinated lifetime/revocation.

## ACC and reverse policy

The separate abstract `bengal-camera-policy-v1` accepts only BengalPower's UID.
All packets are three network-order uint32s: magic0x42434331, nonzero increasing
generation, awake0/1. Replies echo generation with flags1=allowed,2=capture
busy,4=reverse known,8=reverse asserted. Unsupported values, replay, EOF or
connection loss revoke capture. Nothing is allowed before the first packet.
Receipts indicating idle are sent only after STREAMOFF and FD cleanup.

BengalPower consumes its already validated BACP events: GRACE/PREPARE hold,
RESUME or awake SNAPSHOT release. Its existing peripheral readiness includes
a matching-generation held+idle receipt. Admission and the held receipt share
a native mutex, so a stream cannot start after the last quiet check. A held
automatic view closes; resume re-reads the actual input and opens only if high.
The view is protected by DEVICE_POWER and returns to the previous task on exit.
No audio-focus, DSP gain, route or MCU packet is changed by camera policy.

The manual pilot latch `sys.bengal.camera_policy=true` resets at cold boot.
The persistent init deployment instead uses `persist.sys.bengal.camera_service`.
Only set it after installing the rc, launcher, binary and paired module drop.
Init starts the launcher after Android boot completion; it resolves both UIDs,
checks the known module hashes and camera DT nodes, loads absent modules and
execs the broker. No ADB-launched helper survives ACC's adbd cgroup cleanup.

`BENGAL_CAMERA_PREVIEW=1` selects these files in the next full build; use
`platform/scripts/make-device-camera-drop.sh CAMSS_KO TP2855_KO` first. The
build runner forwards this default-off boolean. The launcher is intentionally
restricted to the existing **userdebug/eng + permissive** bring-up profile and
the init service uses that profile's su domain. This adds no policy relaxation
but is NOT a dedicated enforcing service/domain or release-security approval.
The product refuses a user build. Enforcing policy/Camera2 remain open.

Reverse sensing is a separate explicit runtime selection:
`persist.sys.bengal.camera_reverse_line=42`. Empty/-1 disables it while retaining
manual video and ACC cleanup. The selection is not a product default until the
physical wire mapping is confirmed. Changing it requires a service restart.

Strict GKI, OEM HAL exclusion and existing power/graph ownership preserved.
Camera2 support is not claimed by this component.
