# DSP settings persistence

## Prior-art census

The four device-1/2 reference trees and imported ACK patches were searched for
DSP/ADAU/IC60 persistence, saved gains and reboot behavior. Relevant reuse:
The retained census has 271 matching lines, including ignored log/evidence
files (`kernel/out/.tmp/dsp-persist-20260915/prior-art-census.txt`).

- `archive/reference/device12/docs/ADAU1701-DSP-MANUAL.md:38`: DSP power loss
  requires Android-side settings replay. Family evidence, not device-3 proof.
- `archive/reference/device12/handoff/2026-07-27_aidadsp-and-persistent-ic60-review.md:23`
  and archived STATUS/HISTORY entries describe the vendor's raw `adaudate`
  persistence and its corruption. Do not reuse that raw RAM format.
  Archived `handoff/STATUS.md:2822` explicitly rejects copying that replay.
- `archive/reference/device12/handoff/boot-logs/dev1-0370-acc-measure-20260826-2325/dmesg.txt:407`
  records verified IC60 load. The older
  `handoff/evidence/2026-08-09-mainline-boot/twrp-dmesg.txt:2092` records SELinux
  denial on kernel-owned `adaudate` persistence; use the established HUD data
  directory and userspace owner instead. These are narrow historical excerpts,
  not new whole-log or device-3 acceptance claims.
- `archive/reference/device12/hardware/dsp/adau1701/SM4250-OEM-EQ-CORRECTION-2026-07-27.md:31`:
  persisting speculative EQ cells preserved the earlier loud-noise fault.
  Reuse only the current typed guard's supported output/EQ/enhancement controls.
- Archived `kernel/xyauto-sm4250` notes contain the earlier bring-up/power work;
  `kernel/patches/bengal/0033-checkpoint-preserve-pre-4.19-suspend-port-work.patch:109`
  specifies the single guard owner and verified source-before-amplifier contract.
  Keep that owner and use its existing activation boundary.
- Device-3 `docs/research/2026-09-14-shared-audio-master.md`, section “Live
  result”: boot reset all five output gains to 0 dB and required typed replay.
  `2026-09-14-shared-output-gain-editor.md` assigns shared output calibration to
  Radio; `2026-09-14-muted-dsp-setting-changes.md` preserves ordinary update mute
  safety. Neither is replaced by this semantic tuning store.

## Owner and lifecycle

Production HUD constructs `DspControls::with_persistence` before starting any
audio activation or ACC worker. It loads
`/data/vendor/bengal-hud/dsp-settings-v1`. Init already creates the parent as
0700 system:system on post-fs-data, before class hal, with the existing
`vendor_bengal_hud_data_file` label. No new rc, policy, AIDL or app owner is needed.

Construction performs no DSP writes and never adopts program defaults. A
successful typed setter records that control's intent and atomically saves the
bounded file. Failed hardware writes never replace intent. Shared output gain,
Android media volume, FM level/source matching, routing and source mute stay
with their existing owners and are absent from this file.

The guard's normal startup and `recover_after_acc_fault` activation path runs:

1. Invalidate applied flags; perform existing FAULT_SAFE and DSP_ON/validation.
2. Replay saved semantic controls while DAC/amplifier are still off.
3. Perform existing source PREPARE/COMMIT, DAC enable and amplifier enable.

Only successful replay writes increment the fresh process's generation.
Unrecorded controls remain unknown. Recovery retains the process's monotonic
successful-write count, invalidates the old epoch and applies the saved intent
in the new epoch. A failed replay or later activation failure invokes the
existing FAULT_SAFE path and clears every applied claim; outputs do not silently
come up at reset gains. The saved file remains available for repair/retry.

Loudness stores only enabled/disabled intent. Enabled mode waits for a fresh
`notifyVolumeIndex`; the model explicitly reports pending volume policy and
`loudnessApplied=false`. No old media index is replayed. Disabled mode replays
the existing typed program-default operation. Ordinary volume notifications
do not repeatedly rewrite unchanged settings.

The DSP transaction and guard locks cover hardware success, publication and
saving so a concurrent recovery cannot replay the previous intent between a
setter's ioctl and its publication. The restore hook never acquires those locks
itself; it runs under the existing guard lock. It has no second device opener.

## File and failure contract

Maximum input: 4096 bytes, regular UTF-8 file, no symlinks. Missing file means
no saved intent. Malformed, oversized, unsupported-version or corrupt input
causes no partial replay: activation fails safely and exposes the error through
guard status and DSP snapshot `lastError`. Repair requires correcting/removing
the file and restarting the daemon through the lead's deployment procedure.
The daemon never overwrites a rejected file with defaults.

Saves use a 0600 create-new sibling temporary, complete write, file fsync,
same-directory rename and directory fsync. A failed save is returned to the
setter and retained in snapshot `lastError`, while the confirmed hardware model
and generation remain truthful. A directory-fsync failure can mean the new file
is already visible but not durably committed. The next successful setter retries
the full pending intent, including an otherwise unchanged setting.

The file begins with `BENGAL_DSP_SETTINGS 1\n`. Each row is three decimal
integers `operation channel value\n`, sorted by operation then channel. This is
a private semantic schema, **not** a kernel ioctl or register encoding:

| Operation | Meaning | Channel | Value |
| --- | --- | --- | --- |
| 1 | Output mute | 1..5 (FL, FR, RL, RR, sub) | 0 or 1 |
| 2 | Output phase inversion | 1..5 | 0 or 1 |
| 3 | Output delay | 1..4, never sub | 0..145 request steps |
| 4 | Output gain | 1..5 | -200..200 request steps |
| 5 | EQ | band 0..3 | -20..20 request steps |
| 6 | Bass shelf | 0 | -32..32 request level |
| 7 | Treble shelf | 0 | -32..32 request level |
| 8 | Loudness intent | 0 | 0 or 1; no saved volume index |

Only explicitly applied/selected controls get rows, at most 26. Duplicate rows,
unknown operations/channels and values outside these bounds are rejected. The
last line is `END hhhhhhhhhhhhhhhh\n`: exactly 16 lowercase hexadecimal digits
of 64-bit FNV-1a over every preceding byte including newlines. Initial value
`0xcbf29ce484222325`; for each byte, XOR then multiply by `0x100000001b3` modulo
2^64. This detects damage, not malicious authenticated changes.

For first deployment, use a fresh applied semantic capture if one exists;
do not seed earlier snapshots, unknown default fields or an unselected preset.
The simplest adoption is the user's explicit saved-profile Apply or existing
typed Binder setters after deployment: the backend saves each confirmed change.
Both apps subsequently read the same backend truth. No automatic migration of
HuDsp's explicit-Apply profile is performed. Current generation-zero snapshots
cannot recover tuning lost before this implementation.

## Focused verification

From the HUD directory with `BENGAL_DESK_TMP`, `TMPDIR` and `CARGO_TARGET_DIR`
pointing into the assigned exclusive scratch:

```sh
cargo test --offline -j2 --lib dsp:: -- --test-threads=2
```

The production restart regression first failed with “restart lost saved output
gain”. It now verifies typed replay before route/DAC/amp, exact retained mute,
unknown controls, generation and EQ. Additional cases cover every supported
setting, recovery, fresh loudness policy, each replay-write failure plus lost
amplifier reply, corrupt files, schema bounds, failed-save retry and old-inode
atomicity. The Binder compile-only desk check verifies actual Android adapter
wiring against generated AIDL stubs; it is not an Android build/deployment proof.

Desk cannot answer: listening quality, electrical/acoustic transients, actual
device filesystem durability, installed SELinux behavior, or physical ACC
acceptance. Shared output calibration still restores on Radio's lifecycle;
this work makes no new promise about that owner's early-boot timing.
