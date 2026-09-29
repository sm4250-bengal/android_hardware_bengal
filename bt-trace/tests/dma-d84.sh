#!/usr/bin/env bash
# Host only; every output stays in the caller's assigned scratch parent.
set -euo pipefail
src=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
scratch=${BENGAL_DESK_TMP:?set assigned scratch parent}
case "$(realpath "$scratch")" in /home/meinte/android/bengal/kernel/out/.tmp/*) ;; *) exit 64 ;; esac
test -d "$scratch" && test ! -L "$scratch"
out=$(mktemp -d "$scratch/dma.XXXXXXXX")
flags=(-std=c++17 -Wall -Wextra -Werror -O1 -g -I "$src/include")
"${CXX:-c++}" "${flags[@]}" "$src/src/owner.cpp" "$src/src/posix.cpp" "$src/src/main.cpp" -o "$out/production"
"${CXX:-c++}" "${flags[@]}" "$src/src/owner.cpp" "$src/tests/dma_d84.cpp" -o "$out/dma-desk"
"${CXX:-c++}" "${flags[@]}" "$src/src/owner.cpp" "$src/tests/desk.cpp" -o "$out/legacy-desk"
cd "$out"
mkdir dma legacy
timeout 40 ./dma-desk dma 2>&1 | tee dma.log
timeout 100 ./legacy-desk legacy 2>&1 | tee legacy.log
for mode in capture capture-dma-d84; do
    for value in 0 121 -1; do
        rc=0; ./production "$mode" --seconds "$value" || rc=$?
        test "$rc" = 64
    done
    rc=0; env -u ANDROID_SOCKET_bengal_bt_trace ./production "$mode" --seconds 1 || rc=$?
    test "$rc" = 1
done
printf 'PASS production legacy/new CLI refuses invalid durations and absent init socket\n'
sha256sum production dma-desk legacy-desk dma.log legacy.log
stat -c '%s %n' production dma-desk legacy-desk dma.log legacy.log
printf 'Evidence: %s\n' "$out"
