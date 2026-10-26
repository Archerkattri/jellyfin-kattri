#!/usr/bin/env bash
# Downloads the pinned libmpv archive for one platform, verifies its SHA-256, and copies
# the shared library to target/libmpv/<platform>/. Run only when a build needs libmpv;
# a plain check never calls it.
#
# Usage: packaging/libmpv/fetch.sh <windows-x64|windows-arm64|macos-universal|linux-x64>
# Output: the path of the verified library on stdout. Messages go to stderr.
#
# Refuses (exit 1) when the platform is not pinned in manifest.toml, when a field is
# missing or malformed, when the URL is not https, or when the SHA-256 does not match.
# The manifest is read with awk because these scripts have no TOML parser; the values
# must stay in the flat form described at the top of manifest.toml.

set -euo pipefail

usage() {
  echo "usage: $0 <windows-x64|windows-arm64|macos-universal|linux-x64>" >&2
  exit 2
}

[[ $# -eq 1 ]] || usage
PLATFORM="$1"
case "$PLATFORM" in
  windows-x64 | windows-arm64 | macos-universal | linux-x64) ;;
  *) usage ;;
esac

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
MANIFEST="$HERE/manifest.toml"

# Prints the value of KEY in the [platforms.$PLATFORM] table, or nothing when absent.
pin_value() {
  awk -v table="platforms.$PLATFORM" -v key="$1" '
    /^[[:space:]]*#/ { next }
    /^[[:space:]]*\[/ {
      name = $0
      gsub(/[[:space:]]/, "", name)
      section = (name == "[" table "]") ? 1 : 0
      next
    }
    section && $0 ~ ("^[[:space:]]*" key "[[:space:]]*=") {
      sub(/^[^=]*=[[:space:]]*"/, "")
      sub(/"[[:space:]]*$/, "")
      print
      exit
    }
  ' "$MANIFEST"
}

STATUS="$(pin_value status)"
if [[ "$STATUS" != "pinned" ]]; then
  echo "error: libmpv for $PLATFORM is not pinned: $(pin_value todo)" >&2
  exit 1
fi

URL="$(pin_value url)"
ARCHIVE="$(pin_value archive_name)"
MEMBER="$(pin_value library_member)"
SHA="$(pin_value sha256 | tr 'A-F' 'a-f')"

[[ "$URL" == https://* ]] || { echo "error: url for $PLATFORM is not https; refusing" >&2; exit 1; }
[[ "$SHA" =~ ^[0-9a-f]{64}$ ]] || { echo "error: sha256 for $PLATFORM is not 64 hex digits; refusing" >&2; exit 1; }
[[ "$ARCHIVE" =~ ^[A-Za-z0-9._-]+$ ]] || { echo "error: archive_name for $PLATFORM must be a bare file name; refusing" >&2; exit 1; }
[[ -n "$MEMBER" && "$MEMBER" != /* && "$MEMBER" != *..* && "$MEMBER" != *\\* ]] \
  || { echo "error: library_member for $PLATFORM must be a relative path inside the archive; refusing" >&2; exit 1; }

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  else
    shasum -a 256 "$1" | awk '{print $1}'
  fi
}

OUT_DIR="$ROOT/target/libmpv/$PLATFORM"
mkdir -p "$OUT_DIR"
ARCHIVE_PATH="$OUT_DIR/$ARCHIVE"

if [[ -f "$ARCHIVE_PATH" ]] && [[ "$(sha256_of "$ARCHIVE_PATH")" == "$SHA" ]]; then
  echo "Using verified archive: $ARCHIVE_PATH" >&2
else
  rm -f "$ARCHIVE_PATH"
  PART="$ARCHIVE_PATH.part"
  echo "Downloading $ARCHIVE" >&2
  # --proto-redir keeps redirects on https as well, so a redirect cannot downgrade the transfer.
  curl --fail --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --silent --show-error -o "$PART" "$URL"
  ACTUAL="$(sha256_of "$PART")"
  if [[ "$ACTUAL" != "$SHA" ]]; then
    rm -f "$PART"
    echo "error: SHA-256 mismatch for $ARCHIVE (expected $SHA, got $ACTUAL); nothing was extracted" >&2
    exit 1
  fi
  mv "$PART" "$ARCHIVE_PATH"
fi

STAGE="$(mktemp -d "$OUT_DIR/extract.XXXXXX")"
trap 'rm -rf "$STAGE"' EXIT

case "$ARCHIVE" in
  *.zip)
    unzip -q "$ARCHIVE_PATH" -d "$STAGE"
    ;;
  *.tar.gz | *.tgz)
    tar -xzf "$ARCHIVE_PATH" -C "$STAGE"
    ;;
  *.7z)
    SEVENZ="$(command -v 7z || command -v 7za || command -v 7zz || true)"
    [[ -n "$SEVENZ" ]] || { echo "error: 7z is required to extract $ARCHIVE" >&2; exit 1; }
    "$SEVENZ" x -y "-o$STAGE" "$ARCHIVE_PATH" >/dev/null
    ;;
  *)
    echo "error: unsupported archive format for $ARCHIVE" >&2
    exit 1
    ;;
esac

SRC="$STAGE/$MEMBER"
if [[ ! -f "$SRC" ]]; then
  echo "error: library_member '$MEMBER' was not found in $ARCHIVE" >&2
  exit 1
fi
DEST="$OUT_DIR/$(basename "$MEMBER")"
install -m 644 "$SRC" "$DEST"
echo "$DEST"
