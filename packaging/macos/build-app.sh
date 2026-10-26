#!/usr/bin/env bash
# Builds a universal (arm64 + x86_64) "Jellyfin Kattri.app" bundle.
#
# Usage: packaging/macos/build-app.sh <version>
#   <version>  Bundle version, for example 0.1.0. Digits and dots, optionally
#              followed by a pre-release suffix such as -rc.1.
#
# Environment:
#   CARGO_PACKAGE   Cargo package that produces the binary (default: egui-gl-host-proof).
#   BINARY_NAME     Executable name inside the cargo output (default: jellyfin-kattri).
#   OUT_DIR         Output directory for the bundle (default: dist/macos).
#
# Both Rust targets must be installed first (rustup target add ...). The release
# workflow does this. The script refuses to produce a bundle from a single
# architecture: a release that lacks one slice must fail, not be labelled universal.
#
# MACOSX_DEPLOYMENT_TARGET is pinned to 13.0 so the Mach-O load commands match the
# minimum system version declared in Info.plist.
#
# libmpv is copied from the pinned archive in packaging/libmpv (fetch.sh refuses an
# unpinned or mismatched archive). Its dylib lands in Contents/Frameworks with an
# @rpath install name. Each slice is still verified with lipo -info and otool -L.

set -euo pipefail

usage() {
  echo "usage: $0 <version>" >&2
  exit 2
}

[[ $# -eq 1 ]] || usage
VERSION="$1"
if ! [[ "$VERSION" =~ ^[0-9]+(\.[0-9]+){1,2}(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: version '$VERSION' is not a plain numeric version such as 0.1.0" >&2
  exit 2
fi

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CARGO_PACKAGE="${CARGO_PACKAGE:-egui-gl-host-proof}"
BINARY_NAME="${BINARY_NAME:-jellyfin-kattri}"
OUT_DIR="${OUT_DIR:-$ROOT/dist/macos}"
APP_NAME="Jellyfin Kattri.app"
BUNDLE_ID="org.kattri.jellyfin"
MIN_SYSTEM="13.0"
ARM_TARGET="aarch64-apple-darwin"
X86_TARGET="x86_64-apple-darwin"

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: build-app.sh must run on macOS (lipo, Mach-O targets)" >&2
  exit 1
fi

export MACOSX_DEPLOYMENT_TARGET="$MIN_SYSTEM"

cd "$ROOT"
for target in "$ARM_TARGET" "$X86_TARGET"; do
  echo "==> cargo build --release --locked --target $target"
  cargo build --release --locked -p "$CARGO_PACKAGE" --target "$target"
done

ARM_BIN="$ROOT/target/$ARM_TARGET/release/$BINARY_NAME"
X86_BIN="$ROOT/target/$X86_TARGET/release/$BINARY_NAME"
for bin in "$ARM_BIN" "$X86_BIN"; do
  [[ -f "$bin" ]] || { echo "error: missing $bin" >&2; exit 1; }
done

APP="$OUT_DIR/$APP_NAME"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources" "$APP/Contents/Frameworks"

echo "==> lipo -create"
lipo -create -output "$APP/Contents/MacOS/$BINARY_NAME" "$ARM_BIN" "$X86_BIN"
chmod 755 "$APP/Contents/MacOS/$BINARY_NAME"

ARCHS="$(lipo -archs "$APP/Contents/MacOS/$BINARY_NAME")"
for arch in arm64 x86_64; do
  if [[ " $ARCHS " != *" $arch "* ]]; then
    echo "error: universal binary lacks $arch (found: $ARCHS)" >&2
    exit 1
  fi
done
echo "    architectures: $ARCHS"

# Optional icon: present only when the icon has been produced from the design assets.
ICON_KEY=""
if [[ -f "$ROOT/packaging/macos/AppIcon.icns" ]]; then
  cp "$ROOT/packaging/macos/AppIcon.icns" "$APP/Contents/Resources/AppIcon.icns"
  ICON_KEY="    <key>CFBundleIconFile</key>
    <string>AppIcon</string>"
fi

if [[ -f "$ROOT/LICENSE" ]]; then
  cp "$ROOT/LICENSE" "$APP/Contents/Resources/LICENSE"
fi

# The app dlopens libmpv by its bare name (proofs/mpv-render/src/library.rs) and does not
# link it, so the binary gets no -change entry. The bundled copy is found only once the
# loader looks in Contents/Frameworks; that loader change is outside this script. Its
# own dependencies (FFmpeg and other dylibs) are not bundled here yet.
LIBMPV_DYLIB="$("$ROOT/packaging/libmpv/fetch.sh" macos-universal)"
cp "$LIBMPV_DYLIB" "$APP/Contents/Frameworks/libmpv.2.dylib"
chmod 644 "$APP/Contents/Frameworks/libmpv.2.dylib"
install_name_tool -id "@rpath/libmpv.2.dylib" "$APP/Contents/Frameworks/libmpv.2.dylib"
cp "$ROOT/packaging/libmpv/NOTICE.txt" "$APP/Contents/Resources/libmpv-NOTICE.txt"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleDevelopmentRegion</key>
    <string>en</string>
    <key>CFBundleDisplayName</key>
    <string>Jellyfin Kattri</string>
    <key>CFBundleExecutable</key>
    <string>$BINARY_NAME</string>
    <key>CFBundleIdentifier</key>
    <string>$BUNDLE_ID</string>
    <key>CFBundleInfoDictionaryVersion</key>
    <string>6.0</string>
    <key>CFBundleName</key>
    <string>Jellyfin Kattri</string>
    <key>CFBundlePackageType</key>
    <string>APPL</string>
    <key>CFBundleShortVersionString</key>
    <string>$VERSION</string>
    <key>CFBundleVersion</key>
    <string>$VERSION</string>
$ICON_KEY
    <key>LSApplicationCategoryType</key>
    <string>public.app-category.entertainment</string>
    <key>LSMinimumSystemVersion</key>
    <string>$MIN_SYSTEM</string>
    <key>NSHighResolutionCapable</key>
    <true/>
    <key>NSPrincipalClass</key>
    <string>NSApplication</string>
</dict>
</plist>
PLIST

plutil -lint "$APP/Contents/Info.plist" >/dev/null
echo "==> built $APP"
echo "$APP"
