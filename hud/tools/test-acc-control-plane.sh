#!/bin/sh
# SPDX-License-Identifier: Apache-2.0
set -eu

hud=${1:?usage: test-acc-control-plane.sh <hud-root>}
checker="$hud/tools/check-acc-control-plane.py"
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT

python3 "$checker" "$hud"

mutate_and_require_fail() {
    name=$1
    from=$2
    to=$3
    copy="$work/$name"
    cp -a "$hud" "$copy"
    python3 - "$copy" "$from" "$to" <<'PY'
import pathlib, sys
root, old, new = map(pathlib.Path, sys.argv[1:])
for path in root.rglob("*.rs"):
    text = path.read_text()
    if old.as_posix() in text:
        path.write_text(text.replace(old.as_posix(), new.as_posix(), 1))
        break
else:
    raise SystemExit("mutation anchor missing")
PY
    if python3 "$copy/tools/check-acc-control-plane.py" "$copy"; then
        echo "ACC control mutation unexpectedly passed: $name" >&2
        exit 1
    fi
}

mutate_and_require_fail no-machine 'machine: Machine,' 'machine: MissingMachine,'
mutate_and_require_fail no-writer 'self.io.write_all' 'self.io.write_broken'
mutate_and_require_fail wrong-delay 'FINAL_STAGE_DELAY_MS: u64 = 500' 'FINAL_STAGE_DELAY_MS: u64 = 499'
mutate_and_require_fail duplicate-service-machine 'acc_state: AccState' 'machine: Machine, acc_state: AccState'
mutate_and_require_fail no-sleep-seam 'self.control.push(AccControl::SleepRequest' 'self.control.push(AccControl::SleepRequestBroken'

echo "ACC control mutations PASS: 5 negative controls"
