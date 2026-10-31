#!/usr/bin/env bash
#
# Build the ownership indicator (native/indicator/*.swift).
#
# Plain `swiftc`, no Xcode project and no package manager: it is five files of AppKit
# with no dependency, and the crate does not link it — the helper SPAWNS it, from
# Fermix.app/Contents/MacOS/compux-indicator, which scripts/build_app.sh puts there.
#
# Usage: build_indicator.sh [arch] [out-path]
#   arch      arm64 | x86_64   (default: this machine's)
#   out-path  default: /tmp/compux-indicator
#
# For a bare development sidecar (a `compux` outside the bundle), build it as the
# helper's SIBLING, named the way the bundle names it:
#   scripts/build_indicator.sh "$(uname -m)" <dir>/compux-indicator
#
# Then:
#   /tmp/compux-indicator --self-test-headless     # no window, no GUI session needed
#   /tmp/compux-indicator --self-test              # shows a panel for about a second
#   printf '{"state":"working","app":"Safari","title":"Inbox","bounds":{"x":100,"y":100,"w":800,"h":600},"occluded":false}\n' \
#     | /tmp/compux-indicator
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE_DIR="$REPO_ROOT/native/indicator"
ARCH="${1:-$(uname -m)}"
OUT="${2:-/tmp/compux-indicator}"

[ "$(uname -s)" = "Darwin" ] || { echo "build_indicator.sh: macOS only" >&2; exit 1; }
[ -d "$SOURCE_DIR" ] || { echo "build_indicator.sh: missing $SOURCE_DIR" >&2; exit 1; }
command -v swiftc >/dev/null || {
  echo "build_indicator.sh: swiftc not found (install the Command Line Tools)" >&2
  exit 1
}

case "$ARCH" in
  arm64|x86_64) ;;
  *) echo "build_indicator.sh: unsupported arch: $ARCH (arm64 or x86_64)" >&2; exit 1 ;;
esac

# macOS 13 is the crate's floor, so it is the indicator's: anything newer would make a
# bundle that runs refuse to show its own badge. `-swift-version 5` pins the language
# mode rather than letting the toolchain pick — Swift 6 mode's actor isolation is not
# what this source is written against, and the CI runner's toolchain is older than this
# machine's. `-warnings-as-errors` because a warning here is a defect in a process
# nobody is watching the logs of.
# `-parse-as-library` because the entry point is `@main`, not top-level code.
swiftc \
  -target "${ARCH}-apple-macos13" \
  -swift-version 5 \
  -parse-as-library \
  -warnings-as-errors \
  -O \
  -o "$OUT" \
  "$SOURCE_DIR"/*.swift

echo "built: $OUT ($ARCH)"
