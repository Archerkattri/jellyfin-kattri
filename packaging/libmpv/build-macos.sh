#!/usr/bin/env bash
# Builds an LGPL libmpv.2.dylib for ONE macOS architecture from pinned upstream sources.
# Run it natively: arm64 on an Apple-silicon runner, x86_64 on an Intel runner. The release
# workflow then merges the two with lipo (packaging/libmpv/combine-macos.sh).
#
# Usage: build-macos.sh <arm64|x86_64> [out-dir]
# Output: <out-dir>/libmpv.2.dylib and <out-dir>/BUILD-INFO.txt
#
# Licensing: mpv is built with -Dgpl=false (LGPL-2.1-or-later). FFmpeg is configured without
# --enable-gpl, --enable-version3 and --enable-nonfree. FFmpeg, libplacebo, libass, FreeType,
# HarfBuzz, FriBidi and dav1d are linked statically, so the dylib depends on system libraries
# and frameworks only (checked with otool -L below). The LGPL permits this static link only when
# users can relink: the exact sources and this script are the corresponding source offer
# (see packaging/libmpv/NOTICE.txt).
#
# Every source is a git tag whose commit hash is checked, so a moved tag fails the build.
# Tools (Homebrew): meson ninja nasm pkg-config autoconf automake libtool

set -euo pipefail

ARCH="${1:?usage: build-macos.sh <arm64|x86_64> [out-dir]}"
case "$ARCH" in arm64 | x86_64) ;; *) echo "error: ARCH must be arm64 or x86_64" >&2; exit 2 ;; esac
[[ "$(uname -s)" == "Darwin" ]] || { echo "error: run on macOS" >&2; exit 1; }
[[ "$(uname -m)" == "$ARCH" ]] || { echo "error: this host is $(uname -m); build $ARCH on a matching runner" >&2; exit 1; }

FFMPEG_TAG="n9.0.2";      FFMPEG_COMMIT="946fcce07b6dcd0331c8cc609192aeff5e1924f8"
PLACEBO_TAG="v7.360.1";   PLACEBO_COMMIT="cee9b076f2c63104ccfd497fa79c39a867293ec4"
MPV_TAG="v0.41.0";        MPV_COMMIT="41f6a645068483470267271e1d09966ca3b9f413"
DAV1D_TAG="1.5.4";        DAV1D_COMMIT="54706fc6bc0cdecab7e9593974a4039cc038fca7"
FREETYPE_TAG="VER-2-14-3"; FREETYPE_COMMIT="0a0221a1347e2f1e07c395263540026e9a0aa7c7"
HARFBUZZ_TAG="14.6.0";    HARFBUZZ_COMMIT="c7a7457b7385f33178e8cf87615ca077a810bbe7"
FRIBIDI_TAG="v1.0.17";    FRIBIDI_COMMIT="b93119f5fdc7ea47672cc304c1455ffa6dfe7536"
LIBASS_TAG="0.17.5";      LIBASS_COMMIT="4a05d8127f525943ebf45fdc6497c9e665947f0d"

OUT="$(mkdir -p "${2:-out}" && cd "${2:-out}" && pwd)"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/kattri-libmpv.XXXXXX")"
PREFIX="$WORK/prefix"
JOBS="$(sysctl -n hw.ncpu)"
trap 'rm -rf "$WORK"' EXIT
mkdir -p "$PREFIX"

export MACOSX_DEPLOYMENT_TARGET="13.0"
# Only our prefix: Homebrew copies of these libraries must never be picked up.
export PKG_CONFIG_LIBDIR="$PREFIX/lib/pkgconfig"
export PKG_CONFIG_PATH=""
# The libraries are static archives, so their private dependencies (system frameworks and
# libraries) must be on every link line: ask pkg-config for them always.
REAL_PKG_CONFIG="$(command -v pkg-config)"
cat > "$WORK/pkg-config-static" <<WRAP
#!/bin/sh
exec "$REAL_PKG_CONFIG" --static "\$@"
WRAP
chmod +x "$WORK/pkg-config-static"
export PKG_CONFIG="$WORK/pkg-config-static"
export CFLAGS="-O2 -arch $ARCH -mmacosx-version-min=13.0 -fPIC"
export CXXFLAGS="$CFLAGS"
export LDFLAGS="-arch $ARCH -mmacosx-version-min=13.0"

clone() { # url tag commit dir
  git clone --quiet --depth 1 --branch "$2" "$1" "$4"
  local actual
  actual="$(git -C "$4" rev-parse HEAD)"
  [[ "$actual" == "$3" ]] || { echo "error: $1 $2 is $actual, expected $3" >&2; exit 1; }
}

meson_static() { # dir extra-args...
  local dir="$1"; shift
  meson setup "$dir/build" "$dir" --prefix="$PREFIX" --libdir=lib --buildtype=release \
    --default-library=static -Db_staticpic=true "$@"
  ninja -C "$dir/build" install
}

echo "==> dav1d $DAV1D_TAG"
clone https://github.com/videolan/dav1d "$DAV1D_TAG" "$DAV1D_COMMIT" "$WORK/dav1d"
meson_static "$WORK/dav1d" -Denable_tools=false -Denable_tests=false -Denable_examples=false

echo "==> FreeType $FREETYPE_TAG"
clone https://github.com/freetype/freetype "$FREETYPE_TAG" "$FREETYPE_COMMIT" "$WORK/freetype"
meson_static "$WORK/freetype" -Dharfbuzz=disabled -Dpng=disabled -Dbzip2=disabled \
  -Dbrotli=disabled -Dzlib=system

echo "==> HarfBuzz $HARFBUZZ_TAG"
clone https://github.com/harfbuzz/harfbuzz "$HARFBUZZ_TAG" "$HARFBUZZ_COMMIT" "$WORK/harfbuzz"
meson_static "$WORK/harfbuzz" -Dfreetype=enabled -Dcoretext=enabled -Dglib=disabled \
  -Dgobject=disabled -Dicu=disabled -Dcairo=disabled -Dtests=disabled -Ddocs=disabled \
  -Dbenchmark=disabled -Dutilities=disabled

echo "==> FriBidi $FRIBIDI_TAG"
clone https://github.com/fribidi/fribidi "$FRIBIDI_TAG" "$FRIBIDI_COMMIT" "$WORK/fribidi"
meson_static "$WORK/fribidi" -Dbin=false -Dtests=false -Ddocs=false

echo "==> libass $LIBASS_TAG"
clone https://github.com/libass/libass "$LIBASS_TAG" "$LIBASS_COMMIT" "$WORK/libass"
(
  cd "$WORK/libass"
  ./autogen.sh
  ./configure --prefix="$PREFIX" --libdir="$PREFIX/lib" --disable-shared --enable-static \
    --with-pic --disable-fontconfig --enable-coretext --disable-require-system-font-provider \
    --disable-asm
  make -j"$JOBS"
  make install
)

echo "==> FFmpeg $FFMPEG_TAG (LGPL, static)"
clone https://github.com/FFmpeg/FFmpeg "$FFMPEG_TAG" "$FFMPEG_COMMIT" "$WORK/ffmpeg"
(
  cd "$WORK/ffmpeg"
  ./configure --prefix="$PREFIX" --libdir="$PREFIX/lib" --arch="$ARCH" \
    --extra-cflags="$CFLAGS" --extra-ldflags="$LDFLAGS" \
    --disable-gpl --disable-nonfree --disable-version3 \
    --disable-programs --disable-doc --disable-debug \
    --disable-shared --enable-static --enable-pic \
    --disable-autodetect \
    --enable-zlib --enable-videotoolbox --enable-audiotoolbox --enable-libdav1d \
    --disable-network --disable-encoders --disable-muxers
  make -j"$JOBS"
  make install
)
grep -q 'define FFMPEG_LICENSE "LGPL version 2.1 or later"' "$WORK/ffmpeg/config.h" \
  || { echo "error: FFmpeg did not configure as LGPL 2.1 or later" >&2; exit 1; }

echo "==> libplacebo $PLACEBO_TAG"
git clone --quiet --depth 1 --branch "$PLACEBO_TAG" --recurse-submodules --shallow-submodules \
  https://github.com/haasn/libplacebo "$WORK/libplacebo"
[[ "$(git -C "$WORK/libplacebo" rev-parse HEAD)" == "$PLACEBO_COMMIT" ]] \
  || { echo "error: libplacebo $PLACEBO_TAG moved" >&2; exit 1; }
meson_static "$WORK/libplacebo" -Ddemos=false -Dtests=false -Dbench=false -Dvulkan=disabled \
  -Dshaderc=disabled -Dglslang=disabled -Dlcms=disabled -Dd3d11=disabled -Ddovi=disabled \
  -Dlibdovi=disabled -Dopengl=enabled -Dgl-proc-addr=enabled -Dunwind=disabled -Dxxhash=disabled

echo "==> mpv $MPV_TAG (libmpv only, -Dgpl=false)"
clone https://github.com/mpv-player/mpv "$MPV_TAG" "$MPV_COMMIT" "$WORK/mpv"
meson setup "$WORK/mpv/build" "$WORK/mpv" --prefix="$PREFIX" --libdir=lib --buildtype=release \
  -Dlibmpv=true -Dcplayer=false -Dgpl=false -Dtests=false -Dmanpage-build=disabled \
  -Dlua=disabled -Djavascript=disabled -Dlcms2=disabled -Dvulkan=disabled \
  -Dlibarchive=disabled -Drubberband=disabled -Dzimg=disabled -Duchardet=disabled \
  -Dzlib=disabled -Dgl=enabled -Dcocoa=enabled -Dgl-cocoa=enabled -Dmacos-cocoa-cb=disabled \
  -Dmacos-media-player=disabled -Dmacos-touchbar=disabled -Dswift-build=enabled \
  -Dcoreaudio=enabled -Dvideotoolbox-gl=enabled
ninja -C "$WORK/mpv/build" install

LIB="$(find "$PREFIX/lib" -name 'libmpv.2.dylib' -type f | head -1)"
[[ -n "$LIB" ]] || { echo "error: libmpv.2.dylib was not produced" >&2; find "$PREFIX/lib" -name 'libmpv*' >&2; exit 1; }

echo "==> verify"
install -m 644 "$LIB" "$OUT/libmpv.2.dylib"
install_name_tool -id "@rpath/libmpv.2.dylib" "$OUT/libmpv.2.dylib"
otool -L "$OUT/libmpv.2.dylib"
if otool -L "$OUT/libmpv.2.dylib" | tail -n +2 | grep -vE '^\s*(/usr/lib/|/System/Library/|@rpath/libmpv)'; then
  echo "error: libmpv depends on a library outside the system" >&2
  exit 1
fi
lipo -archs "$OUT/libmpv.2.dylib"
nm -gU "$OUT/libmpv.2.dylib" | grep '_mpv_create$' >/dev/null || { echo "error: mpv_create is not exported" >&2; exit 1; }
nm -gU "$OUT/libmpv.2.dylib" | grep '_mpv_render_context_create$' >/dev/null \
  || { echo "error: the render API is not exported" >&2; exit 1; }
strip -x "$OUT/libmpv.2.dylib"

{
  echo "arch: $ARCH  deployment target: $MACOSX_DEPLOYMENT_TARGET"
  echo "mpv $MPV_TAG ($MPV_COMMIT) -Dgpl=false"
  echo "FFmpeg $FFMPEG_TAG ($FFMPEG_COMMIT) LGPL 2.1 or later, static"
  echo "libplacebo $PLACEBO_TAG ($PLACEBO_COMMIT) static"
  echo "libass $LIBASS_TAG ($LIBASS_COMMIT) static"
  echo "FreeType $FREETYPE_TAG ($FREETYPE_COMMIT) static"
  echo "HarfBuzz $HARFBUZZ_TAG ($HARFBUZZ_COMMIT) static"
  echo "FriBidi $FRIBIDI_TAG ($FRIBIDI_COMMIT) static"
  echo "dav1d $DAV1D_TAG ($DAV1D_COMMIT) static"
  echo "Xcode: $(xcodebuild -version | tr '\n' ' ')"
  echo "--- system dependencies"
  otool -L "$OUT/libmpv.2.dylib" | tail -n +2
  echo "--- FFmpeg configuration"
  grep -E '^#define (FFMPEG_CONFIGURATION|FFMPEG_LICENSE)' "$WORK/ffmpeg/config.h"
} > "$OUT/BUILD-INFO.txt"
# License texts of the statically linked components, for the archive.
mkdir -p "$OUT/licenses"
cp "$WORK/mpv/LICENSE.LGPL" "$OUT/licenses/mpv.LGPL"
cp "$WORK/ffmpeg/COPYING.LGPLv2.1" "$OUT/licenses/ffmpeg.LGPL"
cp "$WORK/libplacebo/LICENSE" "$OUT/licenses/libplacebo.LGPL"
cp "$WORK/libass/COPYING" "$OUT/licenses/libass.ISC"
cp "$WORK/dav1d/COPYING" "$OUT/licenses/dav1d.BSD-2-Clause"
cp "$WORK/fribidi/COPYING" "$OUT/licenses/fribidi.LGPL"
cp "$WORK/freetype/docs/FTL.TXT" "$OUT/licenses/freetype.FTL"
cp "$WORK/harfbuzz/COPYING" "$OUT/licenses/harfbuzz.MIT"
cat "$OUT/BUILD-INFO.txt"
