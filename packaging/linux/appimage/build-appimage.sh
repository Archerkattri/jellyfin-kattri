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
WORK="$(mktemp -d "${TMPDIR:-/tmp}/kattri-appimage.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT
APPDIR="$WORK/Kattri.AppDir"
mkdir -p "$APPDIR/usr/bin" "$APPDIR/usr/share/applications" \
  "$APPDIR/usr/share/icons/hicolor/scalable/apps"

# A Windows checkout may expose CRLF here even though Linux source control stores LF.
# Normalize only the temporary packaging copy so desktop-file-validate and linuxdeploy
# see the format required by the freedesktop Desktop Entry specification.
DESKTOP_NORMALIZED="$WORK/kattri.desktop"
sed 's/\r$//' "$DESKTOP" > "$DESKTOP_NORMALIZED"
if command -v desktop-file-validate >/dev/null 2>&1; then
  desktop-file-validate "$DESKTOP_NORMALIZED"
fi

install -m 755 "$BINARY" "$APPDIR/usr/bin/kattri"
install -m 644 "$DESKTOP_NORMALIZED" "$APPDIR/usr/share/applications/kattri.desktop"
install -m 644 "$ICON" "$APPDIR/usr/share/icons/hicolor/scalable/apps/kattri.svg"
# fetch.sh downloads and verifies the archive before either its library or its licenses are used.
LIBMPV="$("$ROOT/packaging/libmpv/fetch.sh" linux-x64)"
# Redistribution requires the project license, the embedded Inter font license, the
# third-party crate notice, and the bundled libmpv notice.
mkdir -p "$APPDIR/usr/share/licenses/kattri"
install -m 644 "$ROOT/LICENSE" "$APPDIR/usr/share/licenses/kattri/LICENSE"
install -m 644 "$ROOT/assets/fonts/OFL.txt" "$APPDIR/usr/share/licenses/kattri/OFL.txt"
install -m 644 "$ROOT/NOTICE" "$APPDIR/usr/share/licenses/kattri/NOTICE"
install -m 644 "$ROOT/packaging/libmpv/NOTICE.txt" \
  "$APPDIR/usr/share/licenses/kattri/libmpv-NOTICE.txt"

# Include the exact upstream notices and build record from the SHA-256-verified archive.
# The short project notice explicitly says it does not replace these upstream materials.
shopt -s nullglob
LIBMPV_ARCHIVES=("$ROOT"/target/libmpv/linux-x64/*.tar.gz)
if [[ ${#LIBMPV_ARCHIVES[@]} -ne 1 ]]; then
  echo "error: expected one checksum-verified Linux libmpv archive, found ${#LIBMPV_ARCHIVES[@]}" >&2
  exit 1
fi
LIBMPV_NOTICES="$WORK/libmpv-notices"
mkdir -p "$LIBMPV_NOTICES"
tar -xzf "${LIBMPV_ARCHIVES[0]}" -C "$LIBMPV_NOTICES" --strip-components=1 \
  --wildcards '*/BUILD-INFO.txt' '*/LICENSE.*'
for notice in BUILD-INFO.txt LICENSE.mpv.LGPL LICENSE.ffmpeg.LGPL \
  LICENSE.libplacebo.LGPL LICENSE.dav1d.BSD-2-Clause LICENSE.libdisplay-info.MIT; do
  if [[ ! -s "$LIBMPV_NOTICES/$notice" ]]; then
    echo "error: pinned libmpv archive is missing required notice $notice" >&2
    exit 1
  fi
  install -m 644 "$LIBMPV_NOTICES/$notice" "$APPDIR/usr/share/licenses/kattri/$notice"
done

# libmpv also loads the host's libjpeg.so.8. The AppImage includes that ABI when linuxdeploy
# resolves it, so include the exact copyright metadata and package version from the same build
# host rather than treating the library as part of the pinned libmpv archive.
LIBJPEG_COPYRIGHT="/usr/share/doc/libjpeg-turbo8/copyright"
if [[ ! -s "$LIBJPEG_COPYRIGHT" ]]; then
  echo "error: missing Ubuntu libjpeg-turbo copyright file $LIBJPEG_COPYRIGHT" >&2
  exit 1
fi
install -m 644 "$LIBJPEG_COPYRIGHT" "$APPDIR/usr/share/licenses/kattri/LICENSE.libjpeg-turbo"
dpkg-query -W -f='${Package} ${Version}\n' libjpeg-turbo8 \
  > "$APPDIR/usr/share/licenses/kattri/BUILD-INFO.libjpeg-turbo.txt"

LINUXDEPLOY_ARGS=(
  --appdir "$APPDIR"
  --executable "$APPDIR/usr/bin/kattri"
  --desktop-file "$APPDIR/usr/share/applications/kattri.desktop"
  --icon-file "$APPDIR/usr/share/icons/hicolor/scalable/apps/kattri.svg"
)
# fetch.sh refuses an unpinned or mismatched archive, so no unverified libmpv is bundled.
LIBXKBCOMMON_X11="$(
  ldconfig -p | awk '
    $1 == "libxkbcommon-x11.so.0" && !found { library = $NF; found = 1 }
    END { if (found) print library }
  '
)"
if [[ -z "$LIBXKBCOMMON_X11" || ! -f "$LIBXKBCOMMON_X11" ]]; then
  echo "error: libxkbcommon-x11.so.0 is required to collect the window backend dependency" >&2
  exit 1
fi
LINUXDEPLOY_ARGS+=(--library "$LIBMPV" --library "$LIBXKBCOMMON_X11")

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
if ! find "$APPDIR/usr/lib" -type f -name 'libxkbcommon-x11.so.0*' -print -quit | grep -q .; then
  echo 'error: AppImage does not contain the dynamically loaded libxkbcommon-x11 runtime' >&2
  exit 1
fi
if [[ ! -e "$APPDIR/usr/lib/libmpv.so.2" ]]; then
  echo 'error: AppImage does not contain libmpv at usr/lib/libmpv.so.2' >&2
  exit 1
fi
if [[ ! -e "$APPDIR/usr/lib/libjpeg.so.8" ]]; then
  echo 'error: AppImage does not contain libjpeg.so.8 required by the pinned libmpv build' >&2
  exit 1
fi
if LD_LIBRARY_PATH="$APPDIR/usr/lib" ldd "$APPDIR/usr/lib/libmpv.so.2" | grep -q 'not found'; then
  echo 'error: bundled libmpv has unresolved native dependencies' >&2
  LD_LIBRARY_PATH="$APPDIR/usr/lib" ldd "$APPDIR/usr/lib/libmpv.so.2" >&2
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

# Preserve raw copies of the exact package payloads for the release-native sidecar. They are
# evidence files in the workflow artifact, not additional published release assets.
EVIDENCE="$OUT_DIR/release-validation"
mkdir -p "$EVIDENCE/native" "$EVIDENCE/licenses/libmpv" \
  "$EVIDENCE/licenses/libjpeg-turbo" "$EVIDENCE/executables"
cp "$APPDIR/usr/bin/kattri" "$EVIDENCE/executables/kattri"
cp -L "$APPDIR/usr/lib/libmpv.so.2" "$EVIDENCE/native/libmpv.so.2"
cp -L "$APPDIR/usr/lib/libjpeg.so.8" "$EVIDENCE/native/libjpeg.so.8"
for notice in LICENSE.mpv.LGPL LICENSE.ffmpeg.LGPL LICENSE.libplacebo.LGPL \
  LICENSE.dav1d.BSD-2-Clause LICENSE.libdisplay-info.MIT; do
  cp "$APPDIR/usr/share/licenses/kattri/$notice" "$EVIDENCE/licenses/libmpv/$notice"
done
cp "$APPDIR/usr/share/licenses/kattri/BUILD-INFO.txt" "$EVIDENCE/licenses/libmpv/BUILD-INFO.txt"
cp "$APPDIR/usr/share/licenses/kattri/libmpv-NOTICE.txt" "$EVIDENCE/licenses/libmpv/libmpv-NOTICE.txt"
cp "$APPDIR/usr/share/licenses/kattri/LICENSE.libjpeg-turbo" \
  "$EVIDENCE/licenses/libjpeg-turbo/LICENSE.libjpeg-turbo"
cp "$APPDIR/usr/share/licenses/kattri/BUILD-INFO.libjpeg-turbo.txt" \
  "$EVIDENCE/licenses/libjpeg-turbo/BUILD-INFO.libjpeg-turbo.txt"
find "$APPDIR/usr/lib" \( -type f -o -type l \) -name '*.so*' -printf '%P\n' | sort \
  > "$EVIDENCE/linuxdeploy-shared-libraries.txt"

echo "==> built $OUT"
echo "$OUT"
