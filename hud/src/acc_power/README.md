# BACPv1 native channel and preparation owners

Default off at compile time: `BENGAL_ACC_POWER_CHANNEL=1` sets the Soong
`bengal_hud.acc_power_channel` boolean, selecting Rust feature
`acc-power-channel` and the frozen V1 dependency together. Off excludes the
module, reader hooks/arguments and checkpoints. The original foundation's desk
expansion matched pre-channel898; this preparation change has ON/OFF compiler
coverage, not a new Android ELF byte-parity claim. The Binder
desk helper likewise requires explicit `BENGAL_ACC_POWER_CHANNEL=1` for its
channel-enabled mode; default0 does not read the optional interface.

When compiled, the second default-off guard is
`ro.vendor.bengal.acc_power_channel`. Enabling registers the
standalone stable `vendor.xyauto.accpower.IAccPowerChannel/default`, using the
frozen V1 Rust dependency. The app offers one connected unnamed SEQPACKET
endpoint through oneway `offerChannel(ParcelFileDescriptor)`. There is no
pathname listener, extra broker, new MCU command, or physical sleep call here.
Failed registration, absent feature, or unavailable entropy leaves legacy UART
startup and the 500 ms/once-only final MCU write behavior unchanged.

The actual Binder caller UID must equal the socket creator's SO_PEERCRED UID.
Only user0 app UIDs 10000–19999 and the exact SO_PEERSEC BengalPower domain plus
its derived four MLS categories pass. Missing context denies. The app must
authenticate the protected canonical Binder service and frozen API/hash; both
socketpair endpoints have the **app's** creator credentials, not HUD's.
The current bring-up product is SELinux permissive: parsing identities and
registering this name are **not trusted authentication proof on that baseline**.
Enforcing/nonpermissive owners and real participant/deep-capability evidence
remain prerequisites for any physical sleep authority. Initial Android
integration must remain observation-only and send no READY when unavailable.

The UART owns its random session nonce, event sequence, snapshot fence and
readiness flag. Each replacement AccSession gets fresh entropy; registration
gets a separate worker-generated nonce. Signed generation/sequence/deadline
overflow invalidates rather than wrapping. No nonce claims wire freshness or
proves hardware delivery. A new process/session has to re-handshake.

Immediately before each serial step the UART snapshots actual Machine state,
drains at most eight controls and samples BOOTTIME outside the hooks. The
CycleEvent hook performs fixed local work and try-enqueue only: no allocation,
clock, syscall, eventfd or blocking lock. A 32-record event ring holds even a
stalled send head and post-snapshot facts awaiting START. Full rings or lock
contention poison the connection out of band. The single worker owns all socket
I/O; the UART signals it only after serial returns. The per-record
step-in-progress check, FIFO FinalResult-before-Authorize ordering and queue
mutex synchronization hold a producing step's AUTHORIZE until its serial result
is known. This is **not** a commit fence for every record: a worker can read an
old false flag, then acquire a newly queued, truthful FINAL_RESULT before that
serial step returns. Complete alone grants no authority. The next iteration
sees that step's in-progress flag (or its completion); failure poisons rather
than releasing its queued AUTHORIZE. A deterministic check/front interleaving
test covers both later failure and successful release.

Only READY recorded by that **pre-step UART checkpoint** can qualify that step's
Complete final host-write result for AUTHORIZE. READY during/after the write
cannot upgrade it. Host Complete is not drain-to-pins, MCU receipt or suspend.
Matching CANCEL revokes the cycle immediately through an atomic generation,
independent of queue space; exact repeats are idempotent. Poison closes the
connection, unlike ordinary cycle cancellation. Historical OFF snapshots and
their later final result remain observation only.

The enabled native path now additionally starts one preparation worker on live
GRACE. UART hooks only publish fixed atomic state; notification is after serial
returns. The worker uses the existing sole AudioGuard FD and FM service mutex,
never a second UART/GPIO/I2C owner. ABI5 feature bit1 and the fixed 32-byte
`_IOWR(0xbc,0x0f)` request must confirm a keyed transient all-output hold before
FM is parked. The physical owner/generation pair is retained across adjacent
logical OFF cycles, and readiness is separately keyed to the current UART
session/cycle. Missing feature, failed/query-fault/late completion denies
authorization; FINAL_RESULT remains only the truthful host-write result.

FM mutations are fenced by the real service while held. An actual user stop or
audio-owner death revokes replay; stale release cannot touch a newer lease.
Audio Binder cleanup retires that replay intent and defers kernel route/rail
changes while the physical mute is held. Release drains deferred cleanup, never
claims an early rail-off. Failed restoration latches unavailable and does not
retry or silently unmute. Normal DSP model/settings are not replaced by a
temporary mute. The new FM field/methods are compile-gated by the same default-off
`bengal_hud.acc_power_channel` selection; no optional feature/dependency off.

HUD's actual removable-file sink set is empty: logs are kmsg/liblog, and its CAN
capture is fixed under `/data/vendor`. This certifies no external collector or
vold mount. The Android owner must stop its own external writers and unmount
public USB volumes; PREPARE timing and wire sequence remain unchanged.

Only an authenticated READY **accepted by a UART checkpoint** requests deep
arming from this worker. GRACE never selects deep. `participants::DeepOwner`
is the root-owned platform seam: `arm(token, absolute_boottime_deadline)` and
`disarm(token)`. Its current constructor is deliberately `UnavailableDeep`:
no physical provider/selector is invented by this change. Native hold AND arm
completion must be sampled before the final-producing step, remain valid at
publication and pass the sender's check. READY at that final checkpoint is too
late to start new arm work; the MCU final frame is neither delayed nor retried.
Failed/late arm is rolled back. Cancellation, session loss and shutdown disarm
before audio release; failed disarm retains the audio hold and forbids later
readiness. These are worker ordering guarantees, not bounded kernel syscall or
cross-process wake-lock-release acknowledgments.

One worker serializes physical changes. A new OFF arriving during old release
cannot become ready until that release and its own new hold finish. An already
completed newer hold is never overwritten by an old release. Thread wake epochs
retain cancellation arriving inside blocking owner code (which can consume a
thread park token). Driver stalls do not block MCU hooks, but may prevent timely
preparation or restoration; no source-only test proves physical liveness.

One persistent worker, one pending/active owned FD, no owner eviction. All
recvmsg records must be exactly 64 bytes with no ancillary data. Every delivered
SCM_RIGHTS and SCM_PIDFD descriptor closes even for malformed/oversized records.
SCM_PIDFD contains one int, possibly a negative allocation error, not an array.
Other ancillary payloads are never interpreted as descriptors. Kernel-truncated
rights are kernel-closed; the selected kernel does not install a PIDFD if its
complete cmsg cannot fit. Send/receive are nonblocking. Handshake and stalled
send have absolute 2 s limits; PREPARE authority expires at pre-step BOOTTIME
+2 s, rechecked freshly before sending. Pending work polls at most 50 ms;
idle retained waiting uses no heartbeat timer.

Focused host checks use `cargo test --offline --manifest-path
platform/hardware/xyauto/hud/Cargo.toml acc_power::` with an isolated
`CARGO_TARGET_DIR`; current MCU regressions use `mcu::`. Real socketpair tests
exercise transport and SO_PEERCRED; positive app UID/context admission is
explicitly injected and is **not** an Android SELinux integration proof.
The actual Binder ReaderHooks desk control holds the lossy diagnostic mailbox
lock and verifies that independent Grace/Prepare/FinalResult facts survive.
Android generated-interface compilation, enforcing policy integration,
retained current, MCU watchdog behavior and same-session wake need separate
lead-owned evidence. Nothing here activates or deploys automatic sleep.
