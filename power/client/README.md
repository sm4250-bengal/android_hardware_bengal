# Single-hint Power HAL observation client

DESK CANDIDATE, NOT INSTALLED. No Android link or device run is implied.
`bengal-power-hint-client` is an explicitly selected, non-installed, arm64
binary. It changes neither the production HAL nor product/policy selection.
The lead must release the build window before mirroring or compiling it.

## Prior-art census and reuse

The four reference trees `~/android/{docs,handoff,hardware,kernel}` were
searched for IPower, Power HAL and INTERACTION/setBoost prior art. The reference
device's `handoff/vendor-rebuild/notes/understand-runtime.md:159` lists IPower,
but does not provide a bounded client or prove this device's behavior.
The lead's existing shell/client census found no boost operation in
PowerManagerShellCommand or `svc`; this client is not a shell command wrapper.

Reuse the typed `IPower::fromBinder` pattern from local
`lineage-src/hardware/interfaces/power/aidl/vts/VtsHalPowerTargetTest.cpp:139`
and that VTS module's static AIDL dependency pattern. Do **not** reuse VTS's
unbounded registration wait or loop over all boost types. Generated frozen
V6 `IPower.cpp:412` confirms that `setBoost` is ONEWAY. A successful status
only establishes successful submission, never HAL admission or restoration.

The parent Power README and `Boost.cpp` define the existing floor-only
implementation. Device-3 frequencies/caps come from
`kernel/out/.tmp/performance-review-lead/power-cpufreq-readonly.txt:1` and
`:16`, not reference-device tuning. The D3-0132 probe's bounded-INTERACTION
row supplies the runtime question. No existing HAL/backend code is copied.

## Exact operation

The sole accepted invocation has `--expected-boot-id UUID`; there is no
default, alternative target, mode, duration, retry or cancel option. Before
any Binder query, require the supplied current boot ID, completed Android boot,
`ro.product.device=bengal3`, `ro.boot.serialno=16cc8af`, kernel release
`6.18.32-4k-gd84cfa4479d9`, real/effective UID0 and exact `u:r:su:s0` context.
The context is an explicit expected condition, **not yet a measured runtime
claim**; the lead must verify it. A mismatch fails closed; do not widen policy.

Use `AServiceManager_checkService`, typed V6 version/support queries, and
SupportInfo with only INTERACTION, no modes/sessions. Missing service, failed
query or unsupported/mismatched provider sends no hint. Observe at least500ms
of idle samples: both mins300000kHz, caps1804800/2016000kHz, `schedutil`, and
target OPPs864000/902400kHz available. Recheck identity and baseline immediately
before the **one** `setBoost(INTERACTION,250)` call.

Sample only policy0/4 min/max/governor/available frequencies, at10ms target
cadence for at most1500ms after submission starts. Rows bound the sequential,
non-atomic reads with monotonic begin/end offsets and a printed phase origin.
Accept at most50ms per-read/inter-sample gaps; preserve all rows on failure.
Require both target floors in one sample and subsequently both original floors
in one sample. Reject foreign floors, changed caps/governors/missing target OPPs,
late/truncated observations or another visible rise after restoration. Continue
the observation window after first restoration to catch later contamination.

The client never writes sysfs, sets modes, creates sessions, launches apps,
cancels hints, retries hints or stops/restarts the HAL. Its only state-changing
operation is that one typed Binder hint. On failure it records NOT_PROVED;
it does not attempt corrective writes. Ordinary framework hints can contaminate
the window, including indistinguishable identical votes. Baseline samples do
not prove continuous idle or exclusive ownership between reads.

## Lead-owned runtime boundary and tells

The lead must run it under an **external5s watchdog that kills only this client**,
not the HAL. Service lookup is non-waiting for registration, but Binder queries,
sysfs reads and scheduling can still block. No internal clock check can impose
a hard deadline on those operations. A watchdog timeout is NOT_PROVED and must
not trigger a retry. Observe/preserve the actual floor state separately if needed.
The client requires no policy addition; denied Binder access is evidence, not
permission to change SELinux. Verify the finished binary's hashes and ELF
dependencies before any lead-authorized temporary deployment.

| Tell | Meaning |
| --- | --- |
| `SERVICE_MISSING`, `*_QUERY_FAILED`, `INTERACTION_UNSUPPORTED`, `SUPPORT_INFO_MISMATCH` | No hint submitted; inspect provider/access selection |
| `BASELINE_*` or identity refusal | No hint submitted; observation prerequisites absent |
| `SUBMISSION_FAILED` | Submission reported failure; admission remains unknown; no retry |
| `HINT SUBMITTED_ONLY ... admission=UNKNOWN` | One-way transaction submitted, not an admission acknowledgment |
| `NO_OBSERVED_BOTH_RISE` | Both target floors were not observed together; no performance claim |
| `NO_AUTONOMOUS_BOTH_RESTORE_OBSERVED` | Both rose but both original floors were not subsequently observed |
| Contamination/read/timing/identity failure | Experiment not proved; preserve raw rows |
| `OBSERVED_BOTH_RISE_AND_RESTORE` | Both transitions observed, no client cancellation; causal attribution and exact250ms timing not proved |

No result establishes improved UI speed, service-restart recovery, thermal
interaction or reliable expiry under blocked I/O. Those are distinct questions.

## Desk checks

Run `bash platform/hardware/xyauto/power/client/desk.sh ABSOLUTE_OWN_SCRATCH
ABSOLUTE_EXISTING_LINEAGE`. This builds/runs only a small host observer test
and syntax-checks the actual client against existing generated V6 headers and
Bionic property declarations. The host shim changes only assert/annotation
compatibility, not Binder or AIDL methods. It launches no Soong graph and makes
no changes to shared outputs. Host tests include a full positive trace,
identity/UID/context refusals, invalid/short/gapped baselines, partial rise or
restore, foreign caps/floors/governors/OPPs, renewed votes and truncated windows.

The static AIDL/common libraries and `c++_static` avoid relying on vendor AIDL
shared libraries when eventually executed from a temporary path. Actual Android
linking, `DT_NEEDED`/interpreter verification, namespace resolution, SELinux Binder
access and measured floor behavior are still **unverified**. The host test ELF
is x86-64 and is never a device payload.
