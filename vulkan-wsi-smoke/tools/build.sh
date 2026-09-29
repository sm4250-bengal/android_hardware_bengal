#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
mode=${1:-}
if [[ "$mode" != host && "$mode" != android && "$mode" != android-ganesh && "$mode" != android-mesa26 ]]; then
  echo "usage: $0 host|android|android-ganesh|android-mesa26" >&2
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
out="$TMPDIR/vulkan-wsi-$mode"
mkdir -p "$out"

if [[ "$mode" == host ]]; then
  clang++ -std=c++17 -O2 -g -Wall -Wextra -Werror \
    -DBENGAL_WSI_PRIVATE_GANESH=1 -I "$root/src" \
    "$root/src/logic.cpp" "$root/tests/logic_test.cpp" \
    -o "$out/logic_test"
  clang++ -std=c++17 -O2 -g -Wall -Wextra -Werror \
    -I "$root/src" "$root/src/frame_loop.cpp" \
    "$root/tests/frame_loop_test.cpp" -o "$out/frame_loop_test"
  clang++ -std=c++17 -O2 -g -Wall -Wextra -Werror \
    -DBENGAL_WSI_MESA26_PROFILE=1 -I "$root/src" \
    "$root/src/logic.cpp" "$root/tests/logic_test.cpp" \
    -o "$out/logic_mesa26_test"
  "$out/logic_test"
  "$out/frame_loop_test"
  "$out/logic_mesa26_test"
  logic_sha=$(sha256sum "$out/logic_test" | awk '{print $1}')
  frame_loop_sha=$(sha256sum "$out/frame_loop_test" | awk '{print $1}')
  mesa26_logic_sha=$(sha256sum "$out/logic_mesa26_test" | awk '{print $1}')
  echo "BUILD PASS mode=host logicArtifact=$out/logic_test logicSha256=$logic_sha mesa26LogicArtifact=$out/logic_mesa26_test mesa26LogicSha256=$mesa26_logic_sha frameLoopArtifact=$out/frame_loop_test frameLoopSha256=$frame_loop_sha"
  exit 0
fi

common_git_dir=$(git -C "$root" rev-parse --path-format=absolute --git-common-dir)
main_repo=$(cd "$common_git_dir/.." && pwd -P)
ndk=${ANDROID_NDK_ROOT:-$main_repo/tools/android-sdk/ndk/28.2.13676358}
prebuilt="$ndk/toolchains/llvm/prebuilt/linux-x86_64"
cc="$prebuilt/bin/aarch64-linux-android35-clang"
cxx="$prebuilt/bin/aarch64-linux-android35-clang++"
glue="$ndk/sources/android/native_app_glue"
common=(
  -O2
  -g
  -fPIC
  -Wall
  -Wextra
  -Werror
  -DVK_USE_PLATFORM_ANDROID_KHR=1
  -I "$root/generated"
  -I "$root/src"
  -I "$glue"
)
objects=(
  "$out/android_native_app_glue.o"
  "$out/frame_loop.o"
  "$out/logic.o"
  "$out/main.o"
)
link_inputs=()
link_options=()
link_libraries=(-landroid -llog -lvulkan)
ganesh_common=()
if [[ "$mode" == android-ganesh ]]; then
  skia_source=${SKIA_SOURCE_ROOT:-/home/meinte/android/bengal/.worktrees/review-skiavk-driver-identity-src}
  skia_archive=${SKIA_STATIC_ARCHIVE:-/home/meinte/android/bengal/kernel/out/.tmp/review-skiavk-driver-identity/arm64-build/libskia.a}
  expected_skia_source=2b8abbdec5fc4b1350fc196689e4d330cc918fe1
  expected_skia_sha=3ac3f4eb10271ba5feef90043a639b1fbf17f996e29ea017326c1719267a567d
  expected_skia_bytes=17073132
  [[ -d "$skia_source" && ! -L "$skia_source" ]] || {
    echo "BUILD FAIL: missing real isolated Skia source tree: $skia_source" >&2; exit 1; }
  [[ -f "$skia_archive" && ! -L "$skia_archive" ]] || {
    echo "BUILD FAIL: missing real isolated Skia archive: $skia_archive" >&2; exit 1; }
  actual_skia_source=$(git -C "$skia_source" rev-parse HEAD)
  actual_skia_status=$(git -C "$skia_source" status --porcelain=v1 --untracked-files=all)
  actual_skia_sha=$(sha256sum "$skia_archive" | awk '{print $1}')
  actual_skia_bytes=$(stat -c '%s' "$skia_archive")
  [[ "$actual_skia_source" == "$expected_skia_source" && -z "$actual_skia_status" ]] || {
    echo "BUILD FAIL: isolated Skia source identity/cleanliness mismatch" >&2; exit 1; }
  [[ "$actual_skia_sha" == "$expected_skia_sha" &&
     "$actual_skia_bytes" == "$expected_skia_bytes" ]] || {
    echo "BUILD FAIL: isolated Skia archive identity mismatch" >&2; exit 1; }
  common+=(-DBENGAL_WSI_PRIVATE_GANESH=1)
  ganesh_common=(
    "${common[@]}"
    -DSK_BENGAL_EXPERIMENTAL_VULKAN_10
    -DNDEBUG
    -DSK_USE_INTERNAL_VULKAN_HEADERS
    -DSK_VULKAN
    -DSK_CODEC_DECODES_BMP
    -DSK_CODEC_DECODES_WBMP
    -DSK_ENABLE_PRECOMPILE
    -DSK_GANESH
    -DSK_DISABLE_TRACING
    '-DSK_USER_CONFIG_HEADER="experimental/bengal-vulkan10/SkUserConfig.h"'
    -fno-exceptions
    -fno-rtti
    -fvisibility=hidden
    -fvisibility-inlines-hidden
    -ffp-contract=off
    -I "$skia_source"
    -I "$skia_source/include/third_party/vulkan"
  )
  link_options+=(-Wl,--gc-sections)
  link_libraries+=(-ldl)
fi
if [[ "$mode" == android-mesa26 ]]; then
  common+=(-DBENGAL_WSI_MESA26_PROFILE=1)
fi
"$cc" -O2 -g -fPIC -Wall -Wextra -Werror -Wno-unused-parameter \
  -I "$glue" \
  -c "$glue/android_native_app_glue.c" -o "$out/android_native_app_glue.o"
"$cxx" -std=c++17 "${common[@]}" -c "$root/src/logic.cpp" \
  -o "$out/logic.o"
"$cxx" -std=c++17 "${common[@]}" -c "$root/src/frame_loop.cpp" \
  -o "$out/frame_loop.o"
"$cxx" -std=c++17 "${common[@]}" -c "$root/src/main.cpp" \
  -o "$out/main.o"
if [[ "$mode" == android-ganesh ]]; then
  "$cxx" -std=c++17 "${ganesh_common[@]}" -c "$root/src/ganesh_renderer.cpp" \
    -o "$out/ganesh_renderer.o"
  objects+=("$out/ganesh_renderer.o")
  link_inputs+=("$skia_archive")
fi
"$cxx" -shared -static-libstdc++ -Wl,--exclude-libs,ALL \
  "${link_options[@]}" \
  -Wl,-soname,libbengal_vulkan_wsi_smoke.so \
  "${objects[@]}" "${link_inputs[@]}" \
  "${link_libraries[@]}" -o "$out/libbengal_vulkan_wsi_smoke.so"
"$prebuilt/bin/llvm-strip" --strip-debug \
  "$out/libbengal_vulkan_wsi_smoke.so"
sha=$(sha256sum "$out/libbengal_vulkan_wsi_smoke.so" | awk '{print $1}')
bytes=$(stat -c '%s' "$out/libbengal_vulkan_wsi_smoke.so")
echo "BUILD PASS mode=$mode artifact=$out/libbengal_vulkan_wsi_smoke.so bytes=$bytes sha256=$sha"
if [[ "$mode" == android-ganesh ]]; then
  echo "BUILD INPUT privateSkiaSource=$actual_skia_source privateSkiaArchive=$skia_archive privateSkiaBytes=$actual_skia_bytes privateSkiaSha256=$actual_skia_sha"
fi
