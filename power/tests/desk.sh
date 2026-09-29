#!/bin/bash
set -euo pipefail
task_root=$(cd -- "$(dirname -- "$0")/../../../../.." && pwd)
scratch=${1:?usage: desk.sh ABSOLUTE_OWN_SCRATCH}
[[ $scratch = /* ]] || exit 64
mkdir -p "$scratch"
export TMPDIR="$scratch"
artifact=$(mktemp -d "$scratch/desk.XXXXXXXX")
g++ -std=c++17 -Wall -Wextra -Werror -pthread -O1 -g \
    -I"$task_root/platform/hardware/xyauto/power" \
    "$task_root/platform/hardware/xyauto/power/Boost.cpp" \
    "$task_root/platform/hardware/xyauto/power/tests/async.cpp" -o "$artifact/async"
"$artifact/async" | tee "$artifact/async-result.txt"
g++ -std=c++17 -Wall -Wextra -Werror -pthread -O1 -g \
    -I"$task_root/platform/hardware/xyauto/power" \
    -I"$task_root/platform/hardware/xyauto/thermal" \
    "$task_root/platform/hardware/xyauto/power/Boost.cpp" \
    "$task_root/platform/hardware/xyauto/power/SysfsIo.cpp" \
    "$task_root/platform/hardware/xyauto/thermal/ThermalSensors.cpp" \
    "$task_root/platform/hardware/xyauto/power/tests/desk.cpp" -o "$artifact/desk"
g++ -std=c++17 -Wall -Wextra -Werror -pthread -c \
    "$task_root/platform/hardware/xyauto/power/SysfsIo.cpp" -o "$artifact/sysfs.o"
"$artifact/desk" "$artifact" | tee "$artifact/result.txt"
printf '%s\n' "$artifact"
