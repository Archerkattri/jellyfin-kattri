#!/usr/bin/env bash
# Wraps a "Jellyfin Kattri.app" bundle in a compressed disk image for drag-install.
#
# Usage: packaging/macos/make-dmg.sh <path-to-app> <version> [output-dir]
#   <path-to-app>  The .app bundle produced by build-app.sh (or signed by sign-and-notarize.sh).
#   <version>      Version used in the image name and volume name.
#   [output-dir]   Defaults to the directory containing the app.
#
# Output: <output-dir>/kattri-<version>-macos-universal.dmg
#
# The image holds the app and a symbolic link to /Applications so the user can
# drag the app across. Signing the disk image itself is handled by
# sign-and-notarize.sh, which runs after this script.

set -euo pipefail

usage() {
  echo "usage: $0 <path-to-app> <version> [output-dir]" >&2
  exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage
APP_PATH="$1"
VERSION="$2"
OUT_DIR="${3:-$(dirname "$APP_PATH")}"

if ! [[ "$VERSION" =~ ^[0-9]+(\.[0-9]+){1,2}(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: version '$VERSION' is not a plain numeric version such as 0.1.0" >&2
  exit 2
fi
if [[ ! -d "$APP_PATH" || "$APP_PATH" != *.app ]]; then
  echo "error: '$APP_PATH' is not an .app bundle" >&2
  exit 1
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: make-dmg.sh must run on macOS (hdiutil)" >&2
  exit 1
fi

APP_BASENAME="$(basename "$APP_PATH")"
DMG="$OUT_DIR/kattri-$VERSION-macos-universal.dmg"
VOLUME_NAME="Jellyfin Kattri $VERSION"
STAGING="$(mktemp -d "${TMPDIR:-/tmp}/kattri-dmg.XXXXXX")"
trap 'rm -rf "$STAGING"' EXIT

mkdir -p "$OUT_DIR"
cp -R "$APP_PATH" "$STAGING/$APP_BASENAME"
ln -s /Applications "$STAGING/Applications"

rm -f "$DMG"
echo "==> hdiutil create $DMG"
hdiutil create \
  -volname "$VOLUME_NAME" \
  -srcfolder "$STAGING" \
  -fs HFS+ \
  -format UDZO \
  -imagekey zlib-level=9 \
  -ov \
  "$DMG"

hdiutil verify "$DMG" >/dev/null
# Hash the bare file name so `shasum -c` works in a directory of downloaded assets. The release
# workflow rewrites this after signing and notarizing, which change the file's bytes.
(cd "$(dirname "$DMG")" && shasum -a 256 "$(basename "$DMG")" > "$(basename "$DMG").sha256")
echo "==> built $DMG"
echo "$DMG"
