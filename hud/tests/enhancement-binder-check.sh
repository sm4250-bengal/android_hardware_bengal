#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Focused compile of the actual HUD crate against frozen V2 and proposed V3.
# V3 is intentionally live-source desk evidence, NOT an AIDL freeze receipt.
set -euo pipefail
repo=$(git -C "$(dirname "$0")" rev-parse --show-toplevel)
main_repo=$(git -C "$repo" worktree list --porcelain | awk '/^worktree / && !seen { print substr($0,10); seen=1 }')
lineage=${BENGAL_LINEAGE_SRC:-$main_repo/lineage-src}
scratch=${BENGAL_DESK_TMP:?set an absolute owned scratch directory}
[[ $scratch = /* && -d $scratch ]] || exit 2
work=$(mktemp -d "$scratch/enhancement-binder.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
compiler=$lineage/prebuilts/rust/linux-x86/1.88.0/bin/clippy-driver
aidl=$lineage/prebuilts/build-tools/linux-x86/bin/aidl
scripts=$repo/platform/scripts
hud=$repo/platform/hardware/xyauto/hud
"$compiler" --edition=2021 --crate-name binder --crate-type rlib \
    "$scripts/desk-stubs/binder.rs" -o "$work/libbinder.rlib"
base=(--extern "binder=$work/libbinder.rlib")
for name in hud audio fm; do
    pin=$(rg -o "vendor\.xyauto\.$name-V[0-9]+-rust" "$hud/Android.bp" | sort -u)
    version=${pin##*-V}; version=${version%-rust}
    api=$repo/platform/interfaces/vendor/xyauto/$name/aidl/aidl_api/vendor.xyauto.$name/$version
    python3 "$scripts/aidl-desk-stub.py" --api-dir "$api" --out "$work/$name.rs"
    "$compiler" --edition=2021 --crate-name "vendor_xyauto_$name" --crate-type rlib \
        -L "$work" "${base[@]:0:2}" "$work/$name.rs" -o "$work/libvendor_xyauto_$name.rlib"
    base+=(--extern "vendor_xyauto_$name=$work/libvendor_xyauto_$name.rlib")
done
"$compiler" --edition=2021 --crate-name xyauto_dsp --crate-type rlib \
    "$repo/platform/hardware/xyauto/dsp/src/lib.rs" -o "$work/libxyauto_dsp.rlib"
"$compiler" --edition=2021 --crate-name xyauto_fm --crate-type rlib -L "$work" \
    --cfg 'feature="binder"' --cfg 'feature="linux-i2c"' "${base[@]}" \
    "$repo/platform/hardware/xyauto/fm/src/lib.rs" -o "$work/libxyauto_fm.rlib"
base+=(--extern "xyauto_dsp=$work/libxyauto_dsp.rlib" --extern "xyauto_fm=$work/libxyauto_fm.rlib")
for version in 2 3; do
    root=$repo/platform/interfaces/vendor/xyauto/dsp/aidl
    cfg=(--cfg 'feature="binder"' --cfg 'feature="dsp-v2"')
    if [[ $version = 2 ]]; then
        root=$root/aidl_api/vendor.xyauto.dsp/2
    else
        cfg+=(--cfg 'feature="dsp-v3"')
        # The live aidl/ directory also contains historical snapshots. Give
        # checkapi an isolated package-root copy, not that recursive mixed tree.
        mkdir -p "$work/live/vendor/xyauto"
        cp -a "$root/vendor/xyauto/dsp" "$work/live/vendor/xyauto/"
        "$aidl" --stability=vintf --checkapi=compatible \
            "$root/aidl_api/vendor.xyauto.dsp/2" "$work/live"
        root=$work/live
    fi
    python3 "$scripts/aidl-desk-stub.py" --api-dir "$root/vendor/xyauto/dsp" \
        --version "$version" --out "$work/dsp.rs"
    mkdir "$work/real-$version"
    "$aidl" --lang=rust --structured --stability=vintf --version="$version" \
        -I "$root" -o "$work/real-$version" "$root/vendor/xyauto/dsp/"*.aidl
    python3 "$scripts/aidl-desk-stub-verify.py" --stub "$work/dsp.rs" --real-dir "$work/real-$version"
    "$compiler" --edition=2021 --crate-name vendor_xyauto_dsp --crate-type rlib \
        -L "$work" --extern "binder=$work/libbinder.rlib" "$work/dsp.rs" -o "$work/libvendor_xyauto_dsp.rlib"
    "$compiler" --edition=2021 --crate-name bengal_hud --crate-type rlib -L "$work" \
        "${base[@]}" --extern "vendor_xyauto_dsp=$work/libvendor_xyauto_dsp.rlib" "${cfg[@]}" \
        -A deprecated -D warnings -A clippy::complexity -A clippy::perf -A clippy::style \
        "$hud/src/lib.rs" -o "$work/libbengal_hud_v$version.rlib"
    printf 'PASS actual HUD + Binder adapter against DSP V%s (V3 is not frozen)\n' "$version"
done
