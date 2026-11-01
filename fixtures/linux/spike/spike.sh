#!/usr/bin/env bash
#
# M86 L0 spikes (a) and (b), reproducibly, in containers only. Run from the
# repository root on any Linux host with Docker:
#
#   fixtures/linux/spike/spike.sh OUT_DIR [RUNS]
#
# 1. Builds the fixture images if they are missing (fixtures/linux/Dockerfile, the
#    noble and jammy variants its header names).
# 2. Builds the compux helper in an Ubuntu 22.04 container (build.Dockerfile), so
#    one binary runs in both images. The repository is copied in, and the cargo
#    caches are named volumes ($SPIKE_VOLUMES-registry, -target below): nothing
#    is mounted from the host.
# 3. Builds the spike images (spike.Dockerfile): fixture image + helper + probes.
# 4. L0(a): for each toolkit in noble, ONE fixture in a private session
#    (private-session.sh: Xvfb, openbox, its own session and accessibility bus):
#    the whole tree dumped (a11y_tree.py), then `elements` timed RUNS times
#    (elements_timing.py).
# 5. L0(b): the GTK3 fixture in jammy (ATK bridge 2.38), `elements` RUNS times while
#    dbus-monitor records the private accessibility bus (atk_bridge_check.sh).
# 6. wayland: the GTK4 fixture under a private headless GNOME Shell, Wayland with no
#    X11 (wayland.Dockerfile, wayland_session.sh): its tree, and what `elements` says.
# 7. chromium: Debian's Chromium (no snap) showing chromium_page.html in a private
#    session (chromium.Dockerfile, chromium_session.sh): its tree and `elements` timed.
#
# 8. verbs (M86 L1): every verb compux answers on Linux, on each fixture in noble,
#    judged by what the fixture recorded (verbs.py); verbs-jammy: the same on GTK3 in
#    jammy (ATK bridge 2.38), under the same bus monitor as L0(b), so its writes are
#    counted and no GetActions and no GetAll may be among anything sent.
# 9. pixels (M86 L2): the X11 pixel route on the GTK3 fixture in noble (pixels.py):
#    XTEST clicks and keys judged by what the fixture recorded, a paste through the
#    selection compux owns, the frame against a pattern tiled on the root, a refused
#    character off the layout, and a button a killed helper held released by the
#    next one, judged by the X server (x11_oracle.py).
# 10. bound (M86 L3): one window bound on X11 and its badge, on the GTK3 fixture in
#    noble (bound.py): none of a covering window's pixels in the first frame, frame
#    and title bar included, a covered window captured, a covered point refused,
#    the badge's buttons reaching the gate as the person clicks them and not when
#    another program sends the click, a click off them passing through, a reused
#    window id refused, a window that never repaints refused, twenty select and
#    release cycles leaving no process behind, and, on a second X server with no
#    window manager, a window with an X border read from inside it. The badge (compux-indicator-x11) is built
#    beside the helper and installed beside it, as the release archive carries it.
#
# The helper is built with no C library but the C runtime (build.Dockerfile has no
# X, XKB, xdo or D-Bus development package); no_c_deps.sh proves that on its own.
#
# SPIKE_STAGES picks the stages (default "a b wayland chromium verbs verbs-jammy pixels bound"). Docker is whatever
# DOCKER (default `docker`) and DOCKER_HOST name. CHROMIUM_BASE is the Debian
# bookworm image the Chromium one starts from (default debian:bookworm; any bookworm
# image with apt works). Every image is built from images already present when the
# daemon cannot reach a registry: run with DOCKER_BUILDKIT=0 then, since BuildKit
# asks the registry about a base image even when it has it.
#
# SPIKE_IMAGE names the spike images (default compux-spike, tagged noble, jammy, ...)
# and SPIKE_VOLUMES the cargo cache volumes (default compux-spike: -registry,
# -target), so two checkouts can run the spike side by side without building into
# each other's images or target directory.
#
# Every session container runs with --network none and mounts nothing from the
# host: the results are copied out with `docker cp`. Every container is waited for
# with a bound (BUILD_LIMIT_S, RUN_LIMIT_S). Nothing here talks to the host's
# display, session bus or accessibility bus.
set -euo pipefail

OUT=${1:?usage: spike.sh OUT_DIR [RUNS]}
RUNS=${2:-20}
DOCKER=${DOCKER:-docker}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../../.." && pwd)
CACHE=${SPIKE_CACHE:-$HOME/.cache/compux-l0-spike}
BUILD_LIMIT_S=${BUILD_LIMIT_S:-3600}
RUN_LIMIT_S=${RUN_LIMIT_S:-600}
CHROMIUM_BASE=${CHROMIUM_BASE:-debian:bookworm}
STAGES=${SPIKE_STAGES:-a b wayland chromium verbs verbs-jammy pixels bound}
IMAGE=${SPIKE_IMAGE:-compux-spike}
VOLUMES=${SPIKE_VOLUMES:-compux-spike}
mkdir -p "$OUT" "$CACHE"

die() { echo "spike.sh: $*" >&2; exit 1; }

image_missing() { ! "$DOCKER" image inspect "$1" >/dev/null 2>&1; }

build() { timeout -k 10 "$BUILD_LIMIT_S" "$DOCKER" build "$@"; }

fixture_images() {
  if image_missing compux-fixtures:noble; then
    build -t compux-fixtures:noble "$REPO/fixtures/linux"
  fi
  if image_missing compux-fixtures:jammy; then
    build -t compux-fixtures:jammy --build-arg BASE=ubuntu:22.04 \
      --build-arg QT_PACKAGES= --build-arg FIXTURES="gtk3 gtk4" "$REPO/fixtures/linux"
  fi
}

# The helper and its X11 badge, built from a copy of the tracked files and the
# working tree.
build_helper() {
  local context c
  build -q -t "$IMAGE-build:jammy" -f "$HERE/build.Dockerfile" "$HERE" >/dev/null
  context=$(mktemp -d)
  (cd "$REPO" && git ls-files -coz --exclude-standard | rsync -rl --from0 --files-from=- ./ "$context/")
  c=compux-spike-build-$$
  "$DOCKER" rm -f "$c" >/dev/null 2>&1 || true
  "$DOCKER" create --name "$c" \
    -v "$VOLUMES-registry:/root/.cargo/registry" -v "$VOLUMES-target:/target" \
    -e CARGO_TARGET_DIR=/target -w /work/native/compux "$IMAGE-build:jammy" \
    cargo build --release >/dev/null
  "$DOCKER" cp "$context/." "$c:/work"
  rm -rf "$context"
  local rc=0
  timeout -k 10 "$BUILD_LIMIT_S" "$DOCKER" start -a "$c" || rc=$?
  "$DOCKER" cp "$c:/target/release/compux" "$CACHE/compux" || rc=1
  "$DOCKER" cp "$c:/target/release/compux-indicator-x11" "$CACHE/compux-indicator-x11" || rc=1
  "$DOCKER" rm -f "$c" >/dev/null
  [ "$rc" -eq 0 ] || die "the helper did not build"
}

spike_images() {
  local context base
  context=$(mktemp -d)
  cp "$CACHE/compux" "$CACHE/compux-indicator-x11" \
    "$HERE"/{elements_timing.py,a11y_tree.py,measure.sh,atk_bridge_check.sh,verbs.py} \
    "$HERE"/{pixels.py,x11_oracle.py,bound.py,panes.py,xcb_victim.py} \
    "$HERE"/{wayland_session.sh,chromium_session.sh,chromium_page.html} "$context/"
  for base in noble jammy; do
    build -q -t "$IMAGE:$base" --build-arg "BASE=compux-fixtures:$base" \
      -f "$HERE/spike.Dockerfile" "$context" >/dev/null
  done
  if staged wayland; then
    build -q -t "$IMAGE:wayland" -f "$HERE/wayland.Dockerfile" "$context" >/dev/null
  fi
  if staged chromium; then
    build -q -t "$IMAGE:chromium" --build-arg "BASE=$CHROMIUM_BASE" \
      -f "$HERE/chromium.Dockerfile" "$context" >/dev/null
  fi
  rm -rf "$context"
}

staged() { [[ " $STAGES " == *" $1 "* ]]; }

# One private session with one fixture; COMMAND runs inside it; /tmp/spike-out
# comes back to OUT.
in_session() {
  local image=$1 toolkit=$2 c rc=0
  shift 2
  c=compux-spike-$toolkit-$$
  "$DOCKER" rm -f "$c" >/dev/null 2>&1 || true
  "$DOCKER" create --name "$c" --network none --init --shm-size 256m \
    -e COMPUX_FIXTURES="$toolkit" "$image" private-session.sh "$@" >/dev/null
  timeout -k 10 "$RUN_LIMIT_S" "$DOCKER" start -a "$c" || rc=$?
  "$DOCKER" cp "$c:/tmp/spike-out/." "$OUT/" 2>/dev/null || true
  "$DOCKER" cp "$c:/tmp/xvfb.log" "$OUT/$toolkit-xvfb.log" 2>/dev/null || true
  "$DOCKER" rm -f "$c" >/dev/null
  return "$rc"
}

window_of() {
  case "$1" in
    gtk3) echo "Compux Fixture GTK3" ;;
    gtk4) echo "Compux Fixture GTK4" ;;
    qt) echo "Compux Fixture Qt" ;;
  esac
}

fixture_images
build_helper
spike_images

# A session image that is not the fixtures' own: SCRIPT starts its own session.
in_own_session() {
  local image=$1 name=$2 c rc=0
  shift 2
  c=compux-spike-$name-$$
  "$DOCKER" rm -f "$c" >/dev/null 2>&1 || true
  "$DOCKER" create --name "$c" --network none --init --shm-size 512m "$image" "$@" >/dev/null
  timeout -k 10 "$RUN_LIMIT_S" "$DOCKER" start -a "$c" || rc=$?
  "$DOCKER" cp "$c:/tmp/spike-out/." "$OUT/" 2>/dev/null || true
  "$DOCKER" rm -f "$c" >/dev/null
  return "$rc"
}

failed=()
if staged a; then
  for toolkit in gtk3 gtk4 qt; do
    echo "== L0(a) $toolkit (noble)"
    in_session "$IMAGE:noble" "$toolkit" \
      bash /opt/compux-spike/measure.sh "$toolkit" "$(window_of "$toolkit")" "$RUNS" \
      || failed+=("a:$toolkit")
  done
fi

if staged b; then
  echo "== L0(b) gtk3 under ATK bridge 2.38 (jammy)"
  in_session "$IMAGE:jammy" gtk3 bash /opt/compux-spike/atk_bridge_check.sh walk "$RUNS" \
    || failed+=("b:gtk3")
fi

if staged wayland; then
  echo "== L0(a) gtk4 under headless GNOME Shell, Wayland (noble)"
  in_own_session "$IMAGE:wayland" wayland /opt/compux-spike/wayland_session.sh "$RUNS" \
    || failed+=("wayland")
fi

if staged chromium; then
  echo "== L0(a) Chromium (Debian bookworm)"
  in_own_session "$IMAGE:chromium" chromium /opt/compux-spike/chromium_session.sh "$RUNS" \
    || failed+=("chromium")
fi

if staged verbs; then
  for toolkit in gtk3 gtk4 qt; do
    echo "== L1 verbs $toolkit (noble)"
    in_session "$IMAGE:noble" "$toolkit" python3 /opt/compux-spike/verbs.py \
      --binary /opt/compux/compux --toolkit "$toolkit" \
      --state "/tmp/compux-fixture-state/$toolkit.json" --out "/tmp/spike-out/$toolkit-verbs.json" \
      || failed+=("verbs:$toolkit")
  done
fi

if staged verbs-jammy; then
  echo "== L1 verbs gtk3 under ATK bridge 2.38 (jammy)"
  in_session "$IMAGE:jammy" gtk3 bash /opt/compux-spike/atk_bridge_check.sh verbs \
    || failed+=("verbs:gtk3-jammy")
fi

if staged pixels; then
  echo "== L2 X11 pixels gtk3 (noble)"
  in_session "$IMAGE:noble" gtk3 python3 /opt/compux-spike/pixels.py \
    --binary /opt/compux/compux --state /tmp/compux-fixture-state/gtk3.json \
    --out /tmp/spike-out/gtk3-pixels.json \
    || failed+=("pixels:gtk3")
fi

if staged bound; then
  echo "== L3 X11 bound window and badge gtk3 (noble)"
  in_session "$IMAGE:noble" gtk3 python3 /opt/compux-spike/bound.py \
    --binary /opt/compux/compux --state /tmp/compux-fixture-state/gtk3.json \
    --out /tmp/spike-out/gtk3-bound.json \
    || failed+=("bound:gtk3")
fi

[ "${#failed[@]}" -eq 0 ] || die "failed: ${failed[*]} (see $OUT)"
echo "SPIKE_DONE $OUT"
