#!/usr/bin/env python3
# SPDX-License-Identifier: Apache-2.0
"""Desk only: real Make source selections and read-only generated AIDL syntax."""
import os
from collections import Counter
from pathlib import Path
import re
import subprocess
import sys

repo = Path(__file__).resolve().parents[5]
lineage = Path(sys.argv[1]).resolve()
device = 'platform/device/xyauto/bengal3/'
env = {'PATH': os.environ['PATH'], 'LANG': 'C'}
variables = ('PRODUCT_PACKAGES', 'PRODUCT_COPY_FILES', 'BOARD_VENDOR_SEPOLICY_DIRS')
suffix = '\n' + '\n'.join('$(info ' + key + '=$(' + key + '))' for key in variables)
suffix += '\n.PHONY: all\nall:\n\t@:\n'

def render(original=False, **knobs):
    paths = [device + 'BoardConfig.mk', device + 'device.mk']
    sources = [subprocess.check_output(['git', 'show', 'cea4aa09:' + p], cwd=repo,
                                      text=True) if original else (repo / p).read_text()
               for p in paths]
    result = subprocess.run(['make', '-j1', '--no-print-directory', '-f', '-', 'all',
                             'BENGAL_BUILD_PROFILE=bringup', 'TARGET_BUILD_VARIANT=userdebug',
                             'TARGET_COPY_OUT_VENDOR=vendor',
                             'TARGET_COPY_OUT_SYSTEM_EXT=system/system_ext'] +
                            [key + '=' + value for key, value in knobs.items()],
                            input='\n'.join(sources) + suffix, cwd=repo / 'platform',
                            env=env, text=True, capture_output=True, timeout=10)
    values = dict(line.split('=', 1) for line in result.stdout.splitlines()
                  if any(line.startswith(key + '=') for key in variables))
    return result, values

before, expected = render(True)
assert before.returncode == 0, before.stderr
for knobs in ({}, {'BENGAL_REAL_POWER': '0', 'BENGAL_REAL_THERMAL': '0'}):
    result, actual = render(**knobs)
    assert result.returncode == 0 and actual == expected, (result.stderr, actual, expected)
for power, thermal in (('1', '0'), ('0', '1'), ('1', '1')):
    result, actual = render(BENGAL_REAL_POWER=power, BENGAL_REAL_THERMAL=thermal)
    assert result.returncode == 0, result.stderr
    packages = actual['PRODUCT_PACKAGES'].split()
    assert packages.count('android.hardware.power-service.bengal') == int(power)
    assert packages.count('android.hardware.power-service.example') == 1 - int(power)
    assert packages.count('android.hardware.thermal-service.bengal') == int(thermal)
    assert packages.count('com.android.hardware.thermal') == 1 - int(thermal)
    assert ('/sepolicy/power' in actual['BOARD_VENDOR_SEPOLICY_DIRS']) == bool(int(power))
    assert ('/sepolicy/thermal' in actual['BOARD_VENDOR_SEPOLICY_DIRS']) == bool(int(thermal))
    copies = Counter(expected['PRODUCT_COPY_FILES'].split())
    for module, enabled in (('power', power), ('thermal', thermal)):
        if enabled == '1':
            copies[f'hardware/xyauto/{module}/{module}-bengal.rc:'
                   f'vendor/etc/init/{module}-bengal.rc'] += 1
    assert Counter(actual['PRODUCT_COPY_FILES'].split()) == copies
for module in ('power', 'thermal'):
    blueprint = (repo / 'platform/hardware/xyauto' / module / 'Android.bp').read_text()
    assert not re.search(r'^\s*init_rc\s*:', blueprint, re.MULTILINE), module
for knob in ('BENGAL_REAL_POWER', 'BENGAL_REAL_THERMAL'):
    for bad in ('', '2', 'true', '01', '1 0'):
        result, _ = render(**{knob: bad})
        assert result.returncode != 0 and knob in result.stderr, (knob, bad)
print('PASS full GNU Make default/off byte equality, independent replacements, invalid values')
print('PASS enabled-only RC copies exactly once; no symlink-preserving init_rc duplicate')

binder = lineage / 'frameworks/native/libs/binder/ndk'
interfaces = lineage / 'out/soong/.intermediates/hardware/interfaces'
args = ['g++', '-std=c++17', '-fsyntax-only', '-include',
        str(repo / 'platform/hardware/xyauto/power/tests/host-binder-compat.h')]
args += ['-I' + str(binder / p) for p in ('include_ndk', 'include_cpp', 'include_platform')]
for module, generated in (
    ('thermal', ['thermal/aidl/android.hardware.thermal-V3-ndk-source']),
    ('power', ['power/aidl/android.hardware.power-V6-ndk-source',
               'common/aidl/android.hardware.common-V2-ndk-source',
               'common/fmq/aidl/android.hardware.common.fmq-V1-ndk-source'])):
    includes = ['-I' + str(interfaces / p / 'gen/include') for p in generated]
    subprocess.run(args + includes + [str(repo / 'platform/hardware/xyauto' / module / 'service.cpp')],
                   check=True, timeout=30)
    print('PASS ' + module + ' adapter syntax against actual frozen generated AIDL headers')
verifier = lineage / 'out/host/linux-x86/bin/host_init_verifier'
if verifier.is_file():
    for module in ('power', 'thermal'):
        subprocess.run([str(verifier), str(repo / 'platform/hardware/xyauto' / module /
                                         (module + '-bengal.rc'))], check=True, timeout=10)
    print('PASS installed host_init_verifier both service rc files')
else:
    print('UNVERIFIED installed host_init_verifier unavailable')
