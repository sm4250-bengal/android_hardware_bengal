#!/usr/bin/env bash
set -euo pipefail

root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd -P)
mode=${1:-}
[[ $mode == host || $mode == android ]] || { echo "usage: $0 host|android" >&2; exit 2; }
: "${TMPDIR:?set TMPDIR to an absolute private scratch directory}"
[[ $TMPDIR == /* ]] || { echo 'BUILD FAIL: TMPDIR must be absolute' >&2; exit 2; }
[[ ${BENGAL_DESK_TMP:-} == "$TMPDIR" ]] || { echo 'BUILD FAIL: BENGAL_DESK_TMP must equal TMPDIR' >&2; exit 2; }

bash "$root/tools/verify-shaders.sh"
common_git_dir=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)
main_repo=$(cd "$common_git_dir/.." && pwd -P)
ndk=${ANDROID_NDK_ROOT:-$main_repo/tools/android-sdk/ndk/28.2.13676358}
prebuilt=$ndk/toolchains/llvm/prebuilt/linux-x86_64
out=$TMPDIR/vulkan-multiview-$mode
mkdir -p "$out"
common=(
  -std=c++17 -O2 -g -Wall -Wextra -Werror -Wno-missing-field-initializers -fPIE -pie
  -I "$root/generated" "$root/src/main.cpp" -lvulkan
  -o "$out/bengal_vulkan_multiview_smoke"
)
if [[ $mode == host ]]; then
  clang++ -idirafter "$prebuilt/sysroot/usr/include" "${common[@]}"
else
  "$prebuilt/bin/aarch64-linux-android35-clang++" -static-libstdc++ \
    -Wl,--exclude-libs,ALL "${common[@]}"
  "$prebuilt/bin/llvm-strip" --strip-debug "$out/bengal_vulkan_multiview_smoke"
fi
sha=$(sha256sum "$out/bengal_vulkan_multiview_smoke" | awk '{print $1}')
bytes=$(stat -c '%s' "$out/bengal_vulkan_multiview_smoke")
printf 'BUILD PASS mode=%s artifact=%s bytes=%s sha256=%s\n' "$mode" \
  "$out/bengal_vulkan_multiview_smoke" "$bytes" "$sha"
