# Initial generated vehicle controls

## Prior-art census

Reused the CAN search across all four archived device-1/2 trees and the
current CAN research. `archive/reference/device12/hardware/xyauto_can/README.md:36`
establishes the overloaded baud selector and default Raise envelope; its
stock tty numbering is historical. The current device-3 transport remains
the existing SE5 runtime. `docs/research/2026-09-11-can-vw-pq-parity-plan.md`
supplies the existing XinPu decoder/amp oracle. No existing RX profile is edited.
`docs/research/2026-09-14-can-oem-automation.md:60` supplies the common JSON
compiler and complete cached DEX census. This slice resolves helper/model
linkage for two exact APKs; it does not activate the other intermediate records.

## Scope and identities

`controls.toml` is executable scalar control data embedded by the runtime.
Both profiles are partial and vehicle-unverified. The seven amplifier controls
operate the **vehicle amplifier**, independently of Android volume and the
head unit DSP. No OEM executable or resource file is shipped.

| Profile | Exact selection / maker / brand | Included controls |
|---|---|---|
| xinpu-vw-pq | 2009002 / 2 / 9 | Seven vehicle amplifier ranges |
| raise-vw-pq | 1001001 / 1 / 1 | Radar sound and parking mode choices |

`provenance.json` pins the APKs, assets, compiler and emitted bundle by SHA-256.
The compiler rejects any changed APK before translation: helper similarity
cannot silently extend this allowlist. Catalogue availability reads the exact
selection/maker/brand bindings from this executable bundle. Missing/ambiguous
catalogue entries remain unavailable.

## OEM linkage

The canonical APK root is
`archive/reference/device12/hardware/firmware/reference/oem-original-unpacked/system/system/media/CanBus/plugin`.
Paths below are Java class paths under the retained decompilation `sources/`.
They identify local OEM evidence, not code incorporated into this project.

### XinPu 2/9

Retained decompilation: `kernel/out/.tmp/can-parity-plan/jadx-p1_301_2_9/`.

- `com/kyhero/car/myplugin/canbus/CanbusJson_P.java:116,144`: amp resource
  selection searches `car_amp_[0-9]{7,8}.json` for the decimal selected user.
  The matching asset is `assets/car_amp_2009002.json`.
- `com/kyhero/car/myplugin/activity/AmpActivity.java:142` loads that asset;
  lines 100 and 114 pair controls and statuses by **list index**. The TX is
  type, command, min plus the requested slider position. The generated ranges
  use that absolute wire value.
- `com/canbus/json/utils/JsonUtils.java:419` parses protocol type/length/command
  as hexadecimal; payload indices and bit positions are decimal.
- `com/kyhero/car/myplugin/canbus/CanbusConfig_P.java:14` returns 38400.
  `com/canbus/utils/ToolClass.java:769,818` builds TYPE,LEN,DATA, then the default
  `2E` prefix and low-byte complement checksum.

| Control | TX type / command | Allowed wire values | RX type / length / index |
|---|---|---|---|
| amp.volume | A0 / 00 | 0..30 | 27 / 8 / 1 |
| amp.balance | A0 / 01 | 0..18 | 27 / 8 / 2 |
| amp.fader | A0 / 02 | 0..18 | 27 / 8 / 3 |
| amp.bass | A0 / 03 | 0..18 | 27 / 8 / 4 |
| amp.mid | A0 / 05 | 0..18 | 27 / 8 / 5 |
| amp.treble | A0 / 04 | 0..18 | 27 / 8 / 6 |
| amp.speed | A0 / 06 | 0..7 | 27 / 8 / 7 |

Every amp RX field uses bits 0..7. Mid and treble commands are deliberately
nonsequential, as in the asset. Example volume30 frame: `2e a0 02 00 1e 3f`.

### Raise 1/1

Retained decompilation: `kernel/out/.tmp/can-profile-compiler/jadx-1-1/`.
The cached method inventory is
`kernel/out/.tmp/can-oem-automation-lead/run-all-002/per-plugin/1_1_P1_301.json`.
Its preparation receipt identifies DEX SHA-256
`2e0c31cf8801883c2c33d324f68f488214c203d57d9fea7b6b63619adeb73a25`.

- `com/kyhero/car/myplugin/canbus/CanbusUser_P.java:17` binds 1001001 to
  `Raise_Dasauto_PQ`. The catalogue independently binds maker1/brand1/model1.
- `CanbusJson_P.java:74,144` chooses `assets/car_set_1001001.json` by selected
  decimal user. `activity/CarSetActivity.java:75` loads that selected asset.
- `com/canbus/json/utils/JsonUtils.java:305` parses that schema, disabling RX
  when `rxDataType` is empty. `CarSetActivity.java:136` and its dialog listener
  send each selected choice directly through `ToolClass.sendBroadcast` with
  the resource type, optional command and byte value. The matching cached
  listener is `CarSetActivity$4.onClick`, normalized code hash
  `e8c85627baf220fa19ed8d407fb252159219600ce3388f216142e7031c1f5fe2`.
- `CanbusConfig_P.java:14` returns 38400 for all models in this APK; cached
  DEX proves the integer constant `0x9600`. `com/canbus/utils/ToolClass.java:769,818`
  implements the same TYPE,LEN,DATA/default `2E` complement-sum envelope.
- Radar sound is C6 command00, values0/1, observed as type25 length2 index0
  bit0. Parking mode is C6 command02, values0/1, **write-only**. No optimistic
  confirmed state may be inferred from sending it. Example on frame:
  `2e c6 02 00 01 36`; parking mode1: `2e c6 02 02 01 34`.

### Startup boundary

Raise `CanbusPattern_P.java:30` schedules a model-code task whose only TX
branch is sentinel selection1000000001. `initCurrentCanbus` at line326 starts
periodic queries only for1001006/1001007. Neither branch applies1001001.
The two settings listeners have no model-code or amplifier-state prerequisite.
This establishes a direct software path for the bounded settings slice.

The OEM also replays saved amplifier values at startup/ACC-on:
`CanbusPattern_P.java:259` (`sendCanbusAck` -> `send_amp` for1001001/1001011),
and sends generic81 on ACC-on at line304. These actions are deliberately outside
this user-action-only slice, agreed with the lead. No startup sequence,
calibration, stored amplifier replay, automatic refresh or framework local DSP
action is emitted. The Raise profile must never borrow the XinPu six-request
refresh or its dashboard decoder merely because the envelope matches.

## Reproduction

From the assigned worktree, with Python3.11 or newer:

```sh
python3 platform/scripts/can-oem-runtime-profiles.py \
  --oem-root /home/meinte/android/bengal/archive/reference/device12/hardware/firmware/reference/oem-original-unpacked/system/system/media/CanBus/plugin \
  --output-dir platform/hardware/xyauto/hud/can/generated --check
python3 platform/scripts/test-can-oem-runtime-profiles.py
python3 platform/scripts/test-can-vehicle-catalogue.py
```

Omit `--check` to regenerate only the two generated data files. No full APK
census/decompilation is required. The focused tests cover wrong helper identity,
byte bounds, incomplete lowering, incompatible RX/TX choice values, deterministic
output, exact maker/brand binding and the actual nonsequential amp commands.

Desk cannot answer: whether a particular installed CAN box requires an81
handshake before accepting these settings, whether the vehicle has the relevant
amplifier/features, or whether it physically acknowledges/applies a host write.
Those unknowns remain labelled unverified rather than blocking the implemented
partial slice or triggering speculative startup writes.
