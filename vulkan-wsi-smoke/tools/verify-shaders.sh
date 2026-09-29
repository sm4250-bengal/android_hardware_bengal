#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
: "${TMPDIR:?set TMPDIR to an absolute private scratch directory}"
case "$TMPDIR" in
  /*) ;;
  *) echo "VERIFY_SHADERS FAIL: TMPDIR must be absolute" >&2; exit 2 ;;
esac
if [[ "${BENGAL_DESK_TMP:-}" != "$TMPDIR" ]]; then
  echo "VERIFY_SHADERS FAIL: BENGAL_DESK_TMP must equal TMPDIR" >&2
  exit 2
fi

out="$TMPDIR/vulkan-wsi-shaders"
mkdir -p "$out"
glslangValidator --target-env vulkan1.0 -V -S vert \
  -o "$out/grid.vert.spv" "$root/shaders/grid.vert"
glslangValidator --target-env vulkan1.0 -V -S frag \
  -o "$out/grid.frag.spv" "$root/shaders/grid.frag"

want_vert_source=db11f8a0f7c4cf3d8397592be272426ecd631278ff4ffabed08ad2881af1e896
want_vert_spv=d166ec1db40275f036c0b6081f3fca45155d74b079e79ad2bac1a44eba7a7790
want_frag_source=f89fa47d72ff536d97db9fb655cc2a231b3ad34f0ac5c9f16aabfd55cd8de322
want_frag_spv=d069bf46fa37c6027388ffd7dcdc6a9d98687c99e405e6a062903ae7137a560e
got_vert_source=$(sha256sum "$root/shaders/grid.vert" | awk '{print $1}')
got_vert_spv=$(sha256sum "$out/grid.vert.spv" | awk '{print $1}')
got_frag_source=$(sha256sum "$root/shaders/grid.frag" | awk '{print $1}')
got_frag_spv=$(sha256sum "$out/grid.frag.spv" | awk '{print $1}')
[[ "$got_vert_source" == "$want_vert_source" ]]
[[ "$got_vert_spv" == "$want_vert_spv" ]]
[[ "$got_frag_source" == "$want_frag_source" ]]
[[ "$got_frag_spv" == "$want_frag_spv" ]]

python3 - "$root/generated/grid_spv.h" \
  "$out/grid.vert.spv" "$out/grid.frag.spv" <<'PY'
import pathlib
import re
import struct
import sys

header = pathlib.Path(sys.argv[1]).read_text()
for symbol, binary_path in (
    ("kGridVertSpv", pathlib.Path(sys.argv[2])),
    ("kGridFragSpv", pathlib.Path(sys.argv[3])),
):
    match = re.search(
        rf"inline constexpr uint32_t {symbol}\[\] = \{{(.*?)\n\}};",
        header,
        re.DOTALL,
    )
    if match is None:
        raise SystemExit(f"VERIFY_SHADERS FAIL: missing {symbol}")
    words = [int(word, 16) for word in
             re.findall(r"0x[0-9a-fA-F]+", match.group(1))]
    embedded = b"".join(struct.pack("<I", word) for word in words)
    if embedded != binary_path.read_bytes():
        raise SystemExit(f"VERIFY_SHADERS FAIL: {symbol} byte mismatch")
PY

if command -v spirv-val >/dev/null 2>&1; then
  spirv-val --target-env vulkan1.0 "$out/grid.vert.spv"
  spirv-val --target-env vulkan1.0 "$out/grid.frag.spv"
fi
echo "VERIFY_SHADERS PASS target=vulkan1.0 source=2 spirv=2"
echo "SHADER_SHA vert_source=$got_vert_source vert_spv=$got_vert_spv"
echo "SHADER_SHA frag_source=$got_frag_source frag_spv=$got_frag_spv"
