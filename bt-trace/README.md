# Bounded Bluetooth UART-drain diagnostic

Single native owner plus a resource-free waiter. This module does not install
itself in a product, start at boot, change a Bluetooth setting, open an HCI/UART
transport, or change the GKI. No production trace operation is part of its desk
tests. The executable is complete; Android build, policy and init integration
remain separate lead-owned work.

## Prior-art census and source authority

The four-tree `WCN3950|hci_uart|btqca|tracefs|kprobe|FIFO` census reuses:

- `~/android/docs/xyauto-mcu/port/MCU-PORT-IS-SE0.md:13,49`: SE3-versus-SE0
  family orientation, not an inherited device-3 electrical measurement.
- `~/android/handoff/probe-0254-bt-keepawake.md:34` and
  `~/android/kernel/xyauto-sm4250/templates/README-0254-bt-keepawake.txt:21`:
  another unit's modified-kernel sleep experiment, explicitly not ported.
- `~/android/handoff/2026-08-24_boot-time-and-usb-logging.md:71`: retained
  log coverage, rather than line-count absence claims. The scoped hardware
  tree search found no native early-capture implementation to reuse.
- Bengal's `kernel/scripts/trace-bt-setup.sh` and
  `docs/research/2026-09-06-d3-0122-boot1.md:562`: measured warm TX/drain
  observations and selective-instance discipline, not its shell lifetime.
- `docs/research/2026-09-06-bt-single-owner-capture.md`: selected one-owner
  lease/timer contract. The rejected two-owner branches are not used. The
  partial `0dd63166` was inspected, not treated as an accepted implementation.
- `docs/research/2026-09-07-bt-next-step.md` and the lead's corrected
  `2026-09-07-bt-uart-drain-map.md`: regular GENI UART uses DMA, not the
  console-only FIFO callback. Initial QCA setup succeeds; failing later
  reopen must be correlated with it, not conflated with first setup failure.

The fixed definitions in `src/owner.cpp` target GKI `92378ced`. All seven
symbols appear in `kernel/out/gki/System.map`. The retained configuration has
`KPROBE_EVENTS`, `PROBE_EVENTS_BTF_ARGS`, and
`HAVE_FUNCTION_ARG_ACCESS_API` enabled.

`Documentation/trace/kprobetrace.rst:80` explicitly permits `$argN` at
kretprobe return. `kernel/trace/trace_probe.c:1027-1045` uses saved entry
data for return arguments; `trace_kprobe.c:1049` installs its entry handler
and per-instance storage, and `:1505-1523` stores the entry arguments.
ARM64 `regs_get_kernel_argument` (`arch/arm64/include/asm/ptrace.h:326`)
maps our first four scalar/pointer parameters to x0..x3. No live return
register is misrepresented as an entry pointer.

BTF named field fetches use `trace_probe.c:631-726` and
`Documentation/trace/fprobetrace.rst:124-158`. No guessed structure offset
or module-local HCI callback is required. Exact definitions are registered
and fully read back before ON; missing BTF fields or attach failures abort.

| Record | Identity/correlation and meaning |
| --- | --- |
| TX return | serdev, serdev->ctrl, requested bytes, signed accepted count |
| RX entry | tty_port, port->client_data (controller), buffer pointer, arg4 count |
| tty wait | tty, tty->port, tty->driver_data (uart_state), timeout |
| software FIFO return | same tty/state, remaining count |
| GENI DMA entry | uart_port, state, mapbase, line; **callback entry only** |
| GENI empty return | same uart_port/state/mapbase, FIFO-empty predicate |
| serdev close | same serdev/controller; lifetime boundary |

`serial_core.c:654` identifies tty->driver_data as uart_state;
`serdev-ttyport.c:25` identifies tty_port->client_data as serdev_controller.
The pointer links connect all seven observations without treating simultaneous
traffic on a different UART as a Bluetooth witness. RX count is not an HCI
payload decode. No unbounded/possibly-out-of-length packet buffer is copied.
`pahole -F btf -C` against the actual `kernel/out/gki/vmlinux` confirms the
referenced serdev_device/tty_struct/tty_port/uart_port field types. The owned
instance selects and reads back `trace_clock=mono` (kernel `trace.c:1761`,
`ktime_get_mono_fast_ns`) to align trace timestamps with lease/capture times;
it never changes the global trace clock.
An independent read-only libbpf enumeration of that same ELF's BTF also
confirms all seven function names and parameter names (including RX arg4
`count`); receipt `kernel/out/.tmp/bt-drain-owner/btf-args.log`.

GENI DMA callback entry can return without submitting a transfer. Its FIFO
empty predicate is **not** DMA completion, CTS state, electrical pin activity
or proof of delivery. No-start absence needs matched UART, complete covered
interval, non-truncated output, no per-CPU drops/overruns and no kretprobe
misses. The status exports per-CPU statistics and owned `kprobe_profile`
rows; it does not automatically label an absence conclusive.

## Production interface and integration contract

```
xyauto-bt-first-trace capture --seconds 1..120
xyauto-bt-first-trace wait --budget-ms 2000
```

There are no runtime root/definition/test/command arguments or fake-selection
environment knobs in the production binary. The owner only accepts Android
init's inherited `ANDROID_SOCKET_bengal_bt_trace` descriptor, validates its
fixed `/dev/socket/bengal_bt_trace` address, SEQPACKET and listening state,
and never binds/unlinks a listening socket itself.

Later debug-only integration must supply:

- Two independent disabled/oneshot init services, running as root in
  `u:r:vendor_bengal_bttrace:s0`. Start owner, exec_start waiter immediately
  before unconditional existing late-fs modprobe. No service restarts.
- An init-owned `bengal_bt_trace seqpacket+listen 0600 root root` socket.
  Its socket creation context must be the helper **domain** above (the
  service's default socket context), while its filesystem pathname gets a
  dedicated socket-file label. `SO_PEERSEC` is the socket security context,
  not the pathname label: init `service_utils.cpp:197` passes service context
  to `CreateSocket`, which uses `setsockcreatecon`, separately from the
  pathname's `setfscreatecon` in `init/util.cpp:98-125`.
- A root-owned mode-0700 `/dev/xyauto-bt-trace` tmpfs directory, with a
  dedicated output label and narrow debug-only tracefs policy. It holds a
  mode-0600 regular `owner.lock` and unique O_EXCL trace/status files.
- Waiter `timeout_period 2` as an independent init containment measure,
  **not** a tracing OFF operation. Do not give the owner a SIGKILL timeout
  and call it a cleanup guarantee.

Both directions validate peer root UID and exact helper security context.
The host-only endpoint adapter substitutes its private socket and current
UID and does not pretend an SELinux-free host proves Android policy.

The fixed-size local native-endian versioned packet contains a random 128-bit
nonce and absolute CLOCK_MONOTONIC T/D. Every operation shares D, bounded by
owner startup+2500 ms. Capture end C=T+N seconds is fixed before ON using a
real absolute timerfd. The client cannot extend it; after ARMED no IPC or
CONTROL traffic is needed. A missed reply leaves a timed capture possibly
running and the waiter prints NOT-CONFIRMED, not a fabricated cleanup claim.

The fresh instance is initially ON in the actual kernel and semantic fake.
Its first control operation is explicit OFF/readback, even if mkdir crossed
the lease. Setup records every successful append before its readback; a
failed or partial write is ambiguous and never authorizes deleting that
name. Matching foreign/extra definitions are preserved. Only the owned
instance trace is cleared with O_TRUNC; `trace`'s write callback alone is a
no-op (`trace.c:4868` / `tracing_write_stub`). Global trace controls are never
written. All path components use held-directory-relative O_NOFOLLOW opens.

OFF and event-disable/readback precede finite trace export: legacy `capture`
keeps 256 KiB; explicit `capture-dma-d84` v3 permits 1 MiB for its owned trace
only. See `tests/DMA-D84.md` for target admission and interpretation limits.
The **request** is 256 KiB per CPU, not a 256-KiB total allocation. The
supported geometry is the current 4K ARM64 ACK kernel: the owner checks
`buffer_subbuf_size_kb=4` and requires `buffer_size_kb=258` after writing
256. Other subbuffer sizes (including a future 16K kernel), unequal sizes
(`X`), undersize/oversize, malformed, failed or truncated reads fail closed
before registering probes. It does not accept an unbounded `>=256` result.

This corrects D3-0132's measured `buffer-read/readback-mismatch`, expected
`256`, observed `258\n`, before any registration or ON. Verified against the
**installed** ACK candidate
`platform/upstream/gki-202609-candidate` at
`d84cfa4479d96e063067ca9ca1a23375483b93fd`:

- `include/linux/ring_buffer.h:85,106-110`: the ARM64 header is 16 bytes.
- `kernel/trace/ring_buffer.c:2507-2508,2639,2978`: default physical page
  4096, payload 4080, requested bytes rounded up to whole payload pages.
- `kernel/trace/ring_buffer.c:6142` and `kernel/trace/trace.c:5939,6951-6965`:
  reported capacity is payload bytes shifted right by 10, not request echo.
- `kernel/trace/trace.c:6989-6991,9660-9672`: write takes KiB; subbuffer
  readback reports physical size in KiB.

Thus `ceil(262144/4080)=65` pages, `65*4080=265200` payload bytes, reported
as 258 KiB. Data pages occupy 266240 bytes (260 KiB) per CPU, plus reader
pages and metadata. Status distinguishes `buffer_requested_per_cpu_kb=256`
from checked `buffer_per_cpu_kb=258`, with `buffer_subbuf_kb=4` and the
separately read kernel total. The existing total-read contract remains
readability-only; it is not a checked memory accounting claim.

The legacy stopped export cap remains exactly 262144 bytes; explicit d84 DMA
v3 uses 1048576 bytes and reports `trace_export_cap_bytes=1048576`. Setup, global
event/profile reads and status retain their prior bounds. Output and
statistics have separate bounded capacities; export+cleanup share one
additional second and 4096-byte I/O chunks, with no fsync/trace_pipe. OFF,
disable, export, statistics and cleanup outcomes remain separate. Ambiguous
definitions or failed OFF leave resources for explicit later inspection;
the helper does not blindly remove them. Partial output is retained uniquely.

The stored status's `status_saved=0` describes its state before the status
write; the final stdout receipt reports whether that save succeeded.

## Desk verification

From an isolated worktree, with an assigned existing scratch parent:

```
BENGAL_DESK_TMP=/home/meinte/android/bengal/kernel/out/.tmp/bt-drain-owner \
  bash platform/scripts/bt-first-trace-desk-test.sh
```

Builds the real production CLI and a separate host test executable with the
same owner/wait/protocol code. The internal fake models initially-ON
instances, global append semantics, exact readback, failed/partial writes,
collision and selective removal. Forked real processes use actual Unix
sockets and timerfd, with isolated operation journals and fake modprobe
release markers. No ordinary temporary directory is passed off as tracefs.
The runner retains binaries, SHA256/size receipts and all test evidence.

Tests cover successful ARMED/release with owner surviving waiter exit;
no/delayed client and absolute expiry; setup/ON/readback crossing D; no
CONTROL and duplicate/flood attempts; initial/terminal OFF failures;
registration/readback/append collisions; partial write ambiguity; FD cleanup,
packet shape/socket type; wrong nonce/version/clock/mask; stopped output
full/short/read failures, truncation and cleanup budget; SIGKILL while ON.

The fake also models the installed ACK's buffer round-up, not a plain-file
write/read echo. The old owner plus this fake reproduces the real
`256` versus `258\n` failure. Focused rounded-buffer tests run real
owner/waiter processes, verify requested/reported status, reject malformed
and unsupported sizes (including embedded NUL and extra lines), exercise
read failures/truncation and a subbuffer read crossing admission, and
measure the unchanged stopped-export byte cap. Run them with diagnostic
error and production CLI controls using:

```
BENGAL_DESK_TMP=/home/meinte/android/bengal/kernel/out/.tmp/bt-trace-rounded-buffer \
  bash platform/hardware/xyauto/bt-trace/tests/setup-errors.sh
```

## Explicit limits / desk cannot answer

SIGKILL, SIGTERM, process crash or an uninterruptible tracefs syscall cannot
be made cleanup-safe by this userspace owner. The killed-owner test leaves
the fake ON. A post-write clock check detects an ON effect that crossed D;
it cannot erase that history. Under scheduling delay it stops promptly on
resumption, not at a hard physical deadline. A private flock coordinates
only this helper, not noncooperating privileged global kprobe writers.

Desk cannot prove actual kernel BTF registration/hits, Android linking,
installed namespaces, DAC/SELinux access, startup scheduling margin, trace
completeness, which UART was active in a real capture, or the Bluetooth root
cause. No Android build, init/policy integration, runtime trace or numbered
probe is claimed by this module.
