# Explicit d84 vendor DMA discriminator (source-only)

## Prior-art census and scope

Reuses the four-tree census and exact transport analysis in
`docs/research/2026-09-07-bt-next-step.md` and
`kernel/out/.tmp/bt-dma-next-kernel/REPORT.md` (the old reference paths now
route through `handoff/MOVED-2026-09-07.md`). The D3-0122 retry-close evidence
and D3-0132 first-good/reopen-stalled trace precede this change. D3-0133 boot2
adds a complete 603-event positive-control/reopen capture with zero drops or
misses: first reopen pulse drains, a separate one-byte write interleaves,
then queued data stops draining despite repeated start entries. None proves
physical DMA completion or the reason for the intervening byte. Device1's
MCU/UART/IBS work is prior art, not a D3 kernel workaround.

This is userspace recorder instrumentation only. No UART, GKI, DT, module,
timeout, flow control, firmware, init, product or SELinux changes. Existing
`capture --seconds N` and its seven event definitions are retained; the new
mode is **not selected automatically**.

## Activation and identity

After separate root review/build/deployment, the only intended init activation
delta is owner command `capture --seconds 120` →
`capture-dma-d84 --seconds 120`. Keep the existing independent owner, bounded
waiter, socket, late-fs order and unconditional modprobe unchanged. This branch
does not make that change. Install the matching new binary for both owner and
waiter; the old waiter does not recognize the new mask.

New status: `mode=capture-dma-d84 schema=d84-vendor-v3 mask=16383` when armed. Waiter's
normal ARMED line carries mask16383 and an additional receipt line explicitly
names mode/schema. DMA receipt/status also names `trace_export_cap_bytes=1048576`;
legacy export remains exactly 262144 bytes. Only the exact old mask127 or new
mask16383 is admitted (v2 mask4095 is no longer complete);
partial masks or masks containing extra bits are rejected. Packet version1 and
shape, nonce, lease, deadline and no-FD contract are unchanged.

Before opening tracefs, explicit DMA mode requires uname release exactly
`6.18.32-4k-gd84cfa4479d9`, machine `aarch64`, and page size4096. This is a
release/architecture admission check, **not Image SHA identity**. Root must
independently bind the packaged/installed Image to SHA256
`4da78c722c1665fdb3e3e6a93e4e3737affcf1542737a2d0a6783798fdb4783e`.
An identically named different build is not supported by this contract.

The default `--vendor-profile original` module is additionally SHA256
`f9fff6cc074575fb3f2b653faeca20c551289d8952e4fd9813fa13ed6195949b`
(47768 bytes). Its retained original is
`.worktrees/acc-resume-fixes-lead/kernel/vendor-modules/bengal-bt-uart/bengal_bt_uart.ko`,
SHA256 `41f0a214babb9109569ba21b6f5ba7320bc006f625bc6779f8250358ce9222a0`.
`llvm-strip --strip-debug`, then `llvm-objcopy --remove-section=.BTF
--remove-section=.BTF.base`, reproduces the deployed module byte-for-byte.
Its DWARF independently proves uart_port state208/8B, mapbase328/8B,
line316/4B and the container offsets below. GKI BTF alone does not prove
the vendor layout. The packaged module has no BTF.

Explicit `--vendor-profile autocts` instead binds the vendor-only CTS-mode
repair from source f6fcf9edfb2557adfd43f3e431108411dad244ad:
stripped48168B SHA256
`ead8f328b0b4e72b3b91cf40e2daa77e6299705a2deb927e98bd77bfef1f1542`,
with DWARF producer SHA256
`a9c3059a25e59242c9d7e72d37d8e6f45fa23fd18e52b4a84459c543537c19de`.
The producer lives in `.worktrees/bt-autocts-lead/kernel/vendor-modules/
bengal-bt-uart/bengal_bt_uart.ko`; the stripped artifact is retained under
`kernel/out/.tmp/reliability-0143-lead/autocts-candidate.ko`.
Both exact artifact pairs must independently pass all compiled layout/symbol
checks below. Mixed pairs and selecting the wrong profile are rejected;
the default does not silently accept the new module. This adds no recorder
event or runtime behavior and does not prove installation or a Bluetooth fix.

Actual matching output is `kernel/out/.tmp/gki-202609-build-lead/gki/`;
`kernel/out/gki/` and `kernel/out/gki-r584948c/` still identify92378 and are
rejected by the source witness. Compiled d84 BTF independently confirms
qcom_geni_serial_port.uport0/544B, se544/96B, tx_dma_addr664/8B,
tx_remaining720/4B. New fixed offsets read ordinary RAM only, never MMIO.

## Events and interpretation

### MCU queue-poll exclusion (D3-0143 follow-up)

The first complete deployed DMA capture contained 1762 unrelated MCU-reader
`uart_chars_in_buffer` returns (362972 bytes), versus 12 BT queue predicates.
All rows, including their distinct UART states, were examined in order. This
consumed most of the 512-KiB stopped export in only 23 seconds of normal MCU
polling; raising the cap or dropping every zero count would obscure the defect
or discard Bluetooth's drain evidence.

Explicit DMA mode now writes and exactly reads back the instance-local FIFO
event filter `comm != "xyauto-mcu-read"` before enabling that event or tracing.
That is the kernel's 15-character truncation of HUD's `xyauto-mcu-reader` name
(`hud/src/binder.rs`). Exact d84 `trace_events.c:199-200` provides the generic
comm field; `trace_events_filter.c:992` evaluates it against current->comm.
The v3 gate events below have their own mapbase filters. Legacy capture,
its event definitions/export limit, per-CPU buffers, absolute deadlines,
and hardware behavior remain unchanged.

DMA status includes `fifo_excluded_comm=xyauto-mcu-read` and separately
`fifo_filter_applied=1` only after successful write/readback. Filter failure
refuses ARMED and follows existing OFF/disable/cleanup. Account for this
explicit task exclusion when interpreting absence; still match UART pointers
and inspect all drops/misses. A renamed MCU thread could reintroduce noise;
this is not an automatic all-other-UART filter.

Host owner/socket tests cover pre-enable ordering, write/read/mismatch/truncated
failures, cleanup and unchanged legacy operations. The recorded trace replay
retains all six zero-valued BT queue returns. Actual kernel filter acceptance,
post-boot output size and ACC transport behavior require separate runtime
evidence; host fake readback does not prove kernel filtering.

### Serial-core TX gates (v3)

The filtered D3-0143 ACC capture accepted one byte at2070.108757 but never
entered matched vendor start-DMA before FIFO1 close waited30.052s. Previous
flushes and unmaps completed normally. This supersedes guessing a stuck DMA
mapping; it does not prove CTS, IBS, firmware or the stop flag responsible.

Two additional events observe generic serial-core `__uart_start`:

- `_gate_in`: state/uport/mapbase, flags, tty pointer, tty.flow.stopped,
  **uart_port.hw_stopped** (not tty.hw_stopped), serial port-device pointer,
  tx_enabled and runtime status. Built-in BTF resolves the fields; tx_enabled
  deliberately omits an explicit type so the parser extracts its single bit.
- `_gate_pm`: exact compiled `__uart_start+76` (0x4c), immediately after the call
  to `__pm_runtime_resume`; x20 is uport, x19 port_dev and w0 the signed PM
  result. Records runtime status/error, serial/physical disable depths and
  tx_enabled. Numeric ordinary-RAM offsets and bit positions are verified
  against the exact Image's BTF and disassembly by `dma-d84-source.py`.
  Decimal spelling is mandatory for exact readback: trace_kprobe.c1304
  prints `%u`. The initial hex spelling registered but correctly refused
  ARMED on readback mismatch and retained one disabled event for lead cleanup.

Both events require checked instance-local `mapbase == 0x4a8c000` filters
before enabling. `gate_filters_applied=2` attests both readbacks. Any failed
filter refuses ARMED and cleans owned resources. The finite stopped DMA
export cap is1MiB to hold these added fields; legacy stays256KiB. Per-CPU
buffers,120s maximum awake capture and1s cleanup/export deadline do not change.

On matched UART, stop flags explain an entry without `_gate_pm`; otherwise
the signed PM result distinguishes an error from deferred activation. A
post-PM point is a branch observation, not an instruction that wakes/reset
anything. Entry and post-PM snapshots need not be simultaneous; distinguish
null/faulted tty fetch from a valid false flag using the tty pointer. Require
covered time, all CPU drop counters and kretprobe misses before using absence.
Runtime registration and a positive real BT capture still require deployment.

The new mode targets `_dma`, `_empty`, `_flush_in`, and `_flush_out` in
`bengal_bt_uart:` explicitly. All four use numeric ordinary-RAM fetches;
the module-qualified name with the old BTF `uport->...` expressions still
fails on this BTF-stripped module. Unqualified start/empty names are ambiguous
against the built-in driver and fail registration (NON_UNIQ_SYMBOL/EINVAL).
The actual vendor flush target is `qcom_geni_serial_flush_buffer_dma`.
The mode extends `_dma` with `dma_addr` and `remaining`, adds `_prep_in`/`_prep_out`
at geni_se_tx_dma_prep, and `_unprep` at geni_se_tx_dma_unprep. Entry/exit names
are not prefixes of each other: the existing conservative removal readback
must not confuse another live event with an undeleted one.

Match UART mapbase0x4a8c000 / line2 and its uport; helper `se` must equal that
uport+544. Other UARTs/serial engines can emit these helpers: **do not infer
their container layout or count their activity as Bluetooth**. Prep includes
length and iova output-pointer identity; its return event reads the saved
entry pointer's current RAM value and signed return status. Only ret0 means
successful preparation; a failed return's address is not a valid DMA mapping.
The unchanged local kprobetrace contract supports saved `$argN` on return.

Unprep records entry x30 as a symbol string (caller+offset). Actual d84 entry
is BTI before PACIASP; the caller LR is not yet signed there. The compiled
ISR calls unprep then clears address/remaining; stop also calls unprep.
There is no standalone compiled qcom_geni_serial_handle_tx_dma symbol and no
probe on that nonexistent symbol. Unprep entry is not proof unmapping returned;
ISR-side provenance is not alone proof of electrical delivery or IRQ health.

The previous `d84-v1` built-in console flush was not the SE3 callback and
could not answer the flush question. D3-0139's vendor driver already carries
the upstream DMA-flush repair and wires `qcom_geni_serial_flush_buffer_dma`
into its non-console ops. This schema corrects observation only; it does not
change that repair, driver, timeouts or Bluetooth state.

The exact d84 `trace_kprobe.c:663-665,693-714,810-832` admits qualified probes
for an unloaded module and attaches them via its module notifier. Raw fetches
avoid requiring module BTF during registration. The two vendor entry probes
use arm64 `%x0`, because unresolved pre-load entries lack FENTRY and reject
`$arg1` (`trace_probe.c:1026-1045`). The two returns retain saved entry `$arg1`:
live return x0 is not the original pointer. Host negatives cover both cases.
Pre-load ARMED means enabled
definitions, possibly pending attachment, NOT proven callbacks or traffic.
Inspect subsequent module-registration errors and require matched positive
hits before interpreting absence. No live attach acceptance is claimed here.

| Observed on matched UART in a complete interval | Meaning / next distinction |
| --- | --- |
| Matched vendor `_flush_in`/`_flush_out` pair | Observe DMA address/remaining across the actual SE3 flush callback; no electrical delivery claim |
| Prep returns error | Mapping/preparation failure is observable; do not call it DMA completion loss |
| Initial lifetime delivers RX, prep/unprep and draining queues | Same-capture positive control for the new observations |
| Truncated trace, missed return probes, dropped/overrun events or unmatched identity | No absence conclusion; preserve as partial evidence |

## Verification

`tests/dma-d84-source.py --repo <Bengal-worktree> --gki <exact-d84-output>
--kernel-source <pinned-d84-source> --vendor-module <stripped-f9fff6cc.ko>
--vendor-unstripped <original-41f0a214.ko> --scratch <owned-existing-scratch>` checks Image bytes, source pin, compiled
symbols/config, actual BTF offsets/sizes, entry LR contract, and byte-identical
legacy definitions. Original owner source rejects with `missing DMA discriminator`;
the old output rejects with `not the d84 compiled output`. Vendor DWARF must
reproduce the stripped artifact exactly; seven vendor field offsets/sizes and
the three compiled local functions are separately checked.

`BENGAL_DESK_TMP=<assigned-existing-scratch> bash tests/dma-d84.sh` builds host
executables only and runs real socket/process/timer tests through the actual
owner and its existing semantic OS adapter. Includes target refusal before
resource access, all14 registration/disable/removal, partial append preserved,
failed OFF/no export, late ON/readback/no ARMED, foreign-state preservation,
DMA exports above 256 KiB, exact masks, and the legacy27-group suite.
The production definition vectors run through a narrow semantic registration
adapter: the real legacy definition rejects as ambiguous, a prefix-only
mutation rejects for absent module BTF, and all current definitions pass.
The actual owner/socket path also arms and cleans up with that adapter active.
This models the pinned parser constraints, not execution of the kernel parser.
It uses no host or device tracefs. The existing setup-errors suite separately
covers diagnostic/geometry/POSIX errors. No new target runtime registration,
Android link or SELinux acceptance is claimed by these host tests.

`BENGAL_DESK_TMP=<assigned-existing-scratch> bash tests/export-cap.sh` runs the
actual owner/socket/timer and POSIX read/save loops on ordinary files. It
substitutes root resolution and filesystem/UID metadata; it does not establish
tracefs or Android permission acceptance. DMA inputs through1048576 bytes are
preserved complete, input of1048676 truncates explicitly at1048576, and legacy
retains its exact 262144-byte cap. Unrelated setup/status/global-event/profile
read caps stay unchanged. Invalid/unowned caps fail before filesystem access;
OFF-before-export and the shared cleanup deadline remain bounded.

## Desk cannot answer / For memory

Desk cannot answer actual new-field registration/readback, live overhead,
whether the expanded trace remains below the DMA 1 MiB export cap and completes
within the retained cleanup second, the live DMA-address/prep/unprep/close
transitions, physical delivery, or a working Bluetooth fix.
All original deadline, single-owner, exact-definition, drop/miss, cleanup and
SIGKILL limitations remain. Failed/late arm still releases modprobe; failed OFF
does not permit export or blind deletion. Do not hold ready factory app fixes
for this optional diagnostic or label Bluetooth fixed.

For memory: explicit DMA mode is unactivated source-only; retained capture is
unchanged. New evidence must use exact d84 Image/BTF identity, matched SE3 and
complete non-dropped/non-missed capture. The source witness rejects misleading
old `kernel/out/gki` contents despite the current source checkout being d84.
