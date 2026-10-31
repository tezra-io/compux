#!/usr/bin/env bash
#
# Assemble (and sign) the Fermix.app bundle around the compux macOS binary.
#
# The bundle gives the sidecar a stable, path-independent TCC identity
# (CFBundleIdentifier = io.tezra.fermix.computer-use) plus the "Fermix" name + icon
# shown in System Settings ▸ Privacy. See bundle/Info.plist.
#
# It carries TWO executables: the helper (Contents/MacOS/compux) and the ownership
# indicator it spawns (Contents/MacOS/compux-indicator, built here from
# native/indicator by scripts/build_indicator.sh). One bundle, one signature, one
# identity: the badge needs no grant and no second row in Privacy settings.
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

# The indicator is built for the architectures the HELPER has, never for the build
# machine's: a bundle cross-built for one host must not ship a badge that cannot launch
# on it. `lipo -create` with a single slice copies it, so one slice and two take the
# same path.
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
SLICES=()
for arch in $(lipo -archs "$BIN"); do
  "$REPO_ROOT/scripts/build_indicator.sh" "$arch" "$WORK/compux-indicator-$arch" >/dev/null
  SLICES+=("$WORK/compux-indicator-$arch")
done
[ "${#SLICES[@]}" -gt 0 ] || { echo "build_app.sh: no architecture in $BIN" >&2; exit 1; }
lipo -create "${SLICES[@]}" -output "$APP/Contents/MacOS/compux-indicator"
chmod 0755 "$APP/Contents/MacOS/compux-indicator"

# Sign inside out, with ONE spelling of the identity and its options: the nested
# executable first, so the bundle's signature seals a binary that already carries the
# hardened runtime and the timestamp, rather than trusting `--deep` to reach it. The
# indicator needs no entitlement of its own — it renders a panel and reads its stdin.
#
# Developer-ID (release) adds a secure timestamp + hardened runtime; ad-hoc (local)
# cannot timestamp. Either way the signature covers Contents/_CodeSignature/CodeResources,
# which must be preserved on extraction (ditto, never plain tar) — see lib/compux/binary.ex.
if [ -n "$IDENTITY" ]; then
  SIGN=(codesign --force --options runtime --timestamp --sign "$IDENTITY")
else
  SIGN=(codesign --force --options runtime --sign -)
fi
"${SIGN[@]}" "$APP/Contents/MacOS/compux-indicator"
"${SIGN[@]}" --deep "$APP"
codesign --verify --strict --verbose=2 "$APP/Contents/MacOS/compux-indicator"
codesign --verify --deep --strict --verbose=2 "$APP"

echo "built: $APP"
