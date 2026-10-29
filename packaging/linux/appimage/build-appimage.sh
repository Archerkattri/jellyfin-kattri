#!/usr/bin/env bash
# Builds Jellyfin Kattri as a single-file AppImage (x86_64).
#
# Usage: packaging/linux/appimage/build-appimage.sh <version> <binary> [out-dir]
#   <version>  Release version, for example 0.1.0 (digits and dots, optional suffix).
#   <binary>   Release binary built by `cargo build --release` on the reference image.
#   [out-dir]  Defaults to dist/linux.
#
# Environment:
#   libmpv        Bundled from the pinned, SHA-256-verified archive via
#                 packaging/libmpv/fetch.sh linux-x64. The app loads libmpv at run
#                 time, so linuxdeploy is given it with --library. LIBMPV_PATH is no
#                 longer read; a distro libmpv is not bundled.
#   UPDATE_INFO   AppImageUpdate update information, for example
#                 "gh-releases-zsync|<owner>|<repo>|latest|kattri-*-linux-x64.AppImage.zsync".
#                 Set only by the release workflow for tagged releases. See below.
#
# Tool choice: linuxdeploy collects the shared libraries and writes the AppDir, and
# appimagetool packs it. appimagetool is pinned to release 1.9.1 and linuxdeploy to
# 1-alpha-20251107-1. Both are downloaded from their GitHub release URLs and must
# match the SHA-256 recorded below before they run. The values are published by the
# upstream GitHub Releases API; source URLs are recorded in README.md in this directory.
#
# Update information and the APPIMAGE variable: the AppImage runtime exports the
# variable APPIMAGE with the path of the running .AppImage file. That is the pathway
# the in-app updater would use to find its file (ADR 0004, section 10.3). The ADR
# marks this as unconfirmed, so the updater has not been verified against it. The
# update information is embedded only when UPDATE_INFO is set, and is never embedded
# for a build made without a release channel.
#
# Glibc and GPU driver libraries are excluded by linuxdeploy's default exclude list.
# The AppImage is built against the glibc of the reference image (ubuntu-22.04),
# which is the oldest userspace the release targets. Running it on a newer
# distribution is the expected case; running it on an older one is not supported.
#
# Runtime note: AppImages need FUSE to mount. Where FUSE is missing, run with
# --appimage-extract-and-run. This script sets APPIMAGE_EXTRACT_AND_RUN=1 for the
# tools it runs in CI.

set -euo pipefail

usage() {
  echo "usage: $0 <version> <binary> [out-dir]" >&2
  exit 2
}

[[ $# -ge 2 && $# -le 3 ]] || usage
VERSION="$1"
BINARY="$2"
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
OUT_DIR="${3:-$ROOT/dist/linux}"

if ! [[ "$VERSION" =~ ^[0-9]+(\.[0-9]+){1,2}(-[0-9A-Za-z.-]+)?$ ]]; then
  echo "error: version '$VERSION' is not a plain numeric version such as 0.1.0" >&2
  exit 2
fi
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: build-appimage.sh must run on Linux" >&2
  exit 1
fi
if [[ ! -x "$BINARY" ]] || [[ "$(head -c 4 "$BINARY")" != $'\x7fELF' ]]; then
  echo "error: '$BINARY' is not an executable ELF binary" >&2
  exit 1
fi

# Pinned tools. The release assets are fixed by tag; the digests are checked on download.
APPIMAGETOOL_VERSION="1.9.1"
APPIMAGETOOL_URL="https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-x86_64.AppImage"
APPIMAGETOOL_SHA256="ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0"
LINUXDEPLOY_VERSION="1-alpha-20251107-1"
LINUXDEPLOY_URL="https://github.com/linuxdeploy/linuxdeploy/releases/download/${LINUXDEPLOY_VERSION}/linuxdeploy-x86_64.AppImage"
LINUXDEPLOY_SHA256="c20cd71e3a4e3b80c3483cef793cda3f4e990aca14014d23c544ca3ce1270b4d"

TOOLS_DIR="$ROOT/target/appimage-tools"
mkdir -p "$TOOLS_DIR"

fetch_tool() {
  local url="$1" expected="$2" dest="$3"
  if [[ -z "$expected" ]]; then
    echo "error: no SHA-256 recorded for $(basename "$dest"); record it in build-appimage.sh before use" >&2
    exit 1
  fi
  if [[ ! -x "$dest" ]]; then
    echo "==> downloading $url"
    curl --fail --location --silent --show-error -o "$dest.part" "$url"
    mv "$dest.part" "$dest"
    chmod 755 "$dest"
  fi
  local actual
  actual="$(sha256sum "$dest" | cut -d' ' -f1)"
  if [[ "$actual" != "$expected" ]]; then
    echo "error: SHA-256 mismatch for $dest (expected $expected, got $actual)" >&2
    rm -f "$dest"
    exit 1
  fi
}

APPIMAGETOOL="$TOOLS_DIR/appimagetool-$APPIMAGETOOL_VERSION-x86_64.AppImage"
LINUXDEPLOY="$TOOLS_DIR/linuxdeploy-$LINUXDEPLOY_VERSION-x86_64.AppImage"
fetch_tool "$APPIMAGETOOL_URL" "$APPIMAGETOOL_SHA256" "$APPIMAGETOOL"
fetch_tool "$LINUXDEPLOY_URL" "$LINUXDEPLOY_SHA256" "$LINUXDEPLOY"

export APPIMAGE_EXTRACT_AND_RUN=1

ICON="$ROOT/packaging/linux/icons/kattri.svg"
DESKTOP="$ROOT/packaging/linux/share/kattri.desktop"
if [[ ! -f "$ICON" ]]; then
  echo "error: missing scalable SVG icon $ICON" >&2
  exit 1
fi
if command -v desktop-file-validate >/dev/null 2>&1; then
  desktop-file-validate "$DESKTOP"
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/kattri-appimage.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
APPDIR="$WORK/Kattri.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/scalable/apps"

install -m 755 "$BINARY" "$APPDIR/usr/bin/kattri"
install -m 644 "$DESKTOP" "$APPDIR/usr/share/applications/kattri.desktop"
install -m 644 "$ICON" "$APPDIR/usr/share/icons/hicolor/scalable/apps/kattri.svg"
# Redistribution requires the project license and the bundled libmpv notice.
mkdir -p "$APPDIR/usr/share/licenses/kattri"
install -m 644 "$ROOT/LICENSE" "$APPDIR/usr/share/licenses/kattri/LICENSE"
install -m 644 "$ROOT/packaging/libmpv/NOTICE.txt" \
  "$APPDIR/usr/share/licenses/kattri/libmpv-NOTICE.txt"

LINUXDEPLOY_ARGS=(
  --appdir "$APPDIR"
  --executable "$APPDIR/usr/bin/kattri"
  --desktop-file "$APPDIR/usr/share/applications/kattri.desktop"
  --icon-file "$APPDIR/usr/share/icons/hicolor/scalable/apps/kattri.svg"
)
# fetch.sh refuses an unpinned or mismatched archive, so no unverified libmpv is bundled.
LIBMPV="$("$ROOT/packaging/libmpv/fetch.sh" linux-x64)"
LINUXDEPLOY_ARGS+=(--library "$LIBMPV")

echo "==> linuxdeploy collects shared libraries"
"$LINUXDEPLOY" "${LINUXDEPLOY_ARGS[@]}"

# Replace the AppRun that linuxdeploy may have created with the one in this repository.
install -m 755 "$ROOT/packaging/linux/appimage/AppRun" "$APPDIR/AppRun"

# The check that matters for a bundle: no library the binary needs is missing.
MISSING="$(LD_LIBRARY_PATH="$APPDIR/usr/lib" ldd "$APPDIR/usr/bin/kattri" | grep 'not found' || true)"
if [[ -n "$MISSING" ]]; then
  echo "error: unresolved libraries in the bundle:" >&2
  echo "$MISSING" >&2
  exit 1
fi

mkdir -p "$OUT_DIR"
OUT="$OUT_DIR/kattri-$VERSION-linux-x64.AppImage"
rm -f "$OUT" "$OUT.zsync"

APPIMAGETOOL_ARGS=()
if [[ -n "${UPDATE_INFO:-}" ]]; then
  APPIMAGETOOL_ARGS+=(--updateinformation "$UPDATE_INFO")
fi

echo "==> appimagetool packs $OUT"
ARCH=x86_64 "$APPIMAGETOOL" "${APPIMAGETOOL_ARGS[@]}" "$APPDIR" "$OUT"
chmod 755 "$OUT"
# Hash the bare file name so `sha256sum -c` works in a directory of downloaded assets.
(cd "$(dirname "$OUT")" && sha256sum "$(basename "$OUT")" > "$(basename "$OUT").sha256")
echo "==> built $OUT"
echo "$OUT"
