# Bengal native GNSS service — opt-in standalone provider

This is a standalone AIDL7 provider backed by `libbengal_loc`, not the AOSP
mock service or an OEM HAL. `BENGAL_GNSS_LOC=1` selects the product package,
feature XML, init service, AIDL7 VINTF declaration and narrow GNSS policy. It
requires the existing ACC power channel. Knob-off omits those new payloads.
The real modem Start/native-report/Stop path was measured by diagnostic4;
the initial Binder component built successfully. Full enabled images and the
framework lifecycle were exercised in the D3-0146/0147 rollout. The GPS antenna
is now attached; the user reports stock success with the same antenna/cable
and a later in-car failure lasting over10minutes. Current indoor runs do not
establish satellite reception or explain that car failure.

## Prior art reused

The census and pinned LOC sources are recorded in `../loc-probe/README.md` and
`docs/research/2026-09-06-claude-gnss.md`. Parent mock GNSS/HIDL mismatches are
not copied. All four reference trees were searched for the original LOC client
work; no earlier working open GNSS HAL was found. The receiver protocol is the
already exercised native LOC path; no NMEA Set, global configuration restore,
firmware replacement or GKI change is added here.

Local Android17 sources used as the interface authority:

- `hardware/interfaces/gnss/aidl/aidl_api/android.hardware.gnss/7/` (frozen).
- FCM202604 accepts GNSS AIDL2–7.
- `frameworks/base/services/core/jni/gnss/Utils.cpp:164`: when elapsed-time
  flags are absent, Android uses reception time; we do not claim modem/AP sync.
- NDK `binder_ibinder.h:672`: dynamically allocated death cookies are released
  only by onUnlinked, including failed links and concurrent death/unlink.
- `AndroidStandbyPeripherals.java:106,183`: existing ACC preparation disables
  location and restores the saved preference; that should invoke close/Stop.
  This connection is source-level, not yet a measured GNSS ACC cycle.

## Supported and explicitly unsupported

Standalone periodic GPS fixes with receiver-requested intervals (minimum1s).
Interval changes stop/restart this client's session rather than silently ignore
the request. GNSS scheduling is the only advertised capability. Success status,
satellite technology, both finite/ranged coordinates and explicit UTC are
required. No mock coordinates, fake satellite list, fake NMEA or fake energy
statistics are generated.

Required extension objects are non-null, but unimplemented measurement,
assistance, aiding deletion/location injection, power/debug queries and blocklist
operations report unsupported. Optional batching/geofence/PSDS/corrections
interfaces report EX_UNSUPPORTED_OPERATION with a null output. Success plus
null is forbidden: Android17's GnssHal wraps successful getters without checking
their Binder, and D3-0146 crashed system_server in GnssPsdsAidl::setCallback.
`python3 extension-contract-test.py` checks all five unavailable getters; it
failed on all five before this fix. Actual Android boot remains the runtime
compatibility check. NMEA output, single-shot and low-power mode are not
implemented. This is NOT full GNSS VTS/CTS compliance.

Uncertainty mapping uses the pinned LOC circular63% convention and an analytic
2D Gaussian scale to68%. Ellipse-only reports use a conservative enclosing
major-axis radius at their supplied confidence. Vertical uncertainty is only
reported when its explicit confidence is at least68%. Speed/bearing uncertainty
and modem-synchronized elapsed time are not advertised without a known contract.
Physical accuracy/fix acceptance still requires antenna and sky view.

## Ownership and stop behavior

- One transport worker, one bounded callback dispatcher per service. The
  dispatcher may be blocked by a remote Binder call without blocking native
  cleanup; it owns shared state, never a dangling service pointer.
- Cancellation uses C++ atomics through the C observer hook, not a volatile
  cross-thread data race. Native polling checks cancellation at most100ms apart;
  command/cleanup exchanges have2s absolute deadlines.
- Disable/close, callback failure and framework death cancel the worker. Only
  fixes after Start acknowledgement and before cancellation reach the queue.
  Stop clears queued fixes; one already in-flight remote callback can complete.
- No library locks are held across remote callbacks. Metadata survives a quick
  setCallback→start, and the latest fix replaces an undelivered older one.
- Start waits up to13s for setup/ack (normal measured protocol is much faster).
  No background retry is attempted on modem loss or rejected configuration.
- Provider mode requires the modern client mask already AFW-enabled/NFW-locked
  and mode already standalone; it never writes lock/NMEA/mode. Only this client's
  Register/Start/Stop/Unregister are used.
- `/data/vendor/gnss/bengal-owner.lock` must live in a gps-owned private directory.
  The service locks it until cleanup finishes. The lead must not run old manual
  LOC clients concurrently: those predate this service's lock.
- `vendor.bengal.gnss.state` records idle/starting/running/stopping/failed.
  Only post-fs-data boot initialization establishes idle after an unclean
  service exit. Service restarts retain failure; BengalPower requires idle
  before declaring the installed receiver quiet. This does not measure RF power.
- Unconfirmed cleanup is sticky: no subsequent Start
  is allowed to conceal it. Stop/close report an error; it never emits an invented
  physical ENGINE_OFF. SESSION_END describes only the client stream.

Build the component with the canonical Lineage24 runner after commit/mirror.
The executable requires `--serve`; no arguments refuse before any receiver or
Binder service action. Record actual Android build, packaged permissions and
manifest, then the focused framework Start/Stop lifecycle after deployment.

## Satellite status

`startSvStatus`/`stopSvStatus` enable/disable real native SV callbacks during
an active positioning session. A separate one-report slot replaces undelivered
SV data, so slow Binder cannot grow the queue. Stop/close/session teardown clear
queued SVs; one already in-flight remote call may finish after stop. No new
native session or configuration write is triggered solely by startSvStatus.

The strict adapter uses the frozen AIDL7 types and their source contract.
Identity, antenna CN0 and angles require valid native fields. Galileo301–336,
BDS201–263 and NavIC401–414 map to Android local IDs; GLONASS stays local1–25.
Deprecated COMPASS, out-of-range IDs and incomplete entries are omitted.
Ephemeris/almanac flags require their native validity bit. TRACK never becomes
USED_IN_FIX. Signal/carrier frequency, elapsed clock and used-in-fix remain
unavailable; these are not inferred from constellation or tracking.

Android basebandCN0 is mandatory. LOC expanded reports with an aligned valid
RF-loss array provide it as antenna CN0 minus RF loss, only when both are
finite and the result is nonnegative. A searched/not-tracked SV with an
explicit native zero CN0 may use Android's documented zero baseband value.
Tracked SVs without known baseband CN0 are not given fabricated zeroes.
An explicit native empty list sends an empty callback. A nonempty list where
all entries lack Android mandatory fields sends no callback and increments
the bounded reception summary's suppressed count; it never becomes a false zero-SV result.
The native diagnostic always retains raw count/search/track/CN0 summaries.
This strict boundary may suppress legacy modem reports; inspect a device
capture before claiming reception visibility through Android.

`loc-probe-desk-test.sh` runs the actual adapter/stream helper tests, including
all mappings, unsupported/incomplete fields, true-empty versus unavailable,
latest-only bounds and stop/restart stale-data rejection. The native wire
tests include RF-loss counts, scalar validity and range checks. Android target:
`android.hardware.gnss-service.bengal` (lead builds and tests Binder/runtime).

### Coordinate-free reception summaries

`BengalGnss: reception summary` records scalar metadata from the existing
validated native event observer. The first line accompanies Start acknowledgement;
subsequent SV/engine events can emit at most one line per30 monotonic seconds.
There is one final summary when the worker ends, including failed setup.
No timer, extra receiver client, RF query, raw packet or position logging is added.
There is no periodic heartbeat when no relevant events arrive.

- `start-ack=1` means only that this client's Start was acknowledged.
  `engine-raw=unknown` means no observed modem engine event; it does not mean off.
  Numeric `engine-raw` retains the validated u32 without assigning an enum meaning.
  `engine-events` counts observations; `engine-changes` counts changes after the
  first observation, not transitions inferred from Start or cleanup.
- `sv-indications` counts valid event0x25 reports. `list-absent` counts reports
  without a list, `list-empty` counts explicit zero-entry lists and `list-nonempty`
  counts real nonempty lists. `latest-list-count=unknown` preserves a missing
  list rather than turning it into zero. No satellite identifiers are logged.
- `android-converted-reports/entries` count the strict adapter's results,
  including real empty reports. `android-suppressed-reports` counts nonempty
  reports with no representable entries. `android-omitted-entries` includes
  both complete suppression and individual omissions from partial conversions.
  These are conversion counts, not Binder delivery acknowledgements or distinct
  satellites. All counts are cumulative within this worker session.

Both native metadata hooks are optional and share the post-Start,
pre-cancellation/cleanup delivery boundary. Pre-ack engine events and teardown
events do not enter the summary; there is no invented ENGINE_OFF. Cancellation
between native metadata and Android conversion can leave the final native count
ahead of conversion counts. Malformed packets retain failure/cleanup behavior.
The host suite drives real packet decoding for absent/empty/nonempty lists,
engine raw values, malformed inputs and lifecycle suppression. The scalar helper
test covers strict-adapter counts and rapid engine changes under the rate limit.
These diagnostics explain reception visibility; they do not repair or establish
RF reception. Android compilation and live logcat checks remain required.

## UTC time aiding

`injectTime` accepts a real framework UTC Unix-millisecond sample, its Android
elapsedRealtime reference and nonnegative millisecond uncertainty. The latest
sample belongs to the callback registration: replacement, death, close and
callback failure discard it; ordinary stop/start preserves an unconsumed sample.
The Binder method only queues, and its success does not claim modem acceptance.

The serialized LOC owner revalidates at send and adds the CLOCK_BOOTTIME delta
(including suspend), requiring age strictly below24h as in Android's
NtpNetworkTimeHelper. Future references, invalid values and overflow are rejected.
No wall clock fallback, GPS epoch conversion or leap-second offset is used.
Uncertainty remains the supplied NTP uncertainty, matching the LOC reference.

Accepted Start and LOC event0x28 request trusted time through the existing Binder
dispatcher. Requests coalesce to one queued plus at most one in-flight callback;
UTC storage is one latest sample. Stop never waits for a Binder callback. AIDL
carries no callback-registration token in injectTime: the registration check
protects queued samples; it cannot identify a late call from an old client.
Scheduling remains the only advertised capability; ON_DEMAND_TIME would enable
framework periodic injection and is not advertised here.

Provider-only event mask0x93 adds time-request bit0x10 to0x83. LOC injectUTC wire
ID0x38 (generated table index46) sends TLV01/u64 UTC and TLV02/u32 uncertainty;
optional time source is omitted. Both QMI response and LOC status indication
must succeed before claiming time acceptance. Explicit modem refusal logs and
continues standalone. An isolated2s UTC completion timeout also continues the
active position/SV stream, but permanently disables UTC injection and time
requests on that client. No retry can consume an ambiguous old completion.
Late same-peer0x38 responses must match the timed-out transaction; indications
use their independent sequence. Their known fields are still decoded strictly,
then discarded, including during Stop/unregister. Malformed packets, service
loss, transport failure and cancellation retain normal cleanup behavior. Logcat
separates queued, accepted completion, refused/expired and timeout-disabled
states without logging the UTC value or coordinates. This is cold-start aiding, not evidence that a fix
exists or that missing time caused the in-car failure.

`loc-probe-desk-test.sh` runs production C/socket UTC replay and the C++ sample
helper tests. Android Binder compilation/runtime, modem acceptance, TTFF and
actual outdoor fix behavior remain lead-owned validation.


## GPS Check request visibility

GPS Check0.3 displays engine started/ended separately from satellite reception.
An engine-ended notification leaves the current request pending only until its
existing1/10minute limit, since framework interval changes may stop/restart the
engine. It does not imply no signal. Stop, Home and the deadline unregister
both listeners and clear the screen-awake flag. Each check owns distinct
callback/listener/timer objects, invalidated before removal; queued old events
cannot mutate or stop a new check. Failed status registration aborts the check.

The host `platform/packages/apps/BengalGpsCheck/tests/test-lifecycle.py OUTPUT_DIR` compiles and runs the
actual Activity with small Android fakes. It covers engine end/restart, stale
and reentrant events, deadlines/Home, and request/registration failure cleanup.
A separate javac check against the current public Android SDK verifies API
compatibility. These checks do not substitute for Android Binder/UI testing.
