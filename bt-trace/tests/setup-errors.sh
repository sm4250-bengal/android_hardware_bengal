#!/usr/bin/env bash
# SPDX-License-Identifier: Apache-2.0
# Focused host-only error evidence. No tracefs, device, Android build or cleanup.
set -euo pipefail
src=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
scratch=${BENGAL_DESK_TMP:?set assigned existing absolute scratch parent}
case "$(realpath "$scratch")" in /home/meinte/android/bengal/kernel/out/.tmp/*) ;; *) exit 64 ;; esac
test -d "$scratch" && test ! -L "$scratch"
out=$(mktemp -d "$scratch/errors.XXXXXXXX")
flags=(-std=c++17 -Wall -Wextra -Werror -O1 -g -I "$src/include")
"${CXX:-c++}" "${flags[@]}" "$src/src/owner.cpp" "$src/src/posix.cpp" "$src/src/main.cpp" -o "$out/production"
"${CXX:-c++}" "${flags[@]}" "$src/src/owner.cpp" "$src/tests/desk.cpp" -o "$out/desk"
"${CXX:-c++}" "${flags[@]}" "$src/src/owner.cpp" "$src/tests/posix_errors.cpp" -o "$out/posix-errors"
cd "$out"
# Relative socket addresses avoid sockaddr_un's 108-byte path limit without
# placing fixtures outside the assigned scratch tree.
mkdir diagnostics posix rounding
timeout 15 ./desk rounding rounding | tee rounding.log
timeout 30 ./desk diagnostics diagnostics | tee diagnostics.log
./posix-errors posix | tee posix.log
for args in 'capture --seconds 0' 'capture --seconds 121' 'wait --budget-ms 1' 'capture --test-root /tmp'; do
    read -r -a argv <<< "$args"
    rc=0; ./production "${argv[@]}" || rc=$?
    test "$rc" = 64
done
rc=0; env -u ANDROID_SOCKET_bengal_bt_trace ./production capture --seconds 1 || rc=$?
test "$rc" = 1
printf 'PASS unchanged CLI refusals and absent init socket\n'
sha256sum production desk posix-errors rounding.log diagnostics.log posix.log
stat -c '%s %n' production desk posix-errors rounding.log diagnostics.log posix.log
printf 'Evidence: %s\n' "$out"
