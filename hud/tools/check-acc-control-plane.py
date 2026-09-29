#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Static guard for D3-0102's single-owner MCU ACC control plane."""

import pathlib
import sys


def require(text, needle, label):
    if needle not in text:
        raise SystemExit(f"ACC control check FAIL: missing {label}: {needle!r}")


def forbid(text, needle, label):
    if needle in text:
        raise SystemExit(f"ACC control check FAIL: forbidden {label}: {needle!r}")


def main(root):
    hud = pathlib.Path(root)
    acc = (hud / "src/mcu/acc.rs").read_text()
    serial = (hud / "src/mcu/serial.rs").read_text()
    service = (hud / "src/service.rs").read_text()
    binder = (hud / "src/binder.rs").read_text()
    cargo = (hud / "Cargo.toml").read_text()

    require(serial, "    machine: Machine,", "UART-owned machine")
    if serial.count("    machine: Machine,") != 1:
        raise SystemExit("ACC control check FAIL: serial must own exactly one Machine")
    if serial.count("self.io.write_all(") != 1:
        raise SystemExit("ACC control check FAIL: serial must have exactly one UART write_all")
    require(acc, "pub const FINAL_STAGE_DELAY_MS: u64 = 500", "exact +500ms delay")
    require(serial, "publish_state_change(before, \"timer\")", "post-timer state publication")
    require(serial, "request_sleep(self.now_ms)", "nonblocking sleep seam")
    forbid(serial, "SysfsSuspend", "direct sysfs suspend")
    forbid(serial, "SuspendRequest", "direct suspend abstraction")
    forbid(serial, "/sys/power/state", "direct power-state path")
    forbid(cargo, "direct-suspend", "direct-suspend feature")

    forbid(service, "Machine", "service-side state machine")
    forbid(service, "LifecycleHooks", "service lifecycle hooks")
    forbid(service, "on_mcu_event", "service-side MCU state entry")
    forbid(service, "self.usb.acc_off()", "automatic USB ACC-off")
    forbid(service, "self.usb.acc_on(", "automatic USB ACC-on")
    require(service, "pub fn publish_acc_state", "state publication entry")
    require(service, "pub fn record_mcu_event", "decoded event accounting entry")

    require(binder, "struct ReaderHooks", "reader-local hooks")
    require(binder, "struct AccControlMailbox", "local ACC mailbox")
    require(
        binder,
        "self.control.push(AccControl::SleepRequest { now_ms });",
        "sleep request queue",
    )
    require(binder, "flush_callback_mailbox(", "post-core Binder flush")
    forbid(binder, "BridgeHooksTodo", "old reader hooks")
    forbid(binder, "NoopSuspend", "old direct suspend hook")
    forbid(binder, ".on_mcu_event(", "duplicate service machine entry")
    forbid(binder, "PowerManager", "framework power call")
    forbid(binder, "/sys/power/state", "direct power-state path")
    print("ACC control check PASS: one UART machine/writer; queued state and sleep seam")


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit("usage: check-acc-control-plane.py <hud-root>")
    main(sys.argv[1])
