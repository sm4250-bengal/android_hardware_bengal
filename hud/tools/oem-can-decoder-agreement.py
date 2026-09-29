#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Compare Rust acceptance with OEM canbox_rx_uart(), without copying it.

The Rust side is the binary that SHIPS: `bengal-can-decode` in the
`platform/hardware/xyauto/hud` crate, over `can::envelope`'s parser. It used to
be `xyauto-can-decode` in the retired `platform/hardware/xyauto/can` crate; that
crate is gone and this tool builds and runs only the new binary, in the target
directory cargo actually writes to (`CARGO_TARGET_DIR` when set).
"""

from __future__ import annotations

import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time


COMMON_GIT_DIR = Path(subprocess.check_output(
    ["git", "-C", str(Path(__file__).resolve().parent), "rev-parse",
     "--path-format=absolute", "--git-common-dir"], text=True,
).strip())
DEFAULT_OEM_SOURCE = (
    COMMON_GIT_DIR.parent
    / "archive/reference/device12/hardware/xyauto_can/evidence/CanbusMonitor.java"
)

# The binary this tool builds and runs. Kept as one constant so the build
# selection (`cargo build --bin`) and the lookup below can never drift apart --
# they did: the tool was renamed with the crate but still executed the retired
# crate's `xyauto-can-decode`, which meant it validated nothing that ships.
RUST_BINARY = "bengal-can-decode"

# Synthetic parser-boundary vectors. They are not vehicle captures.
STREAMS = (
    "2e1000ef",
    "2e41020111aa",
    "992e20020601d6",
    "2e20020601d72e1000ef",
    "2e1000ef2e7f01007f",
    "002e41020111aa55",
)

# The OEM loop does `i5 += data_count` and then the loop's `i5++`, skipping an
# immediately adjacent second header. Pin that recovered bug rather than
# weakening the Rust stream parser to reproduce it.
CONTIGUOUS_FRAME_VECTOR = 4


def extract_method(source: str, name: str) -> str:
    match = re.search(
        rf"^[ \t]*(?:public|private|protected)[^\n]*\b{name}\s*\([^\n]*\)\s*\{{",
        source,
        re.MULTILINE,
    )
    if not match:
        raise RuntimeError(f"OEM method {name} not found")
    start = match.start()
    brace = source.find("{", match.start(), match.end())
    depth = 0
    for index in range(brace, len(source)):
        if source[index] == "{":
            depth += 1
        elif source[index] == "}":
            depth -= 1
            if depth == 0:
                return source[start : index + 1]
    raise RuntimeError(f"OEM method {name} has no closing brace")


def java_harness(method: str) -> str:
    return f"""
import java.util.ArrayList;
import java.util.List;

public class OemHarness {{
    private final byte[] rx_save = new byte[1024];
    private int rx_get;
    private int rx_put;
    private byte mCanHead = 0x2e;
    private final List<byte[]> accepted = new ArrayList<>();
    private int acknowledgements;

    private void canbox_dev_read(byte[] command) {{ accepted.add(command); }}
    private void onDataSend(byte[] data, int size) {{ acknowledgements++; }}
    private static void memcpy(byte[] destination, int destinationOffset,
                               byte[] source, int sourceOffset, Integer size) {{
        if (size.intValue() <= source.length - sourceOffset) {{
            for (int i = 0; i < size.intValue(); i++) {{
                destination[destinationOffset + i] = source[sourceOffset + i];
            }}
        }}
    }}

{method}

    private void feed(byte[] bytes) {{
        if (bytes.length >= rx_save.length) throw new IllegalArgumentException("stream too long");
        System.arraycopy(bytes, 0, rx_save, 0, bytes.length);
        rx_get = 0;
        rx_put = bytes.length;
        canbox_rx_uart();
    }}

    private static byte[] decode(String hex) {{
        byte[] result = new byte[hex.length() / 2];
        for (int i = 0; i < result.length; i++) {{
            result[i] = (byte) Integer.parseInt(hex.substring(i * 2, i * 2 + 2), 16);
        }}
        return result;
    }}

    private static String encode(byte[] bytes) {{
        StringBuilder result = new StringBuilder();
        for (byte value : bytes) result.append(String.format("%02x", value & 0xff));
        return result.toString();
    }}

    public static void main(String[] args) {{
        for (int index = 0; index < args.length; index++) {{
            OemHarness harness = new OemHarness();
            harness.feed(decode(args[index]));
            StringBuilder frames = new StringBuilder();
            for (byte[] frame : harness.accepted) {{
                if (frames.length() != 0) frames.append(',');
                frames.append(encode(frame));
            }}
            System.out.printf("%d\t%s\t%d%n", index, frames, harness.acknowledgements);
        }}
    }}
}}
"""


def target_dir(crate: Path) -> Path:
    """Where cargo will put the binary for a build run with `cwd=crate`.

    `CARGO_TARGET_DIR` wins when set (the desk harnesses set a task-scoped one
    so concurrent agents do not share a target/); a relative value is resolved
    against the cargo process's working directory, which is `crate` here.
    """
    configured = os.environ.get("CARGO_TARGET_DIR")
    if not configured:
        return crate / "target"
    path = Path(configured).expanduser()
    return path if path.is_absolute() else (crate / path)


def build_rust_binary(crate: Path) -> Path:
    """Build `RUST_BINARY` and return the path cargo just wrote.

    Prints the resolved path, its sha256 and its mtime so a reader of the log
    can see WHICH executable answered, and refuses to run a binary that the
    build did not just produce.
    """
    target = target_dir(crate)
    binary = target / "debug" / RUST_BINARY
    stale_marker = binary.stat().st_mtime if binary.exists() else None
    started = time.time()
    subprocess.run(
        ["cargo", "build", "--offline", "--quiet", "--bin", RUST_BINARY],
        cwd=crate,
        check=True,
    )
    if not binary.is_file():
        raise RuntimeError(
            f"cargo build --bin {RUST_BINARY} produced no {binary}; "
            f"CARGO_TARGET_DIR={os.environ.get('CARGO_TARGET_DIR', '<unset>')}"
        )
    digest = hashlib.sha256(binary.read_bytes()).hexdigest()
    mtime = binary.stat().st_mtime
    fresh = "rebuilt" if stale_marker is None or mtime > stale_marker else "up-to-date"
    print(
        f"oem-decoder-agreement: rust binary={binary} sha256={digest} "
        f"mtime={time.strftime('%Y-%m-%dT%H:%M:%S', time.localtime(mtime))} "
        f"({fresh}, build took {time.time() - started:.1f}s)"
    )
    return binary


def accepted_by_rust(binary: Path, stream: str) -> list[str]:
    completed = subprocess.run(
        [str(binary), "--hex", stream],
        check=True,
        text=True,
        capture_output=True,
    )
    accepted = []
    for line in completed.stdout.splitlines():
        if not line.startswith("frame ok=1 "):
            continue
        raw = line.rsplit(" raw=", 1)
        if len(raw) != 2:
            raise RuntimeError(f"unexpected Rust output: {line}")
        accepted.append(raw[1])
    return accepted


def main() -> int:
    crate = Path(__file__).resolve().parent.parent
    source_path = Path(os.environ.get("XYAUTO_CAN_OEM_SOURCE", DEFAULT_OEM_SOURCE))
    source = source_path.read_text(encoding="utf-8")
    method = extract_method(source, "canbox_rx_uart")

    # Refuse to compare after a decompiler/source drift that removes any part
    # of the recovered envelope or the stock transmit side effect.
    required = (
        "(buf[data_pos + 2] & 255) + 4",
        "check_word + data_len",
        "(check_word2 & 255) ^ 255",
        "canbox_dev_read(cmd)",
        "onDataSend(ack, 1)",
    )
    missing = [needle for needle in required if needle not in method]
    if missing:
        raise RuntimeError(f"OEM method shape changed; missing: {missing}")

    rust_binary = build_rust_binary(crate)

    with tempfile.TemporaryDirectory(prefix="xyauto-can-oem-") as temporary:
        temporary_path = Path(temporary)
        java_path = temporary_path / "OemHarness.java"
        java_path.write_text(java_harness(method), encoding="utf-8")
        subprocess.run(["javac", str(java_path)], cwd=temporary_path, check=True)
        completed = subprocess.run(
            ["java", "-cp", str(temporary_path), "OemHarness", *STREAMS],
            cwd=temporary_path,
            check=True,
            text=True,
            capture_output=True,
        )

    oracle = {}
    for line in completed.stdout.splitlines():
        index_text, frames_text, ack_text = line.split("\t")
        frames = [] if not frames_text else frames_text.split(",")
        oracle[int(index_text)] = (frames, int(ack_text))

    agreements = 0
    divergences = 0
    for index, stream in enumerate(STREAMS):
        expected, acknowledgements = oracle[index]
        actual = accepted_by_rust(rust_binary, stream)
        if index == CONTIGUOUS_FRAME_VECTOR:
            exact_oem = ["2e1000ef"]
            exact_rust = ["2e1000ef", "2e7f01007f"]
            if expected != exact_oem or actual != exact_rust:
                print(
                    f"stream {index} changed documented divergence: "
                    f"OEM={expected!r} Rust={actual!r}",
                    file=sys.stderr,
                )
                return 1
            divergences += 1
            continue
        if actual != expected:
            print(
                f"stream {index} disagreement: OEM={expected!r} Rust={actual!r}",
                file=sys.stderr,
            )
            return 1
        agreements += 1
        if acknowledgements != len(expected):
            print(
                f"stream {index}: OEM ack count {acknowledgements} != accepted {len(expected)}",
                file=sys.stderr,
            )
            return 1

    print(
        "oem-decoder-agreement: "
        f"PASS ({agreements} agree, {divergences} documented OEM skip divergence; "
        f"synthetic streams; OEM method lifted from {source_path})"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
