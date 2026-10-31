#!/usr/bin/env bash
# Inside the Wayland spike image only (wayland.Dockerfile): a private GNOME Shell,
# headless, Wayland with no X11, on its own session bus and accessibility bus; the
# GTK4 fixture as a Wayland client. Dumps the fixture's tree, then asks compux for
# `elements`, which needs X11 geometry today: its answer is recorded either way.
# M86 L0(a), Wayland.
#   wayland_session.sh RUNS
set -euo pipefail
[ -f /.dockerenv ] || [ -f /run/.containerenv ] || { echo "runs only inside the spike container" >&2; exit 2; }
if [ -z "${COMPUX_PRIVATE_BUS:-}" ]; then
  exec dbus-run-session -- env COMPUX_PRIVATE_BUS=1 "$0" "$@"
fi
runs=${1:-20}
out=/tmp/spike-out
mkdir -p "$out" /tmp/compux-fixture-state
# Bounded waits: tries x 0.1 s.
SOCKET_TRIES=300
STATE_TRIES=300

XDG_RUNTIME_DIR=$(mktemp -d /tmp/rt-XXXXXX)
export XDG_RUNTIME_DIR
unset DISPLAY
started=()
# shellcheck disable=SC2329  # invoked by the EXIT trap below
stop_started() {
  local pid
  for pid in "${started[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${started[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap stop_started EXIT

wait_for() {
  local test=$1 path=$2 tries=$3
  for _ in $(seq "$tries"); do
    test "$test" "$path" && return 0
    sleep 0.1
  done
  echo "wayland_session.sh: $path did not appear" >&2
  return 1
}

# GNOME Shell 46 aborts at start without a system bus to look for logind on. This
# one is the container's own, private like the session bus, and has no logind.
dbus-daemon --session --nofork --nopidfile \
  --address="unix:path=$XDG_RUNTIME_DIR/system_bus_socket" >/dev/null 2>&1 &
started+=("$!")
wait_for -S "$XDG_RUNTIME_DIR/system_bus_socket" "$SOCKET_TRIES"
export DBUS_SYSTEM_BUS_ADDRESS="unix:path=$XDG_RUNTIME_DIR/system_bus_socket"

gnome-shell --headless --wayland --no-x11 --virtual-monitor 1280x1024 \
  --wayland-display wayland-spike >"$out/gtk4-wayland-shell.log" 2>&1 &
started+=("$!")
wait_for -S "$XDG_RUNTIME_DIR/wayland-spike" "$SOCKET_TRIES"
export WAYLAND_DISPLAY=wayland-spike
busctl --user call org.a11y.Bus /org/a11y/bus org.a11y.Bus GetAddress >/dev/null

GDK_BACKEND=wayland python3 /opt/compux-fixtures/fixture_gtk4.py \
  --state-file /tmp/compux-fixture-state/gtk4.json >"$out/gtk4-wayland-fixture.log" 2>&1 &
started+=("$!")
wait_for -e /tmp/compux-fixture-state/gtk4.json "$STATE_TRIES"

python3 /opt/compux-spike/a11y_tree.py --app-window "Compux Fixture GTK4" \
  --out "$out/gtk4-wayland-tree.json"
status=0
python3 /opt/compux-spike/elements_timing.py --binary /opt/compux/compux --runs "$runs" \
  --expect "Compux Fixture GTK4" --out "$out/gtk4-wayland-elements.json" \
  2>"$out/gtk4-wayland-elements.err" || status=$?
echo "elements_timing exit: $status" | tee -a "$out/gtk4-wayland-elements.err"
