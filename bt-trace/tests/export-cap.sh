#!/usr/bin/env bash
# Host-only actual owner + POSIX export contract, one compiler at a time.
set -euo pipefail
src=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
scratch=${BENGAL_DESK_TMP:?set assigned scratch parent}
case "$(realpath "$scratch")" in /home/meinte/android/bengal/kernel/out/.tmp/*) ;; *) exit 64 ;; esac
test -d "$scratch" && test ! -L "$scratch"
out=$(mktemp -d "$scratch/export.XXXXXXXX")
"${CXX:-c++}" -std=c++17 -Wall -Wextra -Werror -O1 -g -I "$src/include" \
    "$src/src/owner.cpp" "$src/tests/export_cap.cpp" -o "$out/export-cap"
cd "$out"
mkdir cases
timeout 25 ./export-cap cases 2>&1 | tee export.log
sha256sum export-cap export.log
stat -c '%s %n' export-cap export.log
printf 'Evidence: %s\n' "$out"
