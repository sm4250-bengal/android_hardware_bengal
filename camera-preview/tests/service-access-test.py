#!/usr/bin/env python3
"""Check the camera service's real init groups against its required DAC reads."""
from pathlib import Path
import unittest

ROOT = Path(__file__).resolve().parents[5]
RC = ROOT / "platform/hardware/xyauto/camera-preview/bengal-camera.rc"
UEVENTD = ROOT / "platform/device/xyauto/bengal3/rootdir/ueventd.bengal3.rc"


def service_identity():
    lines = [line.split() for line in RC.read_text().splitlines()]
    user = next(row[1] for row in lines if row and row[0] == "user")
    groups = next(row[1:] for row in lines if row and row[0] == "group")
    return user, groups


def readable(mode, owner, group):
    # A confined root service does not implicitly bypass DAC. Its policy
    # deliberately grants neither DAC_OVERRIDE nor DAC_READ_SEARCH.
    user, groups = service_identity()
    bit = 0o400 if user == owner else 0o040 if group in groups else 0o004
    return bool(mode & bit)


class CameraServiceAccess(unittest.TestCase):
    def test_gpio_read_without_dac_bypass(self):
        rule = next(line.split() for line in UEVENTD.read_text().splitlines()
                    if line.startswith("/dev/gpiochip*"))
        self.assertTrue(readable(int(rule[1], 8), rule[2], rule[3]),
                        "camera cannot open gpiochip: ACC camera-idle receipt never arrives")

    def test_package_uid_resolution_without_dac_bypass(self):
        self.assertTrue(readable(0o640, "system", "package_info"))

    def test_only_required_groups(self):
        user, groups = service_identity()
        self.assertEqual(user, "root")
        self.assertEqual(set(groups), {"root", "package_info", "system"})


if __name__ == "__main__":
    unittest.main()
