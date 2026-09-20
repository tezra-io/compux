#!/usr/bin/env bash
#
# Build the macOS fixture application (fixtures/macos/FixtureApp.swift).
#
# Plain `swiftc`, no Xcode project and no .app bundle: it is a target for the
# live check and for later evals, NEVER part of the release. Nothing in the crate
# links it and `release.yml` does not know it exists.
#
# Usage: build_fixture_app.sh [out-path]        (default: /tmp/FixtureApp)
#
# Then:
#   /tmp/FixtureApp --self-test --state-file /tmp/compux-fixture.json
#   /tmp/FixtureApp --state-file /tmp/compux-fixture.json
#
# The self-test drives the handlers in process: it shows no window and posts no
# input, so it needs neither a window server nor an Accessibility grant.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SOURCE="$REPO_ROOT/fixtures/macos/FixtureApp.swift"
OUT="${1:-/tmp/FixtureApp}"

[ "$(uname -s)" = "Darwin" ] || { echo "build_fixture_app.sh: macOS only" >&2; exit 1; }
[ -f "$SOURCE" ] || { echo "build_fixture_app.sh: missing $SOURCE" >&2; exit 1; }
command -v swiftc >/dev/null || {
  echo "build_fixture_app.sh: swiftc not found (install the Command Line Tools)" >&2
  exit 1
}

# macOS 12 as the floor so SwiftUI's `onChange` and the checkbox toggle style need
# no availability dance, and `-warnings-as-errors` because a fixture that compiles
# with warnings is a fixture nobody reads.
# `-parse-as-library` because the entry point is `@main`, not top-level code: a
# single file called anything but main.swift cannot have both.
swiftc \
  -target "$(uname -m)-apple-macos12.0" \
  -parse-as-library \
  -warnings-as-errors \
  -O \
  -o "$OUT" \
  "$SOURCE"

echo "built: $OUT"
