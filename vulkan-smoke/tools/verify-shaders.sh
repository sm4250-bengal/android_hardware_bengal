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

out="$TMPDIR/vulkan-offscreen-shaders"
mkdir -p "$out"
glslangValidator --target-env vulkan1.0 -V -S vert \
  -o "$out/triangle.vert.spv" "$root/shaders/triangle.vert"
glslangValidator --target-env vulkan1.0 -V -S frag \
  -o "$out/triangle.frag.spv" "$root/shaders/triangle.frag"

want_vert_source=b1c540057c8cc583c07acb817113b64800442d9770a9e94e59a66ded3dbefdbf
want_vert_spv=f9a673fbdc72b4dc1cf903f63810f6a6dccf02813ae33781f1925e2f957b1210
want_frag_source=6d80f9e509851f392620f337583e0372b793a5f738086058ea7a304da29e3deb
want_frag_spv=38b54017bc1443198bce5d8e76a7cc525777db877053c8aaaecdcbcb099b1e64
got_vert_source=$(sha256sum "$root/shaders/triangle.vert" | awk '{print $1}')
got_vert_spv=$(sha256sum "$out/triangle.vert.spv" | awk '{print $1}')
got_frag_source=$(sha256sum "$root/shaders/triangle.frag" | awk '{print $1}')
got_frag_spv=$(sha256sum "$out/triangle.frag.spv" | awk '{print $1}')

[[ "$got_vert_source" == "$want_vert_source" ]]
[[ "$got_vert_spv" == "$want_vert_spv" ]]
[[ "$got_frag_source" == "$want_frag_source" ]]
[[ "$got_frag_spv" == "$want_frag_spv" ]]

python3 - "$root/generated/triangle_spv.h" \
  "$out/triangle.vert.spv" "$out/triangle.frag.spv" <<'PY'
import pathlib
import re
import struct
import sys

header = pathlib.Path(sys.argv[1]).read_text()
for symbol, binary_path in (
    ("kTriangleVertSpv", pathlib.Path(sys.argv[2])),
    ("kTriangleFragSpv", pathlib.Path(sys.argv[3])),
):
    match = re.search(
        rf"static constexpr uint32_t {symbol}\[\] = \{{(.*?)\n\}};",
        header,
        re.DOTALL,
    )
    if match is None:
        raise SystemExit(f"VERIFY_SHADERS FAIL: missing {symbol}")
    words = [int(word, 16) for word in re.findall(r"0x[0-9a-fA-F]+", match.group(1))]
    embedded = b"".join(struct.pack("<I", word) for word in words)
    compiled = binary_path.read_bytes()
    if embedded != compiled:
        raise SystemExit(f"VERIFY_SHADERS FAIL: {symbol} differs from compiled SPIR-V")
PY

if command -v spirv-val >/dev/null 2>&1; then
  spirv-val --target-env vulkan1.0 "$out/triangle.vert.spv"
  spirv-val --target-env vulkan1.0 "$out/triangle.frag.spv"
fi

echo "VERIFY_SHADERS PASS source=2 spirv=2 target=vulkan1.0"
echo "SHADER_SHA vert_source=$got_vert_source vert_spv=$got_vert_spv"
echo "SHADER_SHA frag_source=$got_frag_source frag_spv=$got_frag_spv"
