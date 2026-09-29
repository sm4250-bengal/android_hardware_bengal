#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${TMPDIR:?set TMPDIR to an absolute private scratch directory}"
if [[ "${BENGAL_DESK_TMP:-}" != "$TMPDIR" ]]; then
  echo "HOST_TEST FAIL: BENGAL_DESK_TMP must equal TMPDIR" >&2
  exit 2
fi
bash "$root/tools/build.sh" host
binary="$TMPDIR/vulkan-offscreen-host/bengal_vulkan_offscreen_smoke"
self_log="$TMPDIR/vulkan-offscreen-host/self-test.log"
default_log="$TMPDIR/vulkan-offscreen-host/software-default-negative.log"
control_log="$TMPDIR/vulkan-offscreen-host/software-control.log"

"$binary" --self-test | tee "$self_log"
rg -F 'OUTPUT buffering=line newline_flush=1' "$self_log" >/dev/null
rg -F 'SELF_TEST verdict=PASS cases=14 classification=5 pixel_checks=2 teardown=6 padded_noncoherent_map=1' "$self_log" >/dev/null

icd=${BENGAL_SOFTWARE_VULKAN_ICD:-}
if [[ -z "$icd" ]]; then
  echo "HOST_TEST PASS self_test=1 software_vulkan=SKIP reason=BENGAL_SOFTWARE_VULKAN_ICD_unset"
  exit 0
fi
case "$icd" in
  /*) ;;
  *) echo "HOST_TEST FAIL: BENGAL_SOFTWARE_VULKAN_ICD must be absolute" >&2; exit 2 ;;
esac
[[ -f "$icd" ]]

set +e
VK_ICD_FILENAMES="$icd" timeout 20s "$binary" >"$default_log" 2>&1
default_rc=$?
set -e
if [[ "$default_rc" -ne 21 ]]; then
  sed -n '1,240p' "$default_log"
  echo "HOST_TEST FAIL: software default returned $default_rc, expected 21" >&2
  exit 1
fi
rg -F 'IDENTITY device=0 verdict=REJECTED reason=software_device' "$default_log" >/dev/null
rg -F 'RESULT HARDWARE_A610_FAIL reason=no_accepted_a610_graphics_device' "$default_log" >/dev/null

set +e
VK_ICD_FILENAMES="$icd" timeout 20s "$binary" --allow-software-control \
  >"$control_log" 2>&1
control_rc=$?
set -e
if [[ "$control_rc" -ne 3 ]]; then
  sed -n '1,300p' "$control_log"
  echo "HOST_TEST FAIL: software control returned $control_rc, expected 3" >&2
  exit 1
fi
rg -F 'CONTROL verdict=NOT_HARDWARE reason=explicit_software_control hardware_pass_forbidden=1' \
  "$control_log" >/dev/null
rg -F 'SUBMIT verdict=PASS' "$control_log" >/dev/null
rg -F 'READBACK verdict=PASS' "$control_log" >/dev/null
rg -F 'RESULT NOT_HARDWARE shader_render=PASS submit=PASS readback=PASS' \
  "$control_log" >/dev/null

echo "HOST_TEST PASS self_test=1 software_default_negative=1 software_control_render=1"
