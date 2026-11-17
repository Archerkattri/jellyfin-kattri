#!/usr/bin/env bash
# Builds a single-file macOS installer package (.pkg) that installs
# "Jellyfin Kattri.app" into /Applications.
#
# Usage: packaging/macos/build-pkg.sh <version> <path-to-.app> <output-dir>
#   <version>       Release version in X.Y.Z form, for example 0.8.0.
#   <path-to-.app>  The universal bundle produced by build-app.sh and, when
#                   Developer ID credentials exist, signed and notarized by
#                   sign-and-notarize.sh.
#   <output-dir>    Directory that receives the installer file; the release job
#                   passes dist-macos, so the output is
#                   dist-macos/JellyfinKattri-<version>.pkg.
#
# Output: <output-dir>/JellyfinKattri-<version>.pkg. Exactly one file is
# written, with no checksum sidecar, because signing a product archive happens
# after this script and would change its bytes.
#
# Architecture: the package is architecture-neutral and needs no per-arch
# variant. The payload is the .app exactly as given, so one .pkg installs on
# both Apple silicon (arm64) and Intel (x86_64) Macs. Universality comes from
# build-app.sh, which merges the two Rust targets with lipo; this script never
# rebuilds, thins or re-merges the bundle, and the postinstall script contains no
# architecture logic. The slices of the main executable are reported with lipo
# when that tool is present. macOS Installer selects no architecture for a
# package built this way, which is why the same file serves both.
#
# Signing: this script signs nothing. The bundle already carries its signature
# (ad-hoc from the release workflow, or Developer ID plus a notarization ticket
# from sign-and-notarize.sh), and productbuild is called without --sign, so the
# product archive is unsigned and keeps the app's existing signature. pkgutil
# --check-signature is a report only: an unsigned package is a supported output
# (owner decision) and never becomes a failure here.
#
# Payload layout: pkgbuild --root stages a directory whose single entry is the
# app bundle, and --install-location /Applications places that entry directly in
# /Applications. The generated postinstall script clears the
# com.apple.quarantine attribute from the installed app with /usr/bin/xattr,
# best effort, without network access, and always exits 0 so a missing tool or
# attribute can never fail the installation.
#
# Requires macOS (pkgbuild, productbuild, ditto, lipo, pkgutil) and the Xcode
# Command Line Tools. The version is used for the component and product versions
# and the file name; pre-release suffixes are rejected so a package version is
# always X.Y.Z.

set -euo pipefail

usage() {
  echo "usage: $0 <version> <path-to-.app> <output-dir>" >&2
  exit 2
}

[[ $# -eq 3 ]] || usage
VERSION="$1"
APP_PATH="$2"
OUT_DIR="$3"

APP_BASENAME="Jellyfin Kattri.app"
INSTALL_LOCATION="/Applications"
IDENTIFIER="org.kattri.JellyfinKattri"
PKG_NAME="JellyfinKattri-$VERSION.pkg"

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: version '$VERSION' is not a plain numeric version such as 0.8.0" >&2
  exit 2
fi
if [[ ! -d "$APP_PATH" ]]; then
  echo "error: '$APP_PATH' is not a directory; pass the .app bundle built by build-app.sh" >&2
  exit 1
fi
if [[ "$APP_PATH" != *.app ]]; then
  echo "error: '$APP_PATH' is not an .app bundle" >&2
  exit 1
fi
if [[ "$(basename "$APP_PATH")" != "$APP_BASENAME" ]]; then
  echo "error: '$APP_PATH' must be named '$APP_BASENAME'; the postinstall script clears quarantine on the installed copy of that bundle" >&2
  exit 1
fi
if [[ ! -f "$APP_PATH/Contents/Info.plist" ]]; then
  echo "error: '$APP_PATH/Contents/Info.plist' is missing; the bundle is incomplete" >&2
  exit 1
fi
if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: build-pkg.sh must run on macOS (pkgbuild, productbuild)" >&2
  exit 1
fi

for tool in pkgbuild productbuild ditto; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "error: $tool is not on PATH; install the Xcode Command Line Tools" >&2
    exit 1
  fi
done

# Report the slices, never enforce them: a single-slice bundle still produces an
# installable package, and build-app.sh is what refuses a non-universal release
# bundle. A failure of lipo or PlistBuddy must not stop packaging.
if command -v lipo >/dev/null 2>&1; then
  MAIN_EXECUTABLE="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleExecutable' \
    "$APP_PATH/Contents/Info.plist" 2>/dev/null || true)"
  if [[ -n "$MAIN_EXECUTABLE" && -f "$APP_PATH/Contents/MacOS/$MAIN_EXECUTABLE" ]]; then
    ARCHS="$(lipo -archs "$APP_PATH/Contents/MacOS/$MAIN_EXECUTABLE" 2>/dev/null || true)"
    if [[ -n "$ARCHS" ]]; then
      echo "    main executable architectures: $ARCHS"
    fi
  fi
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/kattri-pkg.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
PAYLOAD_ROOT="$WORK/root"
SCRIPTS_DIR="$WORK/scripts"
COMPONENT_PKG="$WORK/component.pkg"
mkdir -p "$PAYLOAD_ROOT" "$SCRIPTS_DIR"

# ditto copies bundle metadata and symlinks unchanged, so the payload is the
# bundle that was signed rather than a lossy re-creation of it.
echo "==> staging $APP_BASENAME"
ditto "$APP_PATH" "$PAYLOAD_ROOT/$APP_BASENAME"

cat > "$SCRIPTS_DIR/postinstall" <<'POSTINSTALL'
#!/bin/sh
# Runs as root after the payload is installed. Clearing the quarantine bit lets a
# notarized or ad-hoc signed build launch without the unidentified-developer
# prompt. Best effort: no network access, and the installation must succeed even
# when xattr is unavailable or the attribute is already absent.
set -u
APP="/Applications/Jellyfin Kattri.app"
if [ -d "$APP" ] && [ -x /usr/bin/xattr ]; then
  /usr/bin/xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true
fi
exit 0
POSTINSTALL
chmod 755 "$SCRIPTS_DIR/postinstall"

echo "==> pkgbuild (install location $INSTALL_LOCATION)"
pkgbuild \
  --root "$PAYLOAD_ROOT" \
  --identifier "$IDENTIFIER" \
  --version "$VERSION" \
  --install-location "$INSTALL_LOCATION" \
  --scripts "$SCRIPTS_DIR" \
  "$COMPONENT_PKG"

mkdir -p "$OUT_DIR"
OUT="$(cd "$OUT_DIR" && pwd)/$PKG_NAME"
rm -f "$OUT"

echo "==> productbuild $OUT"
productbuild \
  --package "$COMPONENT_PKG" \
  --identifier "$IDENTIFIER" \
  --version "$VERSION" \
  "$OUT"

if [[ ! -s "$OUT" ]]; then
  echo "error: productbuild did not write $OUT" >&2
  exit 1
fi

# Signature state is reported, not required. The installers are unsigned by owner
# decision; signing the product archive is a separate release step.
if command -v pkgutil >/dev/null 2>&1; then
  echo "==> pkgutil --check-signature $PKG_NAME"
  if ! pkgutil --check-signature "$OUT"; then
    echo "note: $PKG_NAME is not signed; the app inside keeps its own signature."
  fi
fi

echo "==> built $OUT"
echo "$OUT"
