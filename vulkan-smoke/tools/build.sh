#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mode=${1:-}
if [[ "$mode" != host && "$mode" != android ]]; then
  echo "usage: $0 host|android" >&2
  exit 2
fi
: "${TMPDIR:?set TMPDIR to an absolute private scratch directory}"
case "$TMPDIR" in
  /*) ;;
  *) echo "BUILD FAIL: TMPDIR must be absolute" >&2; exit 2 ;;
esac
if [[ "${BENGAL_DESK_TMP:-}" != "$TMPDIR" ]]; then
  echo "BUILD FAIL: BENGAL_DESK_TMP must equal TMPDIR" >&2
  exit 2
fi

bash "$root/tools/verify-shaders.sh"
common_git_dir=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)
main_repo=$(cd "$common_git_dir/.." && pwd -P)
ndk=${ANDROID_NDK_ROOT:-$main_repo/tools/android-sdk/ndk/28.2.13676358}
prebuilt="$ndk/toolchains/llvm/prebuilt/linux-x86_64"
out="$TMPDIR/vulkan-offscreen-$mode"
mkdir -p "$out"

common=(
  -std=c++17
  -O2
  -g
  -Wall
  -Wextra
  -Werror
  -fPIE
  -pie
  -I "$root/generated"
  "$root/src/main.cpp"
  -lvulkan
  -o "$out/bengal_vulkan_offscreen_smoke"
)

if [[ "$mode" == host ]]; then
  # The NDK directory also contains bionic headers. Search it only after host
  # system headers so <vulkan/vulkan.h> is found without shadowing glibc.
  clang++ -idirafter "$prebuilt/sysroot/usr/include" "${common[@]}"
else
  # Make the standalone desk artifact self-contained. The Soong declaration
  # instead uses Android's platform libc++; the static archive is hidden here
  # so a dlopened Vulkan HAL cannot bind against executable-local C++ symbols.
  "$prebuilt/bin/aarch64-linux-android35-clang++" \
    -static-libstdc++ -Wl,--exclude-libs,ALL "${common[@]}"
  "$prebuilt/bin/llvm-strip" --strip-debug "$out/bengal_vulkan_offscreen_smoke"
fi

sha=$(sha256sum "$out/bengal_vulkan_offscreen_smoke" | awk '{print $1}')
bytes=$(stat -c '%s' "$out/bengal_vulkan_offscreen_smoke")
echo "BUILD PASS mode=$mode artifact=$out/bengal_vulkan_offscreen_smoke bytes=$bytes sha256=$sha"
