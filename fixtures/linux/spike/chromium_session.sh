#!/usr/bin/env bash
# Inside the Chromium spike image only (chromium.Dockerfile): a private session (Xvfb,
# openbox, its own session bus and accessibility bus) with Debian's Chromium showing
# chromium_page.html; dumps the page's tree, then times compux's `elements` on it.
# M86 L0(a), Chromium.
#   chromium_session.sh RUNS
#
# Chromium runs with --no-sandbox: its sandbox needs user namespaces, which the
# container's seccomp profile refuses, and this container has no network and shows
# one local page. ACCESSIBILITY_ENABLED=1 and --force-renderer-accessibility make
# the browser and the page publish their trees without waiting for an assistive
# technology to be noticed (the same switch Fermix is to give the apps it launches).
set -euo pipefail
[ -f /.dockerenv ] || [ -f /run/.containerenv ] || { echo "runs only inside the spike container" >&2; exit 2; }
if [ -z "${COMPUX_PRIVATE_BUS:-}" ]; then
  exec dbus-run-session -- env COMPUX_PRIVATE_BUS=1 "$0" "$@"
fi
runs=${1:-20}
out=/tmp/spike-out
mkdir -p "$out"
DISPLAY_NUMBER=99
# Bounded wait: tries x 0.1 s.
X_TRIES=100

XDG_RUNTIME_DIR=$(mktemp -d /tmp/runtime-XXXXXX)
export XDG_RUNTIME_DIR
started=()
# shellcheck disable=SC2329  # invoked by the EXIT trap below
stop_started() {
  local pid
  for pid in "${started[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${started[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap stop_started EXIT

Xvfb ":$DISPLAY_NUMBER" -screen 0 1280x1024x24 -nolisten tcp >"$out/chromium-xvfb.log" 2>&1 &
started+=("$!")
for _ in $(seq "$X_TRIES"); do [ -e "/tmp/.X11-unix/X$DISPLAY_NUMBER" ] && break; sleep 0.1; done
[ -e "/tmp/.X11-unix/X$DISPLAY_NUMBER" ] || { echo "no Xvfb socket" >&2; exit 1; }
export DISPLAY=":$DISPLAY_NUMBER"
openbox --sm-disable >/dev/null 2>&1 &
started+=("$!")
# The accessibility bus before the browser, as on a desktop login (asking for its
# address starts it). This image has no busctl; GLib asks instead.
python3 -c 'from gi.repository import Gio; Gio.bus_get_sync(Gio.BusType.SESSION).call_sync(
    "org.a11y.Bus", "/org/a11y/bus", "org.a11y.Bus", "GetAddress", None, None,
    Gio.DBusCallFlags.NONE, 5000, None)'

ACCESSIBILITY_ENABLED=1 chromium --no-sandbox --force-renderer-accessibility --no-first-run --no-default-browser-check \
  --disable-gpu --password-store=basic --user-data-dir=/tmp/chromium-profile \
  --window-size=1000,800 --app=file:///opt/compux-spike/chromium_page.html \
  >"$out/chromium-browser.log" 2>&1 &
started+=("$!")

python3 /opt/compux-spike/a11y_tree.py --app-window "Compux Fixture Chromium" \
  --out "$out/chromium-tree.json"
python3 /opt/compux-spike/elements_timing.py --binary /opt/compux/compux --runs "$runs" \
  --expect "Compux Fixture Chromium" --out "$out/chromium-elements.json"
