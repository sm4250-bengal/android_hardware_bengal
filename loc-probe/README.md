# Manual QMI-LOC desk probe

Manual diagnostic, not an Android location provider. Device-3 session4 started
and stopped a native positioning session, with engine/position-status events
and unchanged configuration. No antenna was connected, no fix was obtained,
and physical engine-power release remains unmeasured. `Android.bp` defines a manual vendor `cc_binary`; no product, init,
VINTF, XML, sepolicy, DT, modem lifecycle or firmware integration is included.
The program is deliberately not in `PRODUCT_PACKAGES`.

## Explicit receiver cold start

`--qrtr --delete-aiding-all --log NEW_FILE` performs one receiver assistance
reset and closes. It is mutually exclusive with all query/session actions and
`--rf-metrics`. The operator must hold exclusive LOC command ownership before
using it. It does not stop another provider or acquire that ownership itself.

This reproduces the authentic OEM's old LOC `0x44` DeleteAssistData request:
exactly mandatory TLV01/u8/1, without optional masks. The pinned Qualcomm
IDL2.134 header at `kernel/out/.tmp/car-ready-0145-lead/location_service_v02.h`
lines6630–6745 and C descriptor3504 define it; the exact OEM path is traced in
`docs/research/2026-09-28-oem-gps-trace.md` and the native trace it references.
Receiver assistance includes learned clock/frequency and GPS blacklist state,
as well as ephemeris/time. This is not a raw NV/EFS erase or factory-calibration
restore; its complete firmware-internal persistence is not known.

The CLI requires a successful matching QMI response and same-peer LOC status
completion, in either order. Indications use an independent sequence, not the
request transaction. Discovery and command each have a two-second deadline;
cancellation is checked at most100ms apart. It sends no Get, Set, Register,
Start, Stop, fallback or retry. No provider/startup behavior changes.

`reset=ACCEPTED` means both completions succeeded, not reception or a fix.
`reset=NOT_SENT` means no send was attempted. Any unsuccessful send attempt,
partial completion, refusal, malformed packet, loss or cancellation reports
`reset=UNKNOWN`; the detailed response/status logs retain explicit refusals.
There is no automatic replay or restoration; the final cleanup field is
`NOT_REQUIRED`, never a claim that receiver state was restored. A failed log
sink also fails the operation. Ordinary runs without this flag never reset
aiding. The host suite's independent wire oracle exercises both completion
orders, correlation, malformed/refused/partial results, deadlines and signals.

## Prior-art census and scope

The four-tree search covered `/home/meinte/android/{docs,handoff,hardware,kernel}`
for GNSS/GPS/QMI-LOC/NMEA references. The starting point remains
`docs/research/2026-09-06-claude-gnss.md` in Bengal, especially sections 1 and 6:
the D3-0044 QRTR census found service16, but its historical port30 is not an
address contract. No earlier open LOC client or measured fix was found.
The stock hardware capture and firmware references establish modem GNSS as
the transport target, not acceptance of this message set.

Reused parent evidence: `handoff/vendor-rebuild/notes/design-blob-minimal.md:123`
rejects the mock GNSS provider; `handoff/REMAINING-WORK.md:167` records the
HIDL/AIDL mismatch; `handoff/CURRENT-STATE-2026-07-30.md:227` records a missing
antenna on a different unit, not a device-3 antenna fact. UART guesses in
`docs/xyauto-mcu/INDEX.md:44` are superseded by the CAN mapping, not reused.
No parent hardware or kernel entry supplies an open-client success control.
The Wi-Fi and power memory notes distinguish modem registration from GNSS
function; the old “nobody starts the modem” line only describes D3-0097/98.

This is an independent implementation of the small wire contract, using the
unchanged BSD-3-Clause libqrtr codec and transport API. There is no libqmi,
GLib, ModemManager code linkage or copied HAL implementation.

## Frozen primary protocol evidence

Local sources, read without modification:

| Source | SHA-256 |
| --- | --- |
| `kernel/out/.tmp/claude-gnss/qmi-service-loc-1.38.0.json` (52627 bytes) | `a2c53062255e36ec5d5459d123f2c72e0358f1a015294333efcef184b0c642b8` |
| `/usr/include/libqmi-glib/qmi-enums-loc.h` | `c38eef0198f31203ca0a6954e7936c0cec74576621fa93d66ef7745ac33fecb0` |
| `/usr/include/libqmi-glib/qmi-flags64-loc.h` | `525bbc6b025ae6b19bf01a3eaceaebb413ccb6c11c1723722ec0792f94461c0d` |
| `lineage-src/external/linux-msm/qrtr/include/libqrtr.h` | `32c6fd7e26eaa70086d4af6fa3011ec227e130659125f813ffc51ac833deca1f` |
| `lineage-src/external/linux-msm/qrtr/lib/qmi.c` | `dc5e608ed83348a1536b93877596a8bf4a32498e2b740d18f0f7b3a4951cf04b` |
| `lineage-src/external/linux-msm/qrtr/lib/qrtr.c` | `e2aaa6829bb109c22f2b97c6eca8a1cbf6ff4d1c65d5283c83aa58bc89d91391` |

`pacman -Qo` identifies both installed enum headers as libqmi1.38.0-1. The
saved schema is the primary libqmi1.38.0 LOC message description, originally
from `https://raw.githubusercontent.com/linux-mobile-broadband/libqmi/1.38.0/data/qmi-service-loc.json`.
The modern lock extension was subsequently checked against the Linux Foundation
LOC IDL v2.134 (generated 2020-10-14), in the pinned
[FP5 header](https://gitlab.com/CalyxOS/device_fairphone_FP5/-/raw/56fbc811991084e331c8992a1b6b16d95922ba5d/location/loc_api/loc_api_v02/location_service_v02.h)
and matching `location_service_v02.c` message descriptors. See lock structs
around header lines5680–5869 and descriptors3293–3340. This is protocol evidence,
not copied OEM HAL code.

| Request ID | Request TLVs | Completion contract |
| --- | --- | --- |
| Get lock `003b`, Get NMEA `003f`, Get mode `004b` | none | response result + same-ID indication status1/u32 and value10/u32 |
| Set lock `003a`, Set NMEA `003e`, Set mode `004a` | 01/u32 | response result + same-ID indication status01/u32 |
| Register events `0021` | 01/u64 = `0x85` (bits0,2,7) | response result only; zero mask unregisters this fresh client |
| Start `0022` | 01/u8 session1; 10/u32 recurrence1; 13/u32 interval1000ms | response result only; not evidence of engine/fix |
| Stop `0023` | 01/u8 session1 | response result only |

Modern Get lock may omit legacy value10 and return client-mask12/u64 instead:
AFW bit1, NFW bit2, privileged bit4. The mask is authoritative when both fields
are present; unknown bits and wrong widths refuse mutation. Set lock still
requires legacy type01/u32, with optional client-mask11/u64 overriding it.
The client preserves the privileged bit, permits AFW, blocks NFW and restores
the exact saved mask (including representation) on cleanup. No lock Set is
sent when already correct. Default subscription is DD(2); a returned different
subscription is refused rather than saved/restored against the wrong target.

Schema lines23–39 define indication status and recurrence; 77–131 define
registration/Start/Stop; 787–898 and1006–1054 define Set/Get pairs. Both
**Get and Set** require asynchronous completion; an accepted response alone
is insufficient. Installed enum header lines119–142,488,503–506 fix periodic1,
standalone4, NMEA_ALL`0xffff`, and MT lock3. MT locks mobile-terminated
sessions; it is not “all sessions unlocked”. Indication status success is0,
unsupported2; common QMI response unsupported is94 (`libqrtr.h:109`).

The QRTR QMI header (`lib/qmi.c:46–59`) is packed little-endian
`u8 type,u16 txn,u16 message,u16 payload_length`, followed by unpadded
`u8 tag,u16 length,value` TLVs. There is **no QMUX client byte**: the dedicated
local socket/port supplies client identity. Responses require the pending
transaction. Indications use an independent sequence, not a request transaction.
Paired completions match the selected peer and serialized pending message ID.
Device-3's
first session returned `0401003b001200010400000000001208000200000000000000`:
txn1, success, client-mask2. That captured reply is a regression fixture.
Session2 disproved the initial echo interpretation: response txn9 had indication
sequence6; response txn10 had indication sequence7, with the exact restored
NMEA value. Indication sequence numbers are diagnostic metadata; legacy zero
is accepted too. Only response transactions are correlated. Unknown event and
message IDs remain ignored; session IDs still filter positions. Schema data
alone does not establish runtime transaction behavior.

The modern RegEvents descriptor uses optional11/u32 clientType: AFW1, NFW2,
privileged3. Its header explicitly defaults an absent type to NFW. With NFW
locked, the old registration was inconsistent with its intended AFW session;
session2 received Start result1/error46 (GeneralError). After a modern mask
query, register and unregister explicitly as AFW1. No privileged client or
privacy bypass is requested. Legacy lock-enum peers retain the old encoding.

Device3 session3 with explicit AFW accepted Start and produced live engine and
position-status reports (no fix, antenna absent). Stop was acknowledged but
NMEA restore readback was `0xffc0` instead of saved `0x20000`; a fresh query
returned the original `0x20000`. The same-client NMEA lifecycle is therefore
NOT accepted. Modern-mask peers now leave the NMEA configuration untouched
and register only native position/engine events (`0x81`). This is sufficient
for the native location path, without depending on that unresolved NMEA side
effect. Legacy host-tested behavior still uses `0x85` and NMEA configuration.

`NMEA`0026 uses raw string TLV01 (schema317–325), without an inner length
prefix or terminator. The probe bounds it to256 bytes, requires printable
`$...*HH` and a correct XOR checksum, and allows trailing CR/LF. `Position
Report`0024 requires status01/u32 and session02/u8; optional latitude10 and
longitude11 must each have8 bytes. Position and engine002b/01/i32 are logged
as raw data, never interpreted as a valid location or RF success.

The pinned codec has permissive decoding and trusts incoming scalar lengths.
The wrapper checks the complete7-byte header, exact datagram length, all TLV
boundaries, required scalar widths, and duplicate tags before calling it.
Oversized/truncated recvmsg datagrams fail. Unknown optional TLVs remain raw
diagnostic evidence, not capabilities. Target endian is explicitly restricted
to the codec's existing little-endian implementation.

## Lifecycle and safety contract

1. Create one dedicated AF_QIPCRTR client; request service16 with a wildcard
   version/instance lookup, validate local name-service source, then record
   the discovered instance/node/port and client port. Never connect to port30
   by convention. An unrelated packet does not extend the deadline.
2. Query/save lock, NMEA mask, and operation mode **before any mutation**.
   `--query-only` ends after these three Gets with `outcome=QUERY`, without any
   Set/Register/Start/Stop. It is mutually exclusive with `--seconds`.
   Unsupported, malformed, missing, or response-only Get fails before Set,
   Register or Start. Invalid original lock/mode enums also refuse mutation.
3. Set only differing settings, register this client's events, Start session1,
   and collect for the requested duration. Mark every possible mutation at the
   transport send attempt, so a lost response still schedules restoration or
   Stop. A TX-log failure before sending creates no fictitious mutation.
4. On completion/error/SIGINT/SIGTERM/SIGHUP: attempt Stop if Start might have
   been accepted; unregister events; restore changed mode/NMEA/lock in reverse
   order and Get each value back. Continue all remaining cleanup after failure.
   Finally close the socket. A disappeared service is not silently rediscovered
   and restarted. No automatic retry session is allowed.

Two-second absolute deadlines apply to discovery and every command (including
response plus indication, not two separate timers). `--seconds` must be1–180.
Worst-case protocol wait budget is214 seconds: discovery2, eight setup
commands16, collection180, eight cleanup commands16. Signals stop normal
work; cleanup remains bounded and is not skipped by the cancellation flag.
SIGKILL, process crash, firmware disappearance, Stop refusal, or I/O failure
cannot guarantee engine release. `CLEANUP_FAIL` / `cleanup=UNCONFIRMED` and a
nonzero exit explicitly expose that possibility; socket closure is not proof
of modem power release. Readback prevents an old same-ID Set completion from
being the sole restoration witness, but indication sequence numbers cannot
be cryptographically correlated to requests. This is not a concurrent-client
HAL: a later manual run must have exclusive configuration ownership so that
restoration does not overwrite another GNSS client's intentional changes.

Log health is flushed/checked before every normal command send and after raw
TX/RX, response/status, and event logging. A sticky write/flush failure aborts
normal work, emits a non-location-bearing stderr failure tell, and forces a
nonzero final result. Bounded Stop/unregister/restoration bypass this logging
gate and still attempt all applicable operations even with a failed sink.

## Desk reproduction and future manual invocation

From this worktree, with assigned existing scratch:

```sh
export BENGAL_DESK_TMP=/home/meinte/android/bengal/kernel/out/.tmp/gnss-loc-desk-probe
bash platform/scripts/loc-probe-desk-test.sh
```

The runner compiles tiny host objects from the unchanged local libqrtr sources
plus the client. The production transport is compiled but never opened. The
test executable injects AF_UNIX SOCK_SEQPACKET transport; its six-u32 envelope
is **test-only**, not a QRTR/QMI wire format. The independent Python peer checks
request bytes and uses node7/port4107/instance9, not historical device values.
Thirty-seven scenarios cover success, indication-before-response, unrelated sources,
wrong client/transactions, unsupported Get, response-only Get/Set timeouts,
locked refusal before Start, NMEA/QMI/header corruption, duplicate/short TLVs,
service loss, Start timeout, signal cleanup, Stop/unregister/restore failures
and false-positive restore acknowledgments caught by readback, plus injected
log failures before mutation, before first Set/Start send, and after possible
Start acceptance. Five stub-linked CLI tests prove private file mode under
umask0, existing/symlink target refusal, and descriptor closure on fdopen failure.
The added cases replay the captured modern lock completion, preserve/restore
the privileged mask bit, prefer mask over legacy value, reject unknown high
bits/short mask/wrong subscription, detect a changed restoration format, accept
independent indication sequences and verify AFW registration and query-only
non-mutation. Modern peers explicitly reject any NMEA Set and emit native
position events. CLI tests also check query/session mutual exclusion and log mode.
The native fix decoder adds75 direct scalar/bounds cases. Two more AF_UNIX
scenarios drive its real callback path: no delivery before Start
acknowledgement, while stopping, after cancellation, for duplicate/backwards
UTC, for another session or for in-progress reports. Removing the lifecycle
guard is a compiled negative control and causes an extra Stop-time callback.
All CLI executions use a QRTR-open stub; the real transport is only compiled.
Full logs and
host executable hashes remain in the printed scratch subdirectory.

The session4 client cross-compiled against Android17/Bionic/libqrtr (build
9af246da07594c46a849adbb76e3e5e0) and ran as a ten-second diagnostic on device3.
Binary52496B SHAfd5534ccb0e54e93c4ed2ff16fd4a5b377d99d1320cb9e0f4d2f556574e37ba0
at /data/local/tmp/bengal-loc-0145-4/loc-probe reported12events/cleanupCONFIRMED.
No setting Set was sent; a fresh query returned original2/131072/4. Evidence:
kernel/out/.tmp/car-ready-0145-lead/gnss-session4.txt (56lines) and
gnss-query-after4.txt (19lines). The newer structured decoder/callback changes
are desk-tested only until separately built; session4 does not prove them.
No host binary is deployable to the unit. After lead
review/build, modem-running confirmation, exclusive-client check, antenna
question, and approved manual test setup, the intended command is:

```sh
adb shell /data/local/tmp/bengal3-loc-probe --qrtr --seconds 180 --log /data/local/tmp/loc-probe-NEW.txt
```

This is documentation, not executed here. The log must not already exist.
It is created exclusively with mode0600 and close-on-exec, independent of a
permissive umask; existing files and symlink targets are refused. It can contain
precise position/NMEA data; retain it as device evidence, not
public telemetry. No options means usage/error before opening any socket.

| Later observation | Meaning | Next |
| --- | --- | --- |
| response-only or unsupported query | no usable saved-state contract | stop, no mutation/HAL work |
| Start acknowledged and coherent NMEA/position events | protocol path works, not necessarily a fix | inspect SV/fix fields and antenna/sky conditions |
| `cleanup=UNCONFIRMED` | session/settings may remain changed | lead verifies engine/configuration before any repeat |
| actual fix plus verified Stop | receiver path works | assess physical accuracy and power; HAL software preparation can precede this |

## Native fix boundary

`position.c` independently decodes the LOC IDL2.134 position fields, using the
pinned header1389–1614 and message descriptor2017onwards plus libqmi's schema.
It requires success0, the current session, finite/ranged latitude+longitude,
satellite technology bit and explicit GPS-era UTC that fits signed64. No
coordinate/UTC/technology is synthesized. `(0,0)` is a valid coordinate, not a
no-fix sentinel. Malformed known widths fail the session; incomplete, failed or
non-satellite reports do not deliver a fix. Bad optional numeric values lose
their own validity bit. MSL altitude is not passed off as WGS84 altitude.

The `loc_fix` structure preserves native uncertainty and confidence separately.
The future Android adapter must convert them to its68%-confidence contract;
copying a wire radius or ellipse directly into Android accuracy is incorrect.
This core does not fabricate elapsed-realtime synchronization, SV status,
measurements or raw NMEA. `loc_run_observed` is still finite1–180seconds,
single-threaded, and retains the diagnostic cleanup contract. Its callback
must be prompt/non-reentrant; it owns no Binder thread or long-lived HAL yet.
Coordinates can exist in the private raw packet log, but the new summary tell
prints only status and whether a native fix is available.

`loc_follow` adds an unbounded owner-controlled session with thread-safe
observer cancellation, interval selection and no automatic retries. Polls
check cancellation every100ms; commands and cleanup still have absolute2s
deadlines. It accepts only the modern already-ready AFW/NFW/standalone state
and never writes global configuration. Return2 distinguishes unconfirmed
cleanup from ordinary failure1 and clean cancellation0. Six more AF_UNIX
cases (45total) check normal/idle cancellation, Stop refusal, modem loss,
wrong initial configuration and legacy refusal. A C++ linkage smoke checks
the C prototypes and atomic observer boundary. These are source/host checks,
not a measured long-lived receiver or Android provider result.

Desk cannot answer: device protocol acceptance/transaction conventions, antenna
or sky view, no-SIM TTFF with read-only EFS, valid coordinates, actual Stop/
restore effect, regulatory/RF behavior, standby current, or Android GNSS VTS.

## Satellite reporting diagnostic

The current client additionally registers SV event bit1 (modern mask0x83,
legacy0x87). Historical diagnostic4 used0x81 and could not report satellites.
`satellite.c` decodes indication0x25 independently against the pinned LOC
IDL2.134 header1930–2140 and descriptors134/2299. Required altitude-assumed
TLV01 is a boolean; optional list10 has count:u8 and28-byte entries (max80),
expanded list11 has count:u8 and29-byte entries (max176). Lists are mutually
exclusive. Missing list is logged `list=absent`, never zero satellites. An
explicit zero count is a real empty report. The receive bound is16384 bytes,
sufficient for the expanded list and optional signal/RF-loss arrays.

Each numeric field requires its native validity bit and finite/range checks.
Summary output is `LOC SV count=... searching=... tracked=... cn0-valid=...
max-cn0=...`; absent CN0 is `unavailable`. SEARCH/TRACK describe acquisition,
not use in a fix. No antenna-power, RF-health or fix claim follows from Start.
Provider observers receive reports only between acknowledged Start and
Stop/cancel; optional absent lists produce no callback. No extra configuration
Get/Set, aiding injection or NMEA mutation was added.

The updated AF_UNIX peer rejects the old registration mask (observed red),
replays independent full0x25 packets including expanded176 (>4096 bytes),
and checks pre-Start, Stop and cancellation delivery suppression. Direct
wire tests cover missing/empty lists, lengths/count bounds, validity bits,
nonfinite values and expanded array sizes. These are synthetic protocol
fixtures, not a device3 reception result. Build target: `bengal3-loc-probe`.

Prior art: the archived device12 MCU Toolbox
`docs/live-tests/2026-08-07-gps-zero-sv-and-mcu-ready.md` documents zero-SV
under OEM software, an unsuccessful MCU-ready experiment and measured3.3V
antenna bias on an earlier unit. Reused as diagnostic distinctions only;
none establishes device3 RF or antenna power. Current device3 in-car no-fix
report supersedes the old assumption that no car test has occurred.

Expanded SV RF-loss TLV13 and signal-list TLV12 must have aligned count:u8 and
exact lengths. Finite nonnegative RF loss no greater than native antenna CN0
provides a separate baseband-CN0 validity bit; no absent value is synthesized.
These fields are for the Android adapter; native summary CN0 remains antenna
CN0. Optional arrays without a satellite list are rejected as malformed.

## Explicit session accuracy

Start now includes TLV11/u32=3 (HIGH) for both finite manual diagnostics and
provider sessions. The pinned IDL2.134 header937–947 explicitly defaults an
omitted accuracy TLV to LOW; descriptor1954–1958 fixes its tag and width.
The pinned upstream `LocApiV02.cpp` requests HIGH for preferred accuracy up
to100m at745–750, and always HIGH in its GNSS Start path at9602–9603. This is
a per-session Start option, not a global configuration Set. Mode remains
standalone, recurrence periodic, and interval/cancellation/restoration unchanged.
The independent AF_UNIX peer requires the exact added bytes11 04 00 03 00 00
00 for manual and follow sessions and failed before the constructor change.

This is an acquisition-policy correction to test, not a measured fix. The
lead's real20s capture `kernel/out/.tmp/car-functions-lead/gnss-session1.txt`
(13372bytes, SHAd79b0ebd79df5a7968951ebb23ac3a737f9e75229e088c20b6021cbb98214cc8)
contains19 SV indications, each with only TLV01/len1/value0; no satellite
list was present on wire. The Start altitude-assumed option defaults enabled
(header988–998) and can produce additional altitude-only SV notifications.
Those notifications do not mean an explicit empty list or prove no RF signal.
For this one-variable comparison, altitude-reporting and power-mode defaults
remain unchanged. The user reports this same antenna/cable worked with the
original software; historical bench captures with no fix do not contradict
that later physical history.

## Read-only receiver identity and constellation census

`--qrtr --receiver-info --log NEW_FILE` is mutually exclusive with both
`--query-only` and `--seconds`. It discovers the LOC peer, then sends only
empty-body Get Service Revision0x32 and Get Constellation Control0xBF. It
never reads/restores settings or sends Register/Start/Stop/Set. Existing
`--query-only` retains its three original saved-state Gets. The private log
creation and signal handling are shared. At2s per discovery/paired query,
the maximum normal protocol wait is6s; unrelated traffic cannot extend it.

The pinned IDL2.134 descriptors2921/7415 and service tables9042 define this
wire contract. Revision completion requires status01/u32 and revision02/u32;
optional raw string TLVs10/11/12 are bounded to127/127/255 printable bytes.
The generic status-only libqrtr decoder does not recognize the revision's
second mandatory TLV, so this path decodes the independently length-checked
bytes directly. Constellation completion has status01/u32 and optional
TLVs10GPS,11GLONASS,12BDS,13QZSS,14Galileo,15NavIC, eachu32. Values0/1/2 are
mandatory/internal/client enabled;100/101/102/103 are unsupported/internal
disabled/client disabled/no memory. Unknown values remain `unknown raw=N`;
missing fields are `unavailable`, never disabled or zero by assumption.

Receiver facts print only after both a successful response matching the
request transaction and the same-ID successful indication from the selected
peer. Indication sequence numbers remain independent. Common-QMI unsupported
response or paired LOC unsupported indication is logged explicitly; the other
read-only query is still attempted. Final `outcome=RECEIVER_INFO_PARTIAL`
returns nonzero. Malformed/refused/incomplete queries stop with `outcome=FAIL`
and nonzero, with no fallback writes or retries. Complete supported queries
return0/`outcome=RECEIVER_INFO`. Unknown optional TLVs stay in the raw log.

Eighteen new independent AF_UNIX cases cover both completion orders, missing
optional fields, unknown enum/extension, maximum string lengths, unsupported
response/indication, short/missing/duplicate fields, long/control-character
strings and response-only/indication-only/wrong-transaction timeouts. Every
case asserts the exact request sequence contains only0x32 and optionally0xBF.
CLI tests prove opt-in mode selection and mutual exclusion before socket/log
creation. The unsupported CLI failed before this implementation. The full
suite now has65 peer cases and6 private-log/CLI tests. Build target remains
`bengal3-loc-probe`; actual firmware answers remain a lead device measurement.

### Optional provider UTC hooks

`loc_observer` optionally pairs `take_time` and `request_time`; absent hooks
preserve the existing diagnostic wire path. Only that pair adds event0x28 to
the provider mask (0x93). Hooks are nonblocking and run on the LOC owner thread.
A pending sample is consumed once per event-loop iteration and validated at
send using CLOCK_BOOTTIME, never the CLOCK_MONOTONIC command-deadline clock.
See the GNSS README for registration lifetime,24h age limit and uncertainty.

The pinned location_service_v02 schema defines injectUTC at wire0x38, with
TLV01/u64 UTC and02/u32 uncertainty, and a paired status indication. Explicit
QMI/LOC refusal is distinct from fatal protocol/transport failure: UTC refusal
does not terminate standalone tracking. Missing completion times out within
2s, reports time-result3 and disables all further UTC injection/request hooks
on that client while fix/SV delivery continues. The ambiguous transaction is
retained only to validate and discard late0x38 responses; same-peer indications
have an independent sequence and cannot match a later UTC request because no
later request is sent. Late frames/known fields remain strictly validated,
including during Stop/unregister. Other command timeouts remain fatal, as do
malformed packets and transport/service failures; cancellation remains prompt.
`test-time-peer.py` covers both completion orders, wrong source/transaction,
refusal, cancellation, service loss, transport failure, malformed completion,
invalid samples, lost response/completion/both, continuing real fix/SV callbacks,
late replies during tracking/cleanup, no re-injection and malformed late replies.
Existing no-hook tests retain their original mask0x83 expectation. No new CLI time injection mode is added.

## Opt-in receiver RF telemetry

`--qrtr --seconds 1..180 --rf-metrics --log NEW_FILE` adds a bounded diagnostic
to the native CLI only. It is incompatible with query-only and receiver-info.
The provider and ordinary CLI retain their existing registration/Start bytes.
This diagnostic requires an already-ready modern AFW-enabled/NFW-blocked,
standalone configuration, and refuses other states before Register or Start.
It sends no configuration Set, changes no power/altitude option, and fabricates
no aiding. The normal exclusive owner lock, private log and cleanup apply.

The protocol oracle is the pinned IDL2.134 pair
`/home/meinte/android/bengal/kernel/out/.tmp/car-ready-0145-lead/location_service_v02.h`
and
`/home/meinte/android/bengal/kernel/out/.tmp/car-ready-0145-lead/location_service_v02.c`.
The matching session reference is
`/home/meinte/android/bengal/kernel/out/.tmp/car-ready-0145-lead/LocApiV02.cpp:9580`.
Header602/713 adds bit42 (`0x40000000000`) to this client's RegisterEvents
mask: modern diagnostic mask `0x40000000083`. Header23524 defines indication
`0xCE`; despite its name, there is no corresponding GET request. This is a
report subscription, not an RF configuration change. A rejected registration
is logged with its raw QMI result/error and aborts; there is no fallback mask.

The RF descriptor is at C1828/7691, with fields at header21155–21292. Required
TLV01 is unsigned64 RF-update tick; required02 is signed32 band1 PGA gain in
dB. Optional10–16/18–19 each have exactly8 bytes: unsigned16 I amplitude,
unsigned16 Q amplitude, signed32 jammer power in dB. They represent GPSL1CA,
GloG1, BDSB1, GalE1, GPSL2C, GloG2, BDSB2, GPSL5 and GalE5A respectively.
Optional17 is signed32 band5 PGA gain. Known fields total at most124 payload
bytes (C9378); bounded unknown extensions may increase this to the existing
16384-byte packet cap. All fields are packed little-endian. Duplicate tags,
missing mandatory fields, invalid known widths and malformed framing fail
with normal cleanup. Unknown TLVs retain their tag/length and private raw
bytes; their contents are not interpreted. There is no LOC status field in
this indication: TLV01 is the tick, including valid zero.

Only events after Start ACK and before cleanup/cancellation contribute RF
summaries. At most256 raw RF packets and256 decoded RF summaries are logged;
later packets are still validated, with the latest tick retained. The final
summary exposes the cap, first/last tick, whether the tick ever changed, and
whether any RF report arrived. Present numeric zero is printed as zero;
absent optional fields are `unavailable`. No report means observation
unavailable, even if registration succeeded. No event cadence is promised by
the schema. Amplitudes are printed as raw integers because the header's unit
wording is ambiguous; GEN9 nominal ranges are not board fault thresholds.
Changing ticks establish updated RF telemetry, not antenna bias, correct RF
path, satellite reception or a fix.

After Start ACK, the same client sends one empty GetFixCriteria `0x33`
(header4734, C2955). Output requires both matching response and same-ID
completion. Status01/u32 and optional scalar widths are validated; optional
power18 is signed32 mode plus unsigned32 TBM. The application-ID aggregate
has bounded length-prefixed strings24/32/8 with a boolean version-valid byte.
Absent fields remain unavailable; unknown enums print raw values. A QMI or
LOC refusal produces `criteria=refused`, continues the bounded RF collection,
and returns nonzero/`RF_DIAGNOSTIC_PARTIAL`. Timeout, malformed completion,
transport loss or cancellation produces `criteria=failed` and normal cleanup.
These results remain separate from the RF observation. The query does not
alter the collection deadline. A conservative total protocol-wait bound is
198 seconds: discovery2, saved-state Gets6, Register2, Start2, criteria2,
collection180, Stop2 and unregister2. No global restoration is needed because
this mode never changes global settings.

`test-rf-peer.py` exercises the actual native client with independent packed
AF_UNIX fixtures. It checks the unchanged default mask/Start, the exact opt-in
mask and empty Get, rejection of legacy/unready configuration without Sets,
zero/absent/all optional fields, raw signed limits, unknown extensions,
malformed packets, paired completion ordering, unsupported responses, no
reports, report caps, cancellation, service loss and Stop refusal. Run the
existing desk suite, then run this focused suite against its printed
`fake-client` artifact:

```sh
TMPDIR="$BENGAL_DESK_TMP" python3 platform/hardware/xyauto/loc-probe/test-rf-peer.py "$HOST_TEST_OUT/fake-client"
```

`HOST_TEST_OUT` is the exact host artifact directory printed by
`platform/scripts/loc-probe-desk-test.sh`. CLI negative controls require
`--seconds` and reject both other query modes before opening a socket/log.
No Android build or physical RF result is implied by these tests. Build
target remains `bengal3-loc-probe`; the production provider needs no behavior
change for this diagnostic.

Prior-art correction: `newdevice-3403782-stock-2026-08-26` belongs to device2,
not device3 (`16cc8af`). Its configuration and bench measurements cannot be
attributed to device3. The archived MCU-ready/bias experiment above is also
family prior art. Device3's user-reported stock success with the same antenna
and cable, followed by more than10 minutes without a fix in the car, remains
unresolved; indoor conditions are not an explanation for that car failure.

Desk cannot answer: support/cadence of0xCE on the active modem, actual receiver
RF path or bias, correctness of board-specific calibration, or a satellite
fix. No RF/NV setter or OEM firmware substitution follows from this slice.

## Read-only acquisition configuration diagnostic

`--qrtr --acquisition-config --log NEW_FILE` is an opt-in native diagnostic,
mutually exclusive with all other modes. It sends exactly three serialized
Gets: `0xD5` minimum GPS week, `0xB7` persistent SV blacklist, and `0xD8`
parameter type 1 (minimum SV elevation). It never registers events, starts a
session, changes settings or restores settings. Discovery and each command
have a fixed 2-second deadline (8 seconds of normal protocol waits total).

LOC IDL2.134 defines D5's optional TLV10 as u16 without a narrower range;
B7's optional TLVs10..15 as full u64 masks for GLONASS, BDS, QZSS, Galileo,
SBAS and NavIC; and D8's request TLV01 / indication TLV02 as u32 type1,
with optional indication TLV10 as u8 degrees0..90. The status is mandatory
TLV01/u32 in all three indications. D8 requires its echoed type even on an
unsupported indication. Primary descriptor locations are
`location_service_v02.c:7286,7911,8209`, with field contracts in the matching
header at19430,21816,22197 in the pinned car-ready-0145 research source.
The blacklist has no GPS field; raw 64-bit masks are preserved, including
bits whose mapping is unspecified. Missing optional fields print
`unavailable`, independently from a reported zero.

Values print only after the matching response and indication have both
arrived from the selected peer. Unsupported response1/error94 or indication
status2 yields a partial result and continues to the next Get. Missing
optional fields also yield `outcome=ACQUISITION_CONFIG_PARTIAL` / exit1.
Malformed, refused, cancelled or incomplete queries stop with `outcome=FAIL`
/ exit1. Complete results return0. No retries or fallback writes occur.
These settings are observations, not a diagnosis of absent satellite reports
or an RF-health verdict. The Android provider behavior is unchanged.

`test-acquisition-peer.py` drives the actual native command decoder through
an independent socket peer, checking the exact request whitelist and bytes,
full-width masks, missing versus zero values, bounds, completion correlation,
malformed and unsupported cases, cancellation and timeouts. CLI tests verify
mode exclusion, explicit `--qrtr` and private log creation without hardware.
