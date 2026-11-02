#!/usr/bin/env bash
#
# Run something inside a private Linux desktop session, INSIDE the fixture
# container (Dockerfile beside this file). Nothing here can reach a person's
# desktop: the session bus is dbus-run-session's own, the X server is a private
# Xvfb, and the accessibility bus is the one that session's bus starts. Nothing
# here ever runs on the owner's live desktop.
#
#   private-session.sh --self-test          every fixture's --self-test, in turn
#   private-session.sh COMMAND [ARGS...]    start every fixture, run COMMAND, stop
#
# The fixtures are the ones COMPUX_FIXTURES names (the image sets it), each from
# COMPUX_FIXTURE_DIR, each recording into /tmp/compux-fixture-state/<toolkit>.json.
# The Qt one is started with QT_ACCESSIBILITY=1 and QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1.
# Measured on Qt 6.4.2 (Ubuntu 24.04): QT_ACCESSIBILITY=1 ALONE never registers the
# application. Qt joins the accessibility bus only when org.a11y.Status.IsEnabled
# is true (an assistive technology is running) or when ALWAYS_ON is set. A private
# session has no assistive technology, and the switch stays on the one process
# that needs it rather than on the session every fixture shares.
#
# Order matters, and is the one a desktop login has: the X server and the window
# manager, then the accessibility bus, then the applications. A GTK4 application
# started before the accessibility bus never registers on it, as agent-desktop
# found.
set -euo pipefail

DISPLAY_NUMBER=99
STATE_DIR=/tmp/compux-fixture-state
# Bounded waits: tries × 0.1 s.
X_TRIES=100
STATE_TRIES=150

die() { echo "private-session.sh: $*" >&2; exit 1; }

# A container check, not a convenience: on a desktop this would still start its
# own buses and server, but nobody should be running fixtures there at all.
[ -f /.dockerenv ] || [ -f /run/.containerenv ] || die "runs only inside the fixture container"
: "${COMPUX_FIXTURE_DIR:?set by the fixture image}"
: "${COMPUX_FIXTURES:?set by the fixture image}"

if [ -z "${COMPUX_PRIVATE_BUS:-}" ]; then
  exec dbus-run-session -- env COMPUX_PRIVATE_BUS=1 "$0" "$@"
fi

started=()
# shellcheck disable=SC2329  # invoked by the EXIT trap below
stop_started() {
  local pid
  for pid in "${started[@]}"; do kill "$pid" 2>/dev/null || true; done
  for pid in "${started[@]}"; do wait "$pid" 2>/dev/null || true; done
}
trap stop_started EXIT

wait_for_path() {
  local path=$1 tries=$2 what=$3
  for _ in $(seq "$tries"); do
    [ -e "$path" ] && return 0
    sleep 0.1
  done
  die "$what did not appear at $path"
}

start_display() {
  XDG_RUNTIME_DIR=$(mktemp -d /tmp/runtime-XXXXXX)
  export XDG_RUNTIME_DIR
  Xvfb ":$DISPLAY_NUMBER" -screen 0 1280x1024x24 -nolisten tcp >/tmp/xvfb.log 2>&1 &
  started+=("$!")
  wait_for_path "/tmp/.X11-unix/X$DISPLAY_NUMBER" "$X_TRIES" "the Xvfb socket"
  export DISPLAY=":$DISPLAY_NUMBER"
  openbox --sm-disable >/tmp/openbox.log 2>&1 &
  started+=("$!")
}

# Asking the session bus for the accessibility bus's address starts it (bus
# activation), exactly as an assistive technology's first call does on a desktop.
start_accessibility_bus() {
  busctl --user call org.a11y.Bus /org/a11y/bus org.a11y.Bus GetAddress >/dev/null \
    || die "the session bus could not start the accessibility bus"
}

# Called inside the subshell that becomes the fixture, so the setting reaches
# that one process and no other.
fixture_env() {
  if [ "$1" = qt ]; then export QT_ACCESSIBILITY=1 QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1; fi
}

run_self_tests() {
  local toolkit failed=()
  for toolkit in $COMPUX_FIXTURES; do
    echo "== $toolkit self-test"
    (fixture_env "$toolkit"; exec python3 "$COMPUX_FIXTURE_DIR/fixture_$toolkit.py" \
      --self-test --state-file "$STATE_DIR/$toolkit-self-test.json") || failed+=("$toolkit")
  done
  [ "${#failed[@]}" -eq 0 ] || die "self-test failed: ${failed[*]}"
  echo "all self-tests passed: $COMPUX_FIXTURES"
}

start_fixtures() {
  local toolkit
  for toolkit in $COMPUX_FIXTURES; do
    (fixture_env "$toolkit"; exec python3 "$COMPUX_FIXTURE_DIR/fixture_$toolkit.py" \
      --state-file "$STATE_DIR/$toolkit.json") >"/tmp/fixture-$toolkit.log" 2>&1 &
    started+=("$!")
  done
  # Each fixture writes its (empty) state file as it starts, before its window.
  for toolkit in $COMPUX_FIXTURES; do
    wait_for_path "$STATE_DIR/$toolkit.json" "$STATE_TRIES" "the $toolkit fixture's state file"
  done
}

[ "$#" -gt 0 ] || die "usage: private-session.sh --self-test | COMMAND [ARGS...]"
mkdir -p "$STATE_DIR"
start_display
start_accessibility_bus

if [ "$1" = --self-test ]; then
  run_self_tests
  exit 0
fi

start_fixtures
status=0
"$@" || status=$?
exit "$status"
