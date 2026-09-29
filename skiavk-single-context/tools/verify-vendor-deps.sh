#!/bin/bash
set -euo pipefail
[[ $# == 4 && $1 == /* && $2 == /* && $3 == /* && $4 == /* ]] || {
    echo "Usage: $0 ABS_ELF ABS_CAPTURED_LD_CONFIG ABS_NDK_LIBANDROID_STUB ABS_FRESH_OUTPUT" >&2
    exit 64
}
elf=$1
config=$2
android=$3
out=$4
[[ -f $elf && -f $config && -f $android && ! -e $out ]] || {
    echo "Require existing inputs and fresh output" >&2; exit 64;
}
mkdir -p -- "$out"
readelf -W -h -l -d --dyn-syms "$elf" > "$out/elf.txt"
rg -q 'Class: +ELF64' "$out/elf.txt"
rg -q 'Machine: +AArch64' "$out/elf.txt"
rg -q 'Type: +DYN' "$out/elf.txt"
rg -q 'Requesting program interpreter: /system/bin/linker64' "$out/elf.txt"
awk '/\(NEEDED\)/ { line=$0; sub(/^.*\[/,"",line); sub(/\].*$/,"",line); print line }' \
    "$out/elf.txt" | LC_ALL=C sort > "$out/needed.txt"
expected=$'libc.so\nlibdl.so\nliblog.so\nlibm.so\nlibvulkan.so'
[[ $(<"$out/needed.txt") == "$expected" ]] || {
    echo "FAIL: dependencies are not the five public offscreen libraries" >&2; exit 1;
}
# Examine only the actual captured [vendor] section, never an unrelated namespace's allowlist.
awk '/^\[vendor\]$/ { vendor=1; next }
     /^\[/ { vendor=0 }
     vendor && /^namespace.default.link.system.shared_libs = / {
         sub(/^[^=]*= */, ""); print
     }' "$config" | tr ':' '\n' | LC_ALL=C sort -u > "$out/vendor-system-allowed.txt"
[[ -s $out/vendor-system-allowed.txt ]]
LC_ALL=C comm -23 "$out/needed.txt" "$out/vendor-system-allowed.txt" > "$out/not-allowed.txt"
[[ ! -s $out/not-allowed.txt ]] || {
    echo "FAIL: dependency absent from captured vendor-to-system allowlist" >&2; exit 1;
}
awk '$7 == "UND" && ($5 == "GLOBAL" || $5 == "WEAK") {
         sub(/@.*/, "", $8); print $8
     }' "$out/elf.txt" | LC_ALL=C sort -u > "$out/imports.txt"
readelf -W -d --dyn-syms "$android" > "$out/libandroid-symbols.txt"
rg -q 'Library soname: \[libandroid.so\]' "$out/libandroid-symbols.txt"
awk '$7 != "UND" && ($5 == "GLOBAL" || $5 == "WEAK") {
         sub(/@.*/, "", $8); print $8
     }' "$out/libandroid-symbols.txt" | LC_ALL=C sort -u > "$out/libandroid-exports.txt"
rg -qx 'AHardwareBuffer_allocate' "$out/libandroid-exports.txt"
LC_ALL=C comm -12 "$out/imports.txt" "$out/libandroid-exports.txt" > "$out/android-imports.txt"
[[ ! -s $out/android-imports.txt ]] || {
    echo "FAIL: executable imports an NDK libandroid export" >&2; exit 1;
}
sha256sum "$elf" "$config" "$android" > "$out/input-sha256.txt"
echo "PASS AArch64 PIE: exact five dependencies allowed by captured vendor namespace; no libandroid imports" \
    | tee "$out/results.txt"
