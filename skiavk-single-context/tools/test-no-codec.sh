#!/bin/bash
set -euo pipefail
[[ $# == 3 && $1 == /* && $2 == /* && $3 == /* ]] || {
    echo "Usage: $0 ABS_PATCHED_SKIA_SOURCE ABS_NDK ABS_FRESH_OUTPUT" >&2; exit 64;
}
src=$(realpath -e -- "$1")
ndk=$(realpath -e -- "$2")
out=$3
[[ ! -e $out ]] || { echo "Require fresh output" >&2; exit 64; }
mkdir -p -- "$out"
cd -- "$src"
compiler=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android35-clang++
nm=$ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-nm
# The actual pre-fix configuration is the valid negative control, not a fake encoder.
git show 08b713ed759ccdce018b89d285f1b4d2ee31a0f3:experimental/bengal-vulkan10/SkUserConfig.h \
    > "$out/legacy-config.h"
flags=(-std=c++17 -O2 -DNDEBUG -DSK_GANESH -DSK_DISABLE_TRACING -I.
       -fno-exceptions -fno-rtti -ffunction-sections -fdata-sections)
"$compiler" "${flags[@]}" "-DSK_USER_CONFIG_HEADER=\"$out/legacy-config.h\"" \
    -c src/core/SkWriteBuffer.cpp -o "$out/legacy.o"
"$compiler" "${flags[@]}" \
    '-DSK_USER_CONFIG_HEADER="experimental/bengal-vulkan10/SkUserConfig.h"' \
    -c src/core/SkWriteBuffer.cpp -o "$out/no-codec.o"
"$nm" --undefined-only --demangle "$out/legacy.o" > "$out/legacy.undefined.txt"
"$nm" --undefined-only --demangle "$out/no-codec.o" > "$out/no-codec.undefined.txt"
rg -q 'SkPngEncoder::Encode' "$out/legacy.undefined.txt"
if rg -q 'SkPngEncoder::Encode' "$out/no-codec.undefined.txt"; then
    echo "FAIL: no-codec configuration still requires PNG encoder" >&2; exit 1
fi
echo "PASS actual arm64 SkWriteBuffer object: pre-fix requires PNG; no-codec fix does not" \
    | tee "$out/results.txt"
