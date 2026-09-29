#!/bin/bash
set -euo pipefail
[[ $# == 2 && $1 == /* && $2 == /* ]] || {
    echo "Usage: $0 ABS_PATCHED_SKIA_SOURCE ABS_FRESH_OUTPUT" >&2; exit 64;
}
src=$(realpath -e -- "$1")
out=$2
[[ ! -e $out ]] || { echo "Require fresh output" >&2; exit 64; }
mkdir -p -- "$out"
cd -- "$src"
compiler=${CXX:-clang++}
flags=(-std=c++17 -O1 -DNDEBUG -DSK_DISABLE_TRACING -DSK_USE_INTERNAL_VULKAN_HEADERS
       '-DSK_USER_CONFIG_HEADER="experimental/bengal-vulkan10/SkUserConfig.h"'
       -I. -Ivma_android/include -ffunction-sections -fdata-sections)
sources=(src/gpu/vk/VulkanInterface.cpp src/gpu/vk/VulkanUtilsPriv.cpp
         src/gpu/vk/vulkanmemoryallocator/VulkanAMDMemoryAllocator.cpp)
"$compiler" "${flags[@]}" -DSK_BENGAL_EXPERIMENTAL_VULKAN_10 \
    experimental/bengal-vulkan10/host_test.cpp "${sources[@]}" \
    -Wl,--gc-sections -o "$out/host-test"
"$out/host-test" | tee "$out/results.txt"
# Compare macro-off compiler inputs against the exact Android source base. Fixed __LINE__ removes
# only diagnostic line-number movement introduced by #ifdef blocks. Both inputs use stdin so
# __FILE__ is identical. No behavioral source normalization is performed.
base=22f5edb6c1bf350d1d9d67bb429db1a5539f3c05
for source in "${sources[@]}"; do
    label=${source##*/}
    git show "$base:$source" | "$compiler" "${flags[@]}" -Wno-builtin-macro-redefined \
        -D__LINE__=1 -E -P -x c++ - > "$out/$label.base.ii"
    "$compiler" "${flags[@]}" -Wno-builtin-macro-redefined -D__LINE__=1 \
        -E -P -x c++ - < "$source" > "$out/$label.off.ii"
    diff -B -- "$out/$label.base.ii" "$out/$label.off.ii"
    echo "PASS macro-off unchanged preprocessed source: $source" | tee -a "$out/results.txt"
done
