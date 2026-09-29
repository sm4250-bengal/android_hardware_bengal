#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${TMPDIR:?set TMPDIR to an absolute private scratch directory}"
case "$TMPDIR" in
  /*) ;;
  *) echo "TEST FAIL: TMPDIR must be absolute" >&2; exit 2 ;;
esac
if [[ "${BENGAL_DESK_TMP:-}" != "$TMPDIR" ]]; then
  echo "TEST FAIL: BENGAL_DESK_TMP must equal TMPDIR" >&2
  exit 2
fi
bash "$root/tools/build.sh" host
"$TMPDIR/vulkan-wsi-host/logic_test"
"$TMPDIR/vulkan-wsi-host/frame_loop_test"
echo "HOST_TEST PASS actualFrameLoopControls=run deviceVulkan=not_run WSI=not_run hostGPU=not_touched"
