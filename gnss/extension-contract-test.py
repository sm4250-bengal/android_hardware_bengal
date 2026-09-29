#!/usr/bin/env python3
"""Source regression for unavailable AIDL extensions (D3-0146 boot crash).

Android17 GnssHal wraps a successful getter without checking its Binder for
null. Absence must be an unsuccessful status, not success-with-null.
"""
from pathlib import Path
import re
import unittest


class ExtensionContract(unittest.TestCase):
    def test_absent_extensions_report_unsupported(self):
        source = Path(__file__).with_name("Gnss.h").read_text()
        names = (
            "Psds", "GnssBatching", "GnssGeofence",
            "GnssNavigationMessage", "MeasurementCorrections",
        )
        for name in names:
            with self.subTest(extension=name):
                body = re.search(
                    rf"Status getExtension{name}\([^\n]+override\s*\{{([^}}]+)\}}",
                    source,
                )
                self.assertIsNotNone(body)
                self.assertEqual(
                    body.group(1).strip(),
                    "p->reset(); return unsupported();",
                )


if __name__ == "__main__":
    unittest.main()
