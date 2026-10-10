#!/usr/bin/env bash
# Run INSIDE run-isolated.sh. Logs are retained in repository-local lab/results.
set -euo pipefail
mkdir -p lab/results
"${NIRI_RDP_LAB_NIRI:-compositor/upstream/target/debug/niri}" --config lab/headless.kdl >lab/results/niri.log 2>&1 &
niri_pid=$!
app_pid=
trap '[[ -z "$app_pid" ]] || kill "$app_pid" 2>/dev/null || :; kill "$niri_pid" 2>/dev/null || :; wait || :' EXIT
for ((i=0; i<100; i++)); do
    [[ ! -S "$XDG_RUNTIME_DIR/wayland-0" ]] || break
    kill -0 "$niri_pid"
    sleep .05
done
export WAYLAND_DISPLAY=wayland-0
lab/dynamic-app >lab/results/app.log 2>&1 &
app_pid=$!
sleep 1
bridge/target/debug/capture-probe "$XDG_RUNTIME_DIR/wayland-0" HEADLESS-1 | tee lab/results/capture.log
sleep .2
python - <<'PY'
from pathlib import Path
s=Path('lab/results/app.log').read_text()
for expected in ['KEY 30 1','KEY 30 0','KEY 97 1','KEY 97 0','BUTTON 272 1','BUTTON 272 0']:
    assert expected in s, (expected, s)
print('PASS actual app observed virtual A/right-Control press/release and left-button press/release')
PY
