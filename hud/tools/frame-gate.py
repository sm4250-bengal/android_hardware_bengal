#!/usr/bin/env python3
"""Refuse an artefact that carries an MCU frame the allowlist does not name.

Ported from kernel/initramfs/build-initramfs.sh, which does the same scan on
PID 1 (D15 §10.5: "Build-time assertion that the artefact carries no
frame-shaped constant but the two allowed ones -- port it to the daemon's
build").

WHY IT EXISTS. The MCU is the one peripheral on this unit where being wrong
costs hardware: 05 16 authorises sleep and the IAP commands 43/44/46/4A rewrite
the MCU's own firmware. src/mcu/mod.rs gates transmission at runtime, but a
runtime gate only covers the code paths someone thought about. This covers the
BYTES: if a frame-shaped constant is in .rodata, some path can reach it.

The daemon can emit a fixed version query, two ACC lifecycle frames and the
fixed stock GPS-ready notification. When
Phase 4d adds the two radio frames, they go here and in src/mcu/mod.rs ALLOW in
the same commit -- src/mcu/mod.rs has a test that pins the exact allowlist,
precisely to force that pairing.

    tools/frame-gate.py <file>...          exit 0 = clean, 1 = refused

Run it on whatever is about to ship: the cargo artefact at the desk, the Soong
one at packaging time.
"""
import re
import sys

# ── SCOPE, decided 2026-08-31 (morning item 7) ───────────────────────────────
# THIS BYTE SCAN IS FOR **C ARTEFACTS**. It is meaningful on the PID-1 initramfs
# (kernel/initramfs/diagnostic-init.c), which stores whole frames as constants --
#   static const unsigned char ack_frame[] = {0x5a,0xa5,0x03,0x05,0x17,0xe4};
# at :3303, :3305, :4539, :4657, :5122 -- so `5a a5` really is adjacent in
# .rodata and the scan has material to judge.
#
# IT IS **NOT** A BACKSTOP FOR THE RUST DAEMON, and pretending otherwise is worse
# than not running it. In hardware/xyauto/hud and .../fm the preamble is two
# independent byte constants (`pub const H0: u8 = 0x5a;` / `H1: u8 = 0xa5;`,
# src/mcu/mod.rs:100,102) and the trailing byte is computed by checksum() at
# runtime (:124). Frames are assembled in a buffer, so the bytes never become
# adjacent in the image: MEASURED on the shipped /vendor/bin/hw/bengal-hud, ZERO
# `5a a5` occurrences including zero allowlisted ones. The gate is structurally
# incapable of failing there, whatever the daemon can transmit.
#
# WHAT PROTECTS THE RUST SIDE INSTEAD, and it is accepted as sufficient:
#   1. the runtime ALLOW gate in src/mcu/mod.rs, which is what actually decides
#      whether a frame may be transmitted, and
#   2. the test in that module pinning the exact allowlist, so widening it
#      cannot pass review unnoticed.
# The byte scan was always a backstop for "a path someone did not think about";
# for a Rust artefact that backstop does not exist, and this file now says so out
# loud rather than returning a green that means nothing. See the INCONCLUSIVE
# verdict in main().
#
# ── THE ALLOWLISTS ───────────────────────────────────────────────────────────
# kernel/initramfs/build-initramfs.sh:1466 allows  5aa50223dd + 5aa5030517e4
#                                    (conditionally 5aa5030515e6)
# this file allows query + both ACC frames + fixed GPS-ready notification
#
# The daemon now owns the Capture-B reader. The MCU is silent until queried, so
# the query moved into its narrow gated waist together with the reader thread;
# it is no longer a PID-1-only capability. ACC-off completion remains conditional
# in PID 1 but is part of the daemon's full lifecycle state machine.
#
# Frames this artefact is allowed to contain, as hex. Add a line here ONLY
# together with the matching entry in src/mcu/mod.rs ALLOW and a reason.
ALLOWED_HEX = [
    "5aa502649c",       # fixed stock boot/wake GPS-ready notification
    "5aa50223dd",       # fixed stock version query; wakes the silent peer
    "5aa5030515e6",     # fixed stock ACC-off completion
    "5aa5030517e4",     # fixed stock ACC-on answer
    # "5aa5030503f8",   # Phase 4d: radio source on
    # "5aa5030504f7",   # Phase 4d: radio source off
]
ALLOWED = {bytes.fromhex(h) for h in ALLOWED_HEX}
PREAMBLE = b"\x5a\xa5"


def scan(path):
    data = open(path, "rb").read()
    bad = []
    for m in re.finditer(re.escape(PREAMBLE), data):
        start = m.start()
        # A frame is 5a a5 <len> ...; allow a match on any allowlisted length.
        if any(data[start:start + len(a)] == a for a in ALLOWED):
            continue
        bad.append(start)
    return bad


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    failed = False
    for path in argv[1:]:
        try:
            bad = scan(path)
        except OSError as e:
            print("frame-gate: cannot read %s: %s" % (path, e), file=sys.stderr)
            failed = True
            continue
        if bad:
            failed = True
            print(
                "frame-gate: REFUSED %s -- %d byte sequence(s) look like an MCU "
                "frame header and are not on the allowlist, at offsets %s"
                % (path, len(bad), ", ".join(hex(b) for b in bad[:8])),
                file=sys.stderr,
            )
            print(
                "frame-gate: a transmittable frame the allowlist does not name is "
                "the one thing this must never ship.",
                file=sys.stderr,
            )
        else:
            # ⟲ A SCAN THAT FOUND NOTHING AT ALL IS NOT THE SAME RESULT AS A
            # SCAN THAT FOUND ONLY ALLOWLISTED FRAMES, and printing one sentence
            # for both is how this gate reported a pass on an artefact it could
            # never have failed. Measured 2026-08-31 on /vendor/bin/hw/bengal-hud:
            # ZERO `5a a5` occurrences, including zero of the two allowlisted
            # frames -- the Rust daemon builds frames at runtime from H0/H1 and a
            # computed checksum (src/mcu/mod.rs:100,102,124), so the bytes never
            # become adjacent in .rodata and no scan of that image can ever fire.
            # Say so, instead of saying "clean".
            data = open(path, "rb").read()
            seen = sum(data.count(a) for a in ALLOWED)
            if seen:
                print("frame-gate: %s carries %d allowlisted frame(s) and no "
                      "unlisted one" % (path, seen))
            else:
                print("frame-gate: INCONCLUSIVE %s -- no MCU frame bytes of any "
                      "kind, allowlisted or not." % path)
                print("            This gate cannot fail on such an artefact, so "
                      "its pass is not evidence.")
                print("            Expected for Rust artefacts, which assemble "
                      "frames at runtime; see the SCOPE note above.")
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main(sys.argv))
