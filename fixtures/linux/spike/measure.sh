#!/usr/bin/env bash
# Inside private-session.sh (one fixture running): dump the fixture's whole tree,
# then time compux's `elements` on it. M86 L0(a).
#   measure.sh TOOLKIT "WINDOW TITLE" RUNS
set -euo pipefail
toolkit=$1 window=$2 runs=${3:-20}
out=/tmp/spike-out
mkdir -p "$out"
python3 /opt/compux-spike/a11y_tree.py --app-window "$window" --out "$out/$toolkit-tree.json"
python3 /opt/compux-spike/elements_timing.py --binary /opt/compux/compux --runs "$runs" \
  --expect "$window" --out "$out/$toolkit-elements.json"
