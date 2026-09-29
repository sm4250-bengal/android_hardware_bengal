#!/bin/bash
set -euo pipefail

script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
root=$(cd "$script_dir/.." && pwd)
tmp=${BENGAL_DESK_TMP:-${TMPDIR:-}}
[[ "$tmp" = /* ]] || { echo 'BENGAL_DESK_TMP or TMPDIR must be absolute' >&2; exit 2; }
out="$tmp/vulkan-multiview-shaders"
mkdir -p "$out"

glslangValidator -V --target-env vulkan1.1 -S vert "$root/shaders/multiview.vert" -o "$out/multiview.vert.spv" >/dev/null
glslangValidator -V --target-env vulkan1.1 -S frag "$root/shaders/multiview.frag" -o "$out/multiview.frag.spv" >/dev/null
spirv-val --target-env vulkan1.1 "$out/multiview.vert.spv"
spirv-val --target-env vulkan1.1 "$out/multiview.frag.spv"

[[ "$(sha256sum "$root/shaders/multiview.vert" | awk '{print $1}')" = 60b7e4e3191cdb6ebc604668ec7ec946e51631a1dcbc83bc0209fd7809ff9894 ]]
[[ "$(sha256sum "$root/shaders/multiview.frag" | awk '{print $1}')" = 97d0c1175d9561c919539a4e199739c91fd225935aad40775d815f37f7887c2b ]]
[[ "$(sha256sum "$out/multiview.vert.spv" | awk '{print $1}')" = 11ce94bdfe01c7b9948242cec61a23dab69d9b0acc3ec35d9e08ebc6760a2a24 ]]
[[ "$(sha256sum "$out/multiview.frag.spv" | awk '{print $1}')" = 89198093b27d320e61e48c50c2c0e2c5ef1f28230ad0bdf57e6dad6d403fd3b1 ]]

python3 - "$root/generated/multiview_spv.h" "$out/multiview.vert.spv" "$out/multiview.frag.spv" <<'PY'
import re, struct, sys
header = open(sys.argv[1], encoding='utf-8').read()
for name, path in (("kMultiviewVertSpv", sys.argv[2]), ("kMultiviewFragSpv", sys.argv[3])):
    match = re.search(r"%s\[\] = \{(.*?)\};" % name, header, re.S)
    if not match:
        raise SystemExit("missing %s" % name)
    words = [int(word, 0) for word in re.findall(r"0x[0-9a-fA-F]+|\d+", match.group(1))]
    if struct.pack("<%dI" % len(words), *words) != open(path, "rb").read():
        raise SystemExit("embedded %s differs from generated SPIR-V" % name)
PY
echo 'SHADERS verdict=PASS vulkan=1.1 embedded=exact'
