# Bengal Power HAL: bounded CPU-floor INTERACTION

Default OFF: `BENGAL_REAL_POWER=0` selects the exact existing example provider.
`=1` replaces it with `android.hardware.power-service.bengal`, AIDL V6, and
dedicated policy. Thermal is independently selected with `BENGAL_REAL_THERMAL`.
No proprietary performance library, new kernel interface, app profile, MCU,
display, idle-state, governor or maximum-frequency change is involved.

## Prior art and measured inputs

The required four-tree census (`~/android/{docs,handoff,hardware,kernel}`) found
the Motorola libperfmgr provider in `handoff/vendor-rebuild/notes/understand-refs.md`
and the no-op Power/thermal replacement debt in `design-blob-minimal.md` §9.
Those are other-device design history, not device-3 measurements.

Current local Lineage `hardware/lineage/interfaces/power-libperfmgr/Android.bp`
is open but pulls in Pixel session/task/statistics/control machinery and a
device hint configuration. Reusing it would expand this bounded increment to
unvalidated ADPF tuning. The small local backend instead implements only the
actual INTERACTION operation; every session/headroom/composition/mode response
is honestly unsupported, including the V6 support bitset.

Lead raw inputs `kernel/out/.tmp/performance-review-lead/{cpu-io-readonly,
power-thermal-sysfs-readonly,power-cpufreq-readonly}.txt` and report
`docs/research/2026-09-07-performance-and-lineage24-candidates.md` settle D3 facts.
HUD already holds 500us CPU latency while display-on. There are only WFI1us
and deep666/621us states, so another latency hold would add no useful choice.
This is a CPU frequency floor request, not a latency request or frequency lock.

D3-0132 exposed a missing kernel semantic in the original fake: the saved client
attempt submitted **zero** hints (`INTERACTION_UNSUPPORTED`), while earlier and
later snapshots already showed864000/300000 with no journal. The exact earlier
transaction was not logged. A delayed-effective fake now reproduces that state
against the old backend: `admitted=0 supported=0 floor0=864000 floor4=300000 journal=0`.
No client retry or hardware operation was needed to reproduce the code defect.

The exact candidate source is
`platform/upstream/gki-202609-candidate/drivers/cpufreq/cpufreq.c` at d84cfa4479d9:
`:710-726` reads effective `policy->min`, `:760-775` stores the QoS request,
`:1225-1231` merely queues work, and `:1213-1222,:2659-2687` later publishes the
effective limits. Sysfs show/store and the worker share the policy rwsem
(`:1002-1033`). Therefore a successful store does not imply immediate readback,
but an accepted original store supersedes an older queued target request.

## Ownership, expiry, recovery

Only policies0/4 `scaling_min_freq` are writable. Original floors are recorded
at service startup after recovery. Targets are actual available frequencies:
the highest OPP <= half the current maximum, but never below the original.
The measured normal caps/frequencies select 864000/902400kHz versus 300000kHz
original floors; no unavailable OPP or maximum value is guessed.

INTERACTION zero duration means250ms; positive duration is capped at1000ms;
negative duration cancels. Repeated hints may extend the expiry. A dedicated
steady-clock thread restores without another Binder call. Expiry starts at
admission, before any store or convergence wait: polling cannot extend the lease.
Both targets are submitted before polling their effective values every2ms under
one shared100ms deadline, shortened to the requested expiry for short hints.
Accepted store, observed convergence and timeout/error are distinct states.
Reduced caps, foreign
floors, failed journal/write/readback disable support for this service lifetime.
Each actual request is validated against current caps. The kernel retains
thermal frequency caps and DVFS ownership.

The enabled HAL requires exclusive userspace min-floor ownership. It does not
coordinate arbitrary concurrent external min-frequency writers. Before any
write, a private atomic tmpfs journal in `/dev/bengal-power/votes` records both
original/target pairs. A flock prevents duplicate service/cleanup ownership.
The unchanged V1 journal conservatively covers both possibly submitted policies.
Cleanup explicitly submits each original request when the effective value equals
either original or target: seeing original alone is not proof that an old target
is no longer queued. Then it polls both originals under one shared100ms budget.
Only accepted compensating stores plus observed convergence allow journal removal.
An unresolved read/store/convergence/clear failure retains the journal, disables
hints and schedules the next attempt at least100ms after the failed attempt.
Binder notifications and cancellation requests cannot accelerate those retries.

A visible foreign value (neither original nor target) is never overwritten or
silently adopted. Its journal remains pending and new hints stay blocked until
external ownership reconciliation permits cleanup. Matching values cannot prove
ownership; other QoS contributors and identical competing userspace requests are
indistinguishable through this shared sysfs interface. The exclusive-userspace-
writer requirement remains, and conditional read/write is not compare-and-swap.
Startup and init `onrestart --restore` recover unfinished requests. The journal
survives process death but not reboot, avoiding stale cross-boot replay.

Startup recovery has its own pending state, separate from an active new boost.
Transient journal I/O, original-floor read/write/readback or journal-clear
failures retain the journal and trigger autonomous100ms retries in the same
daemon. New hints (including cancellation) fail while recovery is pending; normal
floor initialization and support are enabled only after successful recovery.
Malformed/unsafe journals and lock contention are distinct terminal refusals:
the daemon remains alive and unsupported, preserves the journal and does not
busy-loop or compete with another owner. A normal initialization failure after
successful cleanup still fails closed; it cannot strand an old journaled boost.

SIGKILL does NOT restore sysfs automatically. Until restart recovery succeeds,
a killed process can leave its modest floor set. If init never restarts it,
the floor persists until another explicit cleanup or reboot. Failed restoration
keeps its journal and is retried every100ms while the owner lives, including
failures in the constructor's recovery attempt. Blocked kernel
I/O or scheduling stalls can exceed the requested deadline; this is a bounded
userspace request, not a hard realtime kernel lease. No permanent-max lock is
implemented or permitted by this policy.

The already unjournaled D3-0132 floor is a separate reconciliation task. Restarting
this correction cannot recover an original value that the old backend discarded;
without a journal it still takes current floors as its baseline, never guesses a
reset. No device reconciliation is performed by the host tests or this source fix.

## Diagnostics

`BENGAL_POWER stage=... policy=... node=... requested=... observed=... errno=...
elapsed_ms=... result=...` distinguishes startup load/init, store acceptance,
effective convergence, timeout/foreign/cap/read/store failure, support disabling
and journal clear/retention. Repeated identical stage/policy/result keys are limited
to once per second per process; value/errno fields reflect that emitted observation.
No log is emitted on each2ms poll. Actual errno is saved before close/logging, and
malformed text is EBADMSG rather than a stale syscall errno.

On Android the sink is Bionic's public `syslog()` which routes to logcat through
`async_safe_write_log` (`lineage-src/bionic/libc/bionic/syslog.cpp:55-102`). The
host seam uses stderr. No RC, service, policy or Android.bp change is needed;
actual Android linking/log visibility remains to be checked by the lead.

## Desk and integration

Run `bash platform/hardware/xyauto/power/tests/desk.sh ABSOLUTE_OWN_SCRATCH`.
Run `python3 platform/hardware/xyauto/power/tests/integration.py ABSOLUTE_LINEAGE_SRC`.
The latter reads existing frozen V6/V3 generated AIDL headers; no graph/build
is launched. Its glibc-only assertion compatibility header changes no AIDL
method or Binder interface. Tests include real file adapter/locking plus fake
write/read/recovery failures and real-clock autonomous expiry.
The delayed-effective suite separately covers queued publication after failed
compensation, never-converging targets/restores, read failure after submission,
cap reduction during settling, partial second-policy submission, foreign-state
journal retention, startup cancellation of not-yet-visible targets and shared
deadline/admission expiry. The native adapter checks errno and log rate limiting.

Root must still mirror these new HAL directories, propagate/pin selection
knobs through the authorized build/package workflow, build both Android binaries
and policy including neverallows, and check final installed providers/contexts.
Do not claim Android build or runtime performance from host tests.

Runtime needed: AIDL/VINTF registration, support bits, actual floor rise/expiry,
no cap/governor drift, thermal cap interaction, restart cleanup, AVC-free journal
and sysfs paths, and paired UI/frame/CPU measurements. A real boost is implemented;
whether these modest floors improve the car UI remains unmeasured.
