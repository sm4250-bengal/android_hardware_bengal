#!/bin/bash
set -euo pipefail
# Generation only. A full build is an explicit, separately scheduled lead action.
if [[ $# != 3 || $1 != /* || $2 != /* || $3 != /* ]]; then
    echo "Usage: $0 ABS_PATCHED_SKIA_SOURCE ABS_NDK ABS_FRESH_OUTPUT" >&2
    exit 64
fi
src=$(realpath -e -- "$1")
ndk=$(realpath -e -- "$2")
out=$3
for path in "$src" "$ndk" "$out"; do
    [[ $path == /* && $path != *[\"\$\\[:space:]]* ]] || {
        echo "Require absolute paths without whitespace or GN metacharacters" >&2; exit 64;
    }
done
[[ ! -e $out ]] || { echo "Output must be fresh; no source/toolchain cache reuse" >&2; exit 64; }
[[ -x $src/bin/gn && -f $src/vma_android/include/vk_mem_alloc.h &&
   -f $src/src/gpu/vk/BengalVulkan10.h &&
   -x $ndk/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android35-clang++ ]] || {
    echo "Missing pinned GN, experimental carrier, bundled VMA, or API35 NDK" >&2; exit 64;
}
carrier=$(cd -- "$(dirname -- "$0")/../../../.." && pwd)
expected_tree=$(<"$carrier/patches/experiments/skia-vulkan10/source-tree.txt")
[[ $(git -C "$src" rev-parse 'HEAD^{tree}') == "$expected_tree" &&
   -z $(git -C "$src" status --porcelain=v1 --untracked-files=all) ]] || {
    echo "Require the exact clean experimental source tree; commit/review changes first" >&2; exit 64;
}
args=$(<"$carrier/patches/experiments/skia-vulkan10/args.gn")
mkdir -p -- "$out"
git -C "$src" rev-parse HEAD > "$out/source-head.txt"
git -C "$src" diff --binary HEAD > "$out/source-diff.patch"
sha256sum "$out/source-diff.patch" "$src/bin/gn" "$ndk/source.properties" > "$out/input-sha256.txt"
git -C "$src" status --porcelain=v1 --untracked-files=all > "$out/source-status.txt"
(cd -- "$src" && bin/gn gen "$out" --fail-on-unused-args --args="$args
ndk = \"$ndk\"")
echo "Generated only: ninja -C '$out' -j4 bengal_skiavk_single_context"
