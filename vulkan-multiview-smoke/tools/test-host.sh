#!/bin/bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$script_dir/.." && pwd)
tmp=${BENGAL_DESK_TMP:-${TMPDIR:-}}
[[ "$tmp" = /* ]] || { echo 'BENGAL_DESK_TMP or TMPDIR must be absolute' >&2; exit 2; }
icd=${BENGAL_SOFTWARE_VULKAN_ICD:?set BENGAL_SOFTWARE_VULKAN_ICD to the lavapipe ICD JSON}
[[ -f "$icd" ]] || { echo "missing ICD: $icd" >&2; exit 2; }
export VK_ICD_FILENAMES="$icd"
export VK_DRIVER_FILES="$icd"

"$root/tools/build.sh" host
common_git_dir=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)
main_repo=$(cd "$common_git_dir/.." && pwd -P)
ndk=${ANDROID_NDK_ROOT:-$main_repo/tools/android-sdk/ndk/28.2.13676358}
clang++ -std=c++17 -O2 -Wall -Wextra -Werror -Wno-missing-field-initializers \
  -idirafter "$ndk/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/include" \
  -I "$root/generated" "$root/tools/test-contract.cpp" -lvulkan \
  -o "$tmp/vulkan-multiview-host/test-contract"
"$tmp/vulkan-multiview-host/test-contract" | tee "$tmp/vulkan-multiview-host/contract.log"
bin="$tmp/vulkan-multiview-host/bengal_vulkan_multiview_smoke"
"$bin" --self-test | tee "$tmp/vulkan-multiview-host/self-test.log"

set +e
timeout 20s "$bin" >"$tmp/vulkan-multiview-host/default.log" 2>&1
status=$?
set -e
[[ $status -eq 21 ]] || { cat "$tmp/vulkan-multiview-host/default.log" >&2; exit 1; }
grep -q 'IDENTITY REJECTED.*software_device' "$tmp/vulkan-multiview-host/default.log"

set +e
timeout 20s "$bin" --allow-software-control | tee "$tmp/vulkan-multiview-host/control.log"
status=$?
set -e
[[ $status -eq 3 ]] || { cat "$tmp/vulkan-multiview-host/control.log" >&2; exit 1; }
grep -q 'READBACK verdict=PASS' "$tmp/vulkan-multiview-host/control.log"
grep -q 'CONTROL verdict=NOT_HARDWARE' "$tmp/vulkan-multiview-host/control.log"
echo 'HOST_TEST verdict=PASS forced_icd=lavapipe default=identity_rejected control=readback_pass'
