#!/usr/bin/env python3
"""Read-only exact compiled d84 contract; no tracefs access or build."""
import argparse
import hashlib
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

# Explicit compiled artifacts, not a permissive set of interchangeable hashes.
# The DWARF producer must reproduce the selected stripped module byte-for-byte.
VENDOR_PROFILES = {
    "original": (
        "f9fff6cc074575fb3f2b653faeca20c551289d8952e4fd9813fa13ed6195949b",
        "41f0a214babb9109569ba21b6f5ba7320bc006f625bc6779f8250358ce9222a0"),
    "autocts": (
        "ead8f328b0b4e72b3b91cf40e2daa77e6299705a2deb927e98bd77bfef1f1542",
        "a9c3059a25e59242c9d7e72d37d8e6f45fa23fd18e52b4a84459c543537c19de"),
}


def require(ok, message):
    if not ok:
        raise ValueError(message)


def command(*args):
    return subprocess.check_output(args, text=True, env={**os.environ,
        "GIT_OPTIONAL_LOCKS": "0", "GIT_NO_LAZY_FETCH": "1"})


def main():
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--repo", type=Path, required=True)
    p.add_argument("--gki", type=Path, required=True, help="exact d84 output directory")
    p.add_argument("--kernel-source", type=Path, required=True)
    p.add_argument("--vendor-profile", choices=VENDOR_PROFILES, default="original",
                   help="explicit compiled UART identity; original remains the default")
    p.add_argument("--vendor-module", type=Path, required=True, help="selected stripped vendor module")
    p.add_argument("--vendor-unstripped", type=Path, required=True, help="selected matching DWARF module")
    p.add_argument("--scratch", type=Path, required=True, help="owned existing kernel/out/.tmp directory")
    a = p.parse_args()
    prefix = "platform/hardware/xyauto/bt-trace/"
    owner = (a.repo / prefix / "src/owner.cpp").read_text()
    baseline = command("git", "-C", str(a.repo), "show",
                       "df962383bf2da2eaf0381a380aacfbaad86372c3:" + prefix + "src/owner.cpp")
    legacy = lambda s: s.split("std::vector<std::string> definitions(", 1)[1].split("\n}\n", 1)[0]
    require(legacy(owner) == legacy(baseline), "legacy seven-event function changed")
    require("dma_d84_definitions" in owner and " dma_addr=+664($arg1):x64 remaining=+720($arg1):u32" in owner,
            "missing DMA discriminator")
    require('const std::string module = "bengal_bt_uart:";' in owner and
            'qcom_geni_serial_flush_buffer_dma" + uart + fields' in owner and
            'state=+208($arg1):x64 mapbase=+328($arg1):x64 line=+316($arg1):u32' in owner and
            'state=+208(%x0):x64 mapbase=+328(%x0):x64 line=+316(%x0):u32' in owner and
            'schema=d84-vendor-v3' in owner, "missing vendor-qualified raw layout/schema")
    vendor_sha = hashlib.file_digest(a.vendor_module.open("rb"), "sha256").hexdigest()
    module_sha, dwarf_sha = VENDOR_PROFILES[a.vendor_profile]
    require(vendor_sha == module_sha, "wrong vendor module for " + a.vendor_profile)
    require(hashlib.file_digest(a.vendor_unstripped.open("rb"), "sha256").hexdigest() ==
            dwarf_sha, "wrong vendor DWARF witness for " + a.vendor_profile)
    require(a.scratch.is_dir() and str(a.scratch.resolve()).startswith("/home/meinte/android/bengal/kernel/out/.tmp/"),
            "scratch must be owned existing Bengal scratch")
    # Same two operations as build-modules.sh. Byte equality binds DWARF to
    # the deployed module; matching only a build-id or GKI BTF is insufficient.
    with tempfile.TemporaryDirectory(prefix="vendor-witness-", dir=a.scratch) as tmp:
        stripped = Path(tmp) / "vendor.ko"
        command("llvm-strip", "--strip-debug", "-o", str(stripped), str(a.vendor_unstripped))
        command("llvm-objcopy", "--remove-section=.BTF", "--remove-section=.BTF.base", str(stripped))
        require(stripped.read_bytes() == a.vendor_module.read_bytes(), "DWARF module does not reproduce deployed bytes")
    sections = command("readelf", "-SW", str(a.vendor_module))
    require(".BTF" not in sections, "expected BTF-stripped vendor artifact")
    vendor_symbols = command("nm", "--defined-only", str(a.vendor_module))
    for name in ("qcom_geni_serial_start_tx_dma", "qcom_geni_serial_tx_empty", "qcom_geni_serial_flush_buffer_dma"):
        require(re.search(r"^[0-9a-f]+ [tT] " + name + r"$", vendor_symbols, re.M), f"missing vendor {name}")
    for typ, fields in (("qcom_geni_serial_port", (("uport", 0, 544), ("se", 544, 96),
                                                ("tx_dma_addr", 664, 8), ("tx_remaining", 720, 4))),
                        ("uart_port", (("state", 208, 8), ("mapbase", 328, 8), ("line", 316, 4)))):
        dwarf = command("pahole", "-F", "dwarf", "-C", typ, str(a.vendor_unstripped))
        for field, offset, size in fields:
            require(re.search(r"\b" + field + r";\s*/\*\s*" + str(offset) + r"\s+" + str(size) + r"\s*\*/", dwarf),
                    f"wrong vendor DWARF {typ}.{field}")
    release = (a.gki / "include/config/kernel.release").read_text().strip()
    require(release == "6.18.32-4k-gd84cfa4479d9", "not the d84 compiled output")
    image = a.gki / "arch/arm64/boot/Image"
    digest = hashlib.file_digest(image.open("rb"), "sha256").hexdigest()
    require(digest == "4da78c722c1665fdb3e3e6a93e4e3737affcf1542737a2d0a6783798fdb4783e", "wrong Image bytes")
    require(command("git", "-C", str(a.kernel_source), "rev-parse", "HEAD").strip() ==
            "d84cfa4479d96e063067ca9ca1a23375483b93fd", "wrong kernel source pin")
    config = (a.gki / ".config").read_text()
    for flag in ("ARM64", "ARM64_4K_PAGES", "SERIAL_QCOM_GENI", "QCOM_GENI_SE", "KPROBES", "KPROBE_EVENTS", "DEBUG_INFO_BTF"):
        require(f"CONFIG_{flag}=y\n" in config, f"missing {flag}")
    symbols = (a.gki / "System.map").read_text()
    for name in ("serdev_device_write_buf", "ttyport_receive_buf", "tty_wait_until_sent", "uart_chars_in_buffer",
                 "qcom_geni_serial_start_tx_dma", "qcom_geni_serial_tx_empty", "serdev_device_close",
                 "qcom_geni_serial_flush_buffer", "geni_se_tx_dma_prep", "geni_se_tx_dma_unprep", "__uart_start"):
        require(re.search(r"^[0-9a-f]+ [tT] " + name + r"$", symbols, re.M), f"missing compiled {name}")
    require(not re.search(r" [tT] qcom_geni_serial_handle_tx_dma$", symbols, re.M), "unexpected source contract: standalone handle_tx_dma")
    btf = command("pahole", "-F", "btf", "-C", "qcom_geni_serial_port", str(a.gki / "vmlinux"))
    for field, offset, size in (("uport", 0, 544), ("se", 544, 96), ("tx_dma_addr", 664, 8), ("tx_remaining", 720, 4)):
        require(re.search(r"\b" + field + r";\s*/\*\s*" + str(offset) + r"\s+" + str(size) + r"\s*\*/", btf),
                f"wrong actual BTF {field}")
    asm = command("aarch64-linux-gnu-objdump", "-d", "--disassemble=geni_se_tx_dma_unprep", str(a.gki / "vmlinux"))
    instructions = asm.split("<geni_se_tx_dma_unprep>:\n", 1)[1].split("\n\n", 1)[0]
    require("bti\tc" in instructions.splitlines()[0] and instructions.index("bti") < instructions.index("paciasp"),
            "unprep entry no longer precedes LR signing")
    # v3 observes the compiled branch immediately after pm_runtime_get, not
    # a guessed source offset or the void function's undefined return value.
    gate_asm = command("aarch64-linux-gnu-objdump", "-d", "--disassemble=__uart_start", str(a.gki / "vmlinux"))
    entry = int(re.search(r"^([0-9a-f]+) <__uart_start>:", gate_asm, re.M)[1], 16)
    for offset, instruction in ((0x3c, r"ldr\s+x19, \[x20, #352\]"),
                                (0x44, r"mov\s+x0, x19"),
                                (0x48, r"bl\s+[0-9a-f]+ <__pm_runtime_resume>"),
                                (0x4c, r"tbz\s+w0, #31,")):
        require(re.search(r"^" + format(entry + offset, "x") + r":\s+[0-9a-f]+\s+" + instruction, gate_asm, re.M),
                f"post-PM register/site witness changed at {offset:#x}")
    for typ, fields in (("uart_state", (("port", 0, 408), ("uart_port", 440, 8))),
                        ("tty_port", (("tty", 144, 8),)),
                        ("tty_struct", (("stopped", 500, 1), ("flow", 496, 8))),
                        ("uart_port", (("flags", 272, 8), ("hw_stopped", 284, 1), ("mapbase", 328, 8),
                                       ("dev", 344, 8), ("port_dev", 352, 8))),
                        ("device", (("power", 272, 336),)),
                        ("serial_port_device", (("dev", 0, 928),)),
                        ("dev_pm_info", (("runtime_status", 236, 4), ("runtime_error", 244, 4)))):
        layout = command("pahole", "-F", "btf", "-C", typ, str(a.gki / "vmlinux"))
        for field, offset, size in fields:
            require(re.search(r"\b" + field + r";\s*/\*\s*" + str(offset) + r"\s+" + str(size) + r"\s*\*/", layout),
                    f"wrong gate BTF {typ}.{field}")
    for typ, field, bits, offset in (("serial_port_device", "tx_enabled", 1, 936),
                                    ("dev_pm_info", "disable_depth", 3, 224)):
        layout = command("pahole", "-F", "btf", "-C", typ, str(a.gki / "vmlinux"))
        require(re.search(r"\b" + field + ":" + str(bits) + r";\s*/\*\s*" + str(offset) + r": 0\s+4\s*\*/", layout),
                f"wrong gate bitfield {typ}.{field}")
    for field in ("sw_stop=state->port.tty->flow.stopped:u8", "hw_stop=state->uart_port->hw_stopped:u8",
                  "tx_en=state->uart_port->port_dev->tx_enabled\"", "_gate_pm __uart_start+76 uport=%x20:x64",
                  "pm_ret=%x0:s32", "pm_status=+508(%x19):s32", "pm_error=+516(%x19):s32",
                  "pm_disabled=+496(%x19):b3@0/32", "phys_disabled=+496(+344(%x20)):b3@0/32",
                  "tx_en=+936(%x19):b1@0/32", "mapbase == 0x4a8c000"):
        require(field in owner, f"missing gate definition/filter {field}")
    regs = (a.kernel_source / "arch/arm64/kernel/ptrace.c").read_text()
    require('"x" #r' in regs and "GPR_OFFSET_NAME(30)" in regs and "GPR_OFFSET_NAME(0)" in regs,
            "x0/x30 register spelling unavailable")
    docs = (a.kernel_source / "Documentation/trace/kprobetrace.rst").read_text()
    probe_source = (a.kernel_source / "kernel/trace/trace_kprobe.c").read_text()
    require('seq_printf(m, " %s+%u", trace_kprobe_symbol(tk),' in probe_source,
            "symbol offset canonical readback spelling changed")
    require("Function arguments can be accessed at kretprobe using $arg<N>" in docs and
            "symbol-string type ('symstr')" in docs, "saved return arguments/caller format unsupported")
    print(f"PASS exact compiled d84 release={release} ImageSHA={digest} bytes={image.stat().st_size}")
    print("PASS eleven compiled symbols, BTF offsets/sizes/bitfields, exact post-PM site/registers, entry LR before PAC, saved return args/symstr")
    print(f"PASS vendor profile={a.vendor_profile} DWARF reproduces selected SHA={vendor_sha} bytes={a.vendor_module.stat().st_size}; seven vendor offsets/sizes, three local symbols, no BTF")
    print("PASS legacy seven-event source byte-identical; runtime registration UNVERIFIED; no physical DMA claim")


if __name__ == "__main__":
    try:
        main()
    except (ValueError, OSError, subprocess.CalledProcessError, IndexError) as ex:
        print(f"FAIL {ex}", file=sys.stderr)
        sys.exit(1)
