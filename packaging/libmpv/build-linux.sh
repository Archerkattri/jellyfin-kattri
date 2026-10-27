#!/usr/bin/env bash
# Builds an LGPL libmpv.so.2 for Linux x86_64 from pinned upstream sources.
#
# Output (in the directory given as $1, default ./out):
#   kattri-libmpv-linux-x64-<MPV_TAG>.tar.gz   libmpv.so.2 plus LICENSE texts and BUILD-INFO.txt
#   <archive>.sha256
#
# Licensing: mpv is built with -Dgpl=false (LGPL-2.1-or-later). FFmpeg is configured without
# --enable-gpl, --enable-version3 and --enable-nonfree, so it stays LGPL-2.1-or-later, and is
# linked statically into libmpv together with libplacebo (LGPL-2.1-or-later) and dav1d (BSD-2).
# dav1d is built from source here (the distro archive is not position-independent).
# libass, freetype, harfbuzz, fribidi, fontconfig, X11, Wayland, GL and the audio backends stay
# shared system libraries. X11 output contexts are GPL-only in mpv, so an LGPL libmpv has none;
# the app creates its own GL context and hands it to the render API. Nothing here enables x264, x265, fdk-aac or other GPL/nonfree code.
#
# Every source is a git tag whose commit hash is checked below, so a moved tag fails the build.
# Build host: Ubuntu 22.04 (the glibc 2.35 floor of the Linux release).
#
# Packages (Ubuntu): build-essential git meson ninja-build nasm pkg-config python3-jinja2
#   hwdata libva-dev libdrm-dev libdav1d-dev zlib1g-dev libass-dev libfreetype-dev
#   libharfbuzz-dev libfribidi-dev libfontconfig-dev libegl-dev libgl-dev libx11-dev libxext-dev
#   libxrandr-dev libxpresent-dev libxss-dev libxkbcommon-dev libwayland-dev wayland-protocols
#   libasound2-dev libpulse-dev libuchardet-dev

set -euo pipefail

FFMPEG_TAG="n9.0.2"
FFMPEG_COMMIT="946fcce07b6dcd0331c8cc609192aeff5e1924f8"
PLACEBO_TAG="v7.360.1"
PLACEBO_COMMIT="cee9b076f2c63104ccfd497fa79c39a867293ec4"
DAV1D_TAG="1.5.4"
DAV1D_COMMIT="54706fc6bc0cdecab7e9593974a4039cc038fca7"
DISPLAYINFO_TAG="0.2.0"
DISPLAYINFO_COMMIT="66b802d05b374cd8f388dc6ad1e7ae4f08cb3300"
MPV_TAG="v0.41.0"
MPV_COMMIT="41f6a645068483470267271e1d09966ca3b9f413"

OUT="$(mkdir -p "${1:-out}" && cd "${1:-out}" && pwd)"
# KATTRI_LIBMPV_WORK keeps the build tree between runs (finished stages are skipped); without
# it a temporary tree is used and removed.
if [[ -n "${KATTRI_LIBMPV_WORK:-}" ]]; then
  WORK="$(mkdir -p "$KATTRI_LIBMPV_WORK" && cd "$KATTRI_LIBMPV_WORK" && pwd)"
else
  WORK="$(mktemp -d "${TMPDIR:-/tmp}/kattri-libmpv.XXXXXX")"
  trap 'rm -rf "$WORK"' EXIT
fi
PREFIX="$WORK/prefix"
JOBS="$(nproc)"
stage_done() { [[ -f "$WORK/.done-$1" ]]; }
mark_done() { touch "$WORK/.done-$1"; }
mkdir -p "$PREFIX"
export PKG_CONFIG_PATH="$PREFIX/lib/pkgconfig:$PREFIX/lib/x86_64-linux-gnu/pkgconfig"
# mpv asks pkg-config for FFmpeg, libplacebo and dav1d without --static. Those are static
# archives, so their private dependencies (zlib, libva, ...) must be on the link line too.
cat > "$WORK/pkg-config-static" <<'WRAP'
#!/bin/sh
exec pkg-config --static "$@"
WRAP
chmod +x "$WORK/pkg-config-static"
export PKG_CONFIG="$WORK/pkg-config-static"

clone() { # url tag commit dir
  git clone --quiet --depth 1 --branch "$2" "$1" "$4"
  local actual
  actual="$(git -C "$4" rev-parse HEAD)"
  [[ "$actual" == "$3" ]] || { echo "error: $1 $2 is $actual, expected $3" >&2; exit 1; }
}

echo "==> dav1d $DAV1D_TAG (static)"
if ! stage_done dav1d; then
rm -rf "$WORK/dav1d"
clone https://github.com/videolan/dav1d "$DAV1D_TAG" "$DAV1D_COMMIT" "$WORK/dav1d"
meson setup "$WORK/dav1d/build" "$WORK/dav1d" --prefix="$PREFIX" --libdir=lib --buildtype=release \
  --default-library=static -Db_staticpic=true -Denable_tools=false -Denable_tests=false \
  -Denable_examples=false
ninja -C "$WORK/dav1d/build" install
mark_done dav1d
fi

echo "==> FFmpeg $FFMPEG_TAG (LGPL, static)"
if ! stage_done ffmpeg; then
rm -rf "$WORK/ffmpeg"
clone https://github.com/FFmpeg/FFmpeg "$FFMPEG_TAG" "$FFMPEG_COMMIT" "$WORK/ffmpeg"
(
  cd "$WORK/ffmpeg"
  ./configure --prefix="$PREFIX" --libdir="$PREFIX/lib" \
    --disable-gpl --disable-nonfree --disable-version3 \
    --disable-programs --disable-doc --disable-debug \
    --disable-shared --enable-static --enable-pic \
    --disable-autodetect \
    --enable-zlib --enable-vaapi --enable-libdav1d \
    --disable-network --disable-encoders --disable-muxers
  make -j"$JOBS"
  make install
)
mark_done ffmpeg
fi
if ! grep -q 'define FFMPEG_LICENSE "LGPL version 2.1 or later"' "$WORK/ffmpeg/config.h"; then
  echo "error: FFmpeg did not configure as LGPL 2.1 or later" >&2
  grep -n "FFMPEG_LICENSE" "$WORK/ffmpeg/config.h" >&2 || true
  exit 1
fi

echo "==> libplacebo $PLACEBO_TAG (static)"
if ! stage_done placebo; then
rm -rf "$WORK/libplacebo"
git clone --quiet --depth 1 --branch "$PLACEBO_TAG" --recurse-submodules --shallow-submodules \
  https://github.com/haasn/libplacebo "$WORK/libplacebo"
[[ "$(git -C "$WORK/libplacebo" rev-parse HEAD)" == "$PLACEBO_COMMIT" ]] \
  || { echo "error: libplacebo $PLACEBO_TAG moved" >&2; exit 1; }
meson setup "$WORK/libplacebo/build" "$WORK/libplacebo" --prefix="$PREFIX" --libdir=lib \
  --buildtype=release --default-library=static -Db_pie=false -Db_staticpic=true \
  -Ddemos=false -Dtests=false -Dbench=false -Dvulkan=disabled -Dshaderc=disabled \
  -Dglslang=disabled -Dlcms=disabled -Dd3d11=disabled -Ddovi=disabled -Dlibdovi=disabled \
  -Dopengl=enabled -Dgl-proc-addr=enabled -Dunwind=disabled -Dxxhash=disabled
ninja -C "$WORK/libplacebo/build" install
mark_done placebo
fi

echo "==> libdisplay-info $DISPLAYINFO_TAG (static, needed by mpv's DRM support)"
if ! stage_done displayinfo; then
rm -rf "$WORK/libdisplay-info"
clone https://gitlab.freedesktop.org/emersion/libdisplay-info.git "$DISPLAYINFO_TAG" "$DISPLAYINFO_COMMIT" \
  "$WORK/libdisplay-info"
meson setup "$WORK/libdisplay-info/build" "$WORK/libdisplay-info" --prefix="$PREFIX" --libdir=lib \
  --buildtype=release --default-library=static -Db_staticpic=true
ninja -C "$WORK/libdisplay-info/build" install
mark_done displayinfo
fi

echo "==> mpv $MPV_TAG (libmpv only, -Dgpl=false)"
rm -rf "$WORK/mpv"
clone https://github.com/mpv-player/mpv "$MPV_TAG" "$MPV_COMMIT" "$WORK/mpv"
meson setup "$WORK/mpv/build" "$WORK/mpv" --prefix="$PREFIX" --libdir=lib \
  --buildtype=release -Db_pie=false \
  -Dlibmpv=true -Dcplayer=false -Dgpl=false -Dtests=false -Dmanpage-build=disabled \
  -Dlua=disabled -Djavascript=disabled -Dlcms2=disabled -Dvulkan=disabled \
  -Dlibarchive=disabled -Drubberband=disabled -Dzimg=disabled \
  -Dgl=enabled -Degl=enabled -Dx11=disabled -Dwayland=disabled -Ddrm=enabled -Dvaapi=enabled -Dvaapi-drm=enabled -Dvdpau=disabled
ninja -C "$WORK/mpv/build" install

LIB="$(find "$PREFIX/lib" -name 'libmpv.so.2*' -type f | head -1)"
[[ -n "$LIB" ]] || { echo "error: libmpv.so.2 was not produced" >&2; exit 1; }

echo "==> verify"
# libavcodec, libavformat, libavutil, libswresample and libplacebo must be inside the library.
if readelf -d "$LIB" | grep -E 'NEEDED.*(libav|libsw|libplacebo|libdav1d|libdisplay-info)'; then
  echo "error: FFmpeg or libplacebo is dynamically linked, the archive would not be self-contained" >&2
  exit 1
fi
readelf -d "$LIB" | grep -E 'SONAME'
nm -D --defined-only "$LIB" | grep ' mpv_create$' >/dev/null || { echo "error: mpv_create is not exported" >&2; exit 1; }
nm -D --defined-only "$LIB" | grep ' mpv_render_context_create$' >/dev/null \
  || { echo "error: the render API is not exported" >&2; exit 1; }

STAGE="$WORK/stage"
NAME="kattri-libmpv-linux-x64-${MPV_TAG}"
mkdir -p "$STAGE/$NAME"
install -m 755 "$LIB" "$STAGE/$NAME/libmpv.so.2"
strip --strip-unneeded "$STAGE/$NAME/libmpv.so.2"
cp "$WORK/mpv/LICENSE.LGPL" "$STAGE/$NAME/LICENSE.mpv.LGPL"
cp "$WORK/ffmpeg/COPYING.LGPLv2.1" "$STAGE/$NAME/LICENSE.ffmpeg.LGPL"
cp "$WORK/libplacebo/LICENSE" "$STAGE/$NAME/LICENSE.libplacebo.LGPL"
cp "$WORK/dav1d/COPYING" "$STAGE/$NAME/LICENSE.dav1d.BSD-2-Clause"
cp "$WORK/libdisplay-info/LICENSE" "$STAGE/$NAME/LICENSE.libdisplay-info.MIT"
{
  echo "mpv $MPV_TAG ($MPV_COMMIT) -Dgpl=false"
  echo "FFmpeg $FFMPEG_TAG ($FFMPEG_COMMIT) LGPL 2.1 or later, static"
  echo "libplacebo $PLACEBO_TAG ($PLACEBO_COMMIT) static"
  echo "dav1d $DAV1D_TAG ($DAV1D_COMMIT) static"
  echo "libdisplay-info $DISPLAYINFO_TAG ($DISPLAYINFO_COMMIT) static"
  echo "Build host: $(. /etc/os-release && echo "$PRETTY_NAME"), glibc $(ldd --version | head -1 | awk '{print $NF}')"
  echo "--- shared dependencies"
  readelf -d "$STAGE/$NAME/libmpv.so.2" | grep NEEDED
  echo "--- FFmpeg configuration"
  grep -E '^#define (FFMPEG_CONFIGURATION|FFMPEG_LICENSE)' "$WORK/ffmpeg/config.h"
} > "$STAGE/$NAME/BUILD-INFO.txt"
tar -C "$STAGE" --sort=name --owner=0 --group=0 --numeric-owner --mtime='2026-01-01 00:00:00Z' \
  -cf - "$NAME" | gzip -9n > "$OUT/$NAME.tar.gz"
(cd "$OUT" && sha256sum "$NAME.tar.gz" | tee "$NAME.tar.gz.sha256")
cat "$STAGE/$NAME/BUILD-INFO.txt"
