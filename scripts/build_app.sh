#!/usr/bin/env bash
#
# Assemble (and sign) the Fermix.app bundle around the compux macOS binary.
#
# The bundle gives the sidecar a stable, path-independent TCC identity
# (CFBundleIdentifier = io.tezra.fermix.computer-use) plus the "Fermix" name + icon
# shown in System Settings ▸ Privacy. See bundle/Info.plist.
#
# Usage: build_app.sh <compux-binary> <out-Fermix.app> <version> [signing-identity]
#   signing-identity omitted / empty  -> ad-hoc sign (local dev; TCC works but the
#                                        grant does not persist across a rebuild)
#   signing-identity set              -> Developer-ID sign + hardened runtime (release)
set -euo pipefail

BIN="${1:?usage: build_app.sh <compux-binary> <out-Fermix.app> <version> [identity]}"
APP="${2:?usage: build_app.sh <compux-binary> <out-Fermix.app> <version> [identity]}"
VERSION="${3:?usage: build_app.sh <compux-binary> <out-Fermix.app> <version> [identity]}"
IDENTITY="${4:-}"

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

[ -f "$BIN" ] || { echo "build_app.sh: binary not found: $BIN" >&2; exit 1; }
[ -f "$REPO_ROOT/bundle/Info.plist" ] || { echo "build_app.sh: missing bundle/Info.plist" >&2; exit 1; }
[ -f "$REPO_ROOT/bundle/Fermix.icns" ] || { echo "build_app.sh: missing bundle/Fermix.icns" >&2; exit 1; }

rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN" "$APP/Contents/MacOS/compux"
chmod 0755 "$APP/Contents/MacOS/compux"
cp "$REPO_ROOT/bundle/Fermix.icns" "$APP/Contents/Resources/Fermix.icns"
sed "s/__VERSION__/${VERSION}/g" "$REPO_ROOT/bundle/Info.plist" > "$APP/Contents/Info.plist"

# Sign the whole bundle. Developer-ID (release) adds a secure timestamp + hardened
# runtime; ad-hoc (local) cannot timestamp. Either way the signature covers
# Contents/_CodeSignature/CodeResources, which must be preserved on extraction
# (ditto, never plain tar) — see lib/compux/binary.ex.
if [ -n "$IDENTITY" ]; then
  codesign --force --deep --options runtime --timestamp --sign "$IDENTITY" "$APP"
else
  codesign --force --deep --options runtime --sign - "$APP"
fi
codesign --verify --deep --strict --verbose=2 "$APP"

echo "built: $APP"
