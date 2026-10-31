#!/usr/bin/env bash
# Inside private-session.sh on Ubuntu 22.04 (ATK bridge 2.38), with the GTK3 fixture
# running: record every message on the PRIVATE accessibility bus while compux reads
# the fixture, then show that the fixture survived and that no `GetActions` and no
# `GetAll` was ever sent. M86 L0(b).
#   atk_bridge_check.sh RUNS
set -euo pipefail
runs=${1:-20}
out=/tmp/spike-out
mkdir -p "$out"
log=$out/gtk3-a11y-bus.log

address=$(busctl --user call org.a11y.Bus /org/a11y/bus org.a11y.Bus GetAddress | sed -e 's/^s "//' -e 's/"$//')
[ -n "$address" ] || { echo "no accessibility bus address" >&2; exit 1; }
dbus-monitor --address "$address" >"$log" 2>&1 &
monitor=$!
trap 'kill "$monitor" 2>/dev/null || true; wait "$monitor" 2>/dev/null || true' EXIT
sleep 1

# No pipe: under pipefail, `tr | grep -q` can end in SIGPIPE and read as no match.
# The glob is expanded before any grep starts, so no grep finds itself.
fixture_pid() {
  local proc
  for proc in /proc/[0-9]*; do
    if grep -qa 'fixture_gtk3.py' "$proc/cmdline" 2>/dev/null; then
      basename "$proc"; return 0
    fi
  done
  return 1
}
before=$(fixture_pid) || { echo "the GTK3 fixture is not running" >&2; exit 1; }

python3 /opt/compux-spike/elements_timing.py --binary /opt/compux/compux --runs "$runs" \
  --expect "Compux Fixture GTK3" --out "$out/gtk3-jammy-elements.json"
sleep 1
kill "$monitor"; wait "$monitor" 2>/dev/null || true
trap - EXIT

after=$(fixture_pid || echo none)
# Counts read with `|| true`: a count of zero is the answer, not a failure.
getactions=$(grep -c 'member=GetActions' "$log" || true)
getall=$(grep -c 'member=GetAll' "$log" || true)
getname=$(grep -c 'interface=org.a11y.atspi.Action; member=GetName' "$log" || true)
nactions=$(grep -A2 'member=Get$' "$log" | grep -c '"NActions"' || true)
calls=$(grep -c '^method call' "$log" || true)
{
  echo "fixture pid before: $before"
  echo "fixture pid after:  $after"
  echo "method calls on the bus: $calls"
  echo "Action.GetActions: $getactions"
  echo "Properties.GetAll: $getall"
  echo "Action.GetName: $getname"
  echo "Get(NActions): $nactions"
} | tee "$out/gtk3-jammy-verdict.txt"
if [ "$after" != "$before" ]; then
  echo "VERDICT: the fixture did not survive"; exit 1
fi
if [ "$getactions" -ne 0 ]; then
  echo "VERDICT: Action.GetActions was sent"; exit 1
fi
if [ "$getall" -ne 0 ]; then
  echo "VERDICT: Properties.GetAll was sent"; exit 1
fi
# The positive control: a walk that never named an action would pass the two
# checks above without ever reaching the code that could have sent GetActions.
if [ "$getname" -eq 0 ]; then
  echo "VERDICT: no action was ever named, so the check proves nothing"; exit 1
fi
echo "VERDICT: survived, no GetActions, no GetAll, $getname actions named by index"
