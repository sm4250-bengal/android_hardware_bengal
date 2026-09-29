#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
set -euo pipefail
[[ $# == 2 && $1 == /* && $2 == /* ]] || {
    echo "usage: bash desk.sh ABSOLUTE_OWN_SCRATCH ABSOLUTE_EXISTING_LINEAGE" >&2; exit 64;
}
scratch=$1
lineage=$2
here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
mkdir -p "$scratch"
export TMPDIR="$scratch"
clang++ -std=c++17 -Wall -Wextra -Werror -O2 "$here/observation-test.cpp" -o "$scratch/observation-test"
"$scratch/observation-test"
# Read-only syntax check: actual generated frozen V6 APIs; no Binder/Power stubs.
# Bionic availability annotations and assert ABI only need host compatibility.
binder="$lineage/frameworks/native/libs/binder/ndk"
generated="$lineage/out/soong/.intermediates/hardware/interfaces"
clang++ -std=c++17 -Wall -Wextra -Werror -Wno-nullability-completeness -fsyntax-only \
    '-D__BIONIC_AVAILABILITY_GUARD(x)=0' \
    '-D__unused=__attribute__((__unused__))' \
    -include "$here/../tests/host-binder-compat.h" \
    -I"$binder/include_ndk" -I"$binder/include_cpp" -I"$binder/include_platform" \
    -I"$generated/power/aidl/android.hardware.power-V6-ndk-source/gen/include" \
    -I"$generated/common/aidl/android.hardware.common-V2-ndk-source/gen/include" \
    -I"$generated/common/fmq/aidl/android.hardware.common.fmq-V1-ndk-source/gen/include" \
    -idirafter "$lineage/bionic/libc/include" "$here/client.cpp"
echo "PASS real generated V6 + Bionic property declarations host syntax (NOT Android link)"
readelf -h -l "$scratch/observation-test"
sha256sum "$scratch/observation-test"
stat -c '%s %n' "$scratch/observation-test"
