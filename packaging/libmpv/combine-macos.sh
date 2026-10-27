#!/usr/bin/env bash
# Merges the arm64 and x86_64 libmpv.2.dylib built by build-macos.sh into one universal archive.
#
# Usage: combine-macos.sh <arm64-dir> <x86_64-dir> <out-dir>
# Output: <out-dir>/kattri-libmpv-macos-universal-<mpv tag>.tar.gz and its .sha256
set -euo pipefail
ARM="${1:?arm64 dir}"; X86="${2:?x86_64 dir}"; OUT="$(mkdir -p "${3:?out dir}" && cd "$3" && pwd)"
TAG="$(sed -n 's/^mpv \(v[0-9.]*\) .*/\1/p' "$ARM/BUILD-INFO.txt" | head -1)"
[[ -n "$TAG" && "$TAG" == "$(sed -n 's/^mpv \(v[0-9.]*\) .*/\1/p' "$X86/BUILD-INFO.txt" | head -1)" ]] \
  || { echo "error: the two builds are not the same mpv version" >&2; exit 1; }
NAME="kattri-libmpv-macos-universal-$TAG"
STAGE="$(mktemp -d)"; trap 'rm -rf "$STAGE"' EXIT
mkdir -p "$STAGE/$NAME"
lipo -create -output "$STAGE/$NAME/libmpv.2.dylib" "$ARM/libmpv.2.dylib" "$X86/libmpv.2.dylib"
install_name_tool -id "@rpath/libmpv.2.dylib" "$STAGE/$NAME/libmpv.2.dylib"
ARCHS="$(lipo -archs "$STAGE/$NAME/libmpv.2.dylib")"
[[ " $ARCHS " == *" arm64 "* && " $ARCHS " == *" x86_64 "* ]] || { echo "error: archs are '$ARCHS'" >&2; exit 1; }
cp -R "$ARM/licenses" "$STAGE/$NAME/licenses"
{ echo "== arm64"; cat "$ARM/BUILD-INFO.txt"; echo; echo "== x86_64"; cat "$X86/BUILD-INFO.txt"; } > "$STAGE/$NAME/BUILD-INFO.txt"
tar -C "$STAGE" -cf - "$NAME" | gzip -9n > "$OUT/$NAME.tar.gz"
(cd "$OUT" && shasum -a 256 "$NAME.tar.gz" | tee "$NAME.tar.gz.sha256")
echo "architectures: $ARCHS"
