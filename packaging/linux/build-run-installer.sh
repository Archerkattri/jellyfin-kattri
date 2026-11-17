#!/usr/bin/env bash
# Builds Jellyfin Kattri as one self-extracting Linux installer (.run).
#
# Usage: packaging/linux/build-run-installer.sh <version> <x64-appimage> <arm64-appimage> <output-dir>
#   <version>         Release version in X.Y.Z form, for example 0.7.0.
#   <x64-appimage>    AppImage built for x86_64 by packaging/linux/appimage/build-appimage.sh.
#   <arm64-appimage>  AppImage built for aarch64 by the same script on an aarch64 host, or - when no aarch64 build is published.
#   <output-dir>      Directory that receives the installer, for example dist/linux.
#
# Output:
#   <output-dir>/JellyfinKattri-<version>-linux.run
#
# The output is a POSIX sh header followed by a gzip-compressed tar payload holding
# both AppImages (JellyfinKattri-x64.AppImage and JellyfinKattri-arm64.AppImage). The
# header embeds the line number where the payload starts, so it extracts itself with
# `tail -n +<line> "$file" | tar -xz` after resolving its own path.
#
# Auto-architecture: no separate x64 and arm64 installers are produced. The header
# reads the target machine with `uname -m` at install time: x86_64 installs the x64
# AppImage, aarch64 and arm64 install the arm64 AppImage, and every other machine is
# refused with a clear error before anything is written. One download therefore
# serves both architectures.
#
# The header installs per user under $HOME/.local by default, or system wide under
# /opt, /usr/local/bin and /usr/share/applications when run as root with --system.
# It supports --uninstall for the same scope. It never uses the network and never
# escalates privileges: without --system it writes only below $HOME, and --system
# requires that it already runs as root.
#
# This script must run on Linux (GNU tar and sh). It reads the two AppImages and only
# writes the single .run file; the inputs are not modified.
#
# The generated header is checked with `sh -n`, its payload offset is checked by
# listing the archive through that offset, and --help and unknown-argument exit codes
# are checked by running the finished installer.

set -euo pipefail

usage() {
  echo "usage: $0 <version> <x64-appimage> <arm64-appimage|-> <output-dir>" >&2
  exit 2
}

[[ $# -eq 4 ]] || usage
VERSION="$1"
X64_APPIMAGE="$2"
ARM64_APPIMAGE="$3"
OUT_DIR="$4"

if ! [[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]]; then
  echo "error: version '$VERSION' is not X.Y.Z, for example 0.7.0" >&2
  exit 2
fi
if [[ "$(uname -s)" != "Linux" ]]; then
  echo "error: build-run-installer.sh must run on Linux (GNU tar, sh)" >&2
  exit 1
fi

check_appimage() {
  local label="$1" path="$2"
  if [[ ! -f "$path" ]]; then
    echo "error: $label AppImage not found: $path" >&2
    exit 1
  fi
  if [[ ! -s "$path" ]]; then
    echo "error: $label AppImage is empty: $path" >&2
    exit 1
  fi
  if [[ "$(head -c 4 "$path")" != $'\x7fELF' ]]; then
    echo "error: $label AppImage is not an ELF executable: $path" >&2
    exit 1
  fi
}

check_appimage x64 "$X64_APPIMAGE"
ARM64_PRESENT=0
if [[ "$ARM64_APPIMAGE" != "-" ]]; then
  check_appimage arm64 "$ARM64_APPIMAGE"
  ARM64_PRESENT=1
fi

WORK="$(mktemp -d "${TMPDIR:-/tmp}/kattri-run-installer.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

PAYLOAD_X64="JellyfinKattri-x64.AppImage"
PAYLOAD_ARM64="JellyfinKattri-arm64.AppImage"
PAYLOAD_DIR="$WORK/payload"
mkdir -p "$PAYLOAD_DIR"
install -m 755 "$X64_APPIMAGE" "$PAYLOAD_DIR/$PAYLOAD_X64"
if [[ "$ARM64_PRESENT" -eq 1 ]]; then
  install -m 755 "$ARM64_APPIMAGE" "$PAYLOAD_DIR/$PAYLOAD_ARM64"
fi

echo "==> compressing payload"
TAR_MEMBERS=("$PAYLOAD_X64")
if [[ "$ARM64_PRESENT" -eq 1 ]]; then
  TAR_MEMBERS+=("$PAYLOAD_ARM64")
fi
tar -czf "$WORK/payload.tar.gz" -C "$PAYLOAD_DIR" "${TAR_MEMBERS[@]}"

# The header is written verbatim and is never expanded by this script, so the
# placeholders below are the only values substituted into it.
cat > "$WORK/header.tmpl" <<'KATTRI_HEADER'
#!/bin/sh
# Jellyfin Kattri single-file Linux installer.
#
# This file is a POSIX sh header followed by a gzip-compressed tar payload that holds
# one AppImage per architecture. The header reads the machine architecture from
# `uname -m`, chooses the matching AppImage, and installs it for the current user by
# default, or system wide with --system. It never uses the network and never
# escalates privileges: --system requires that it already runs as root.
#
# The payload starts on the line named by PAYLOAD_LINE. The header resolves its own
# path first, so it works when the file is placed anywhere and when it is reached
# through a symbolic link, then extracts the payload from that path.
#
# Exit codes: 0 success, 1 failure, 2 usage error.

set -eu

VERSION="@VERSION@"
PAYLOAD_LINE="@PAYLOAD_LINE@"
HAS_ARM64="@HAS_ARM64@"

usage_text() {
  cat <<'USAGE'
usage: JellyfinKattri-<version>-linux.run [--system] [--uninstall]

Installs the Jellyfin Kattri AppImage for this machine.

  (no option)   install for the current user under $HOME/.local
  --system      install under /opt for every user (must run as root)
  --uninstall   remove an installation of the same scope
  -h, --help    show this message

Exit codes: 0 success, 1 failure, 2 usage error.
USAGE
}

usage_error() {
  usage_text >&2
  exit 2
}

fail() {
  echo "error: $*" >&2
  exit 1
}

# Resolves $0 to the absolute path of this file, following symbolic links, so the
# payload can be read from wherever the file was placed.
resolve_self() {
  self_path=$0
  case "$self_path" in
    */*) ;;
    *) self_path=$(command -v "$self_path" 2>/dev/null || true) ;;
  esac
  if [ -z "$self_path" ]; then
    fail "cannot locate the installer file; run it with an explicit path"
  fi
  hops=0
  while [ -L "$self_path" ]; do
    hops=$((hops + 1))
    if [ "$hops" -gt 40 ]; then
      fail "too many symbolic links while resolving the installer path"
    fi
    if ! command -v readlink >/dev/null 2>&1; then
      fail "readlink is required to resolve a symbolic link installer"
    fi
    self_dir=$(unset CDPATH; cd -P "$(dirname "$self_path")" && pwd)
    self_path=$(readlink "$self_path")
    case "$self_path" in
      /*) ;;
      *) self_path=$self_dir/$self_path ;;
    esac
  done
  self_dir=$(unset CDPATH; cd -P "$(dirname "$self_path")" && pwd)
  SELF=$self_dir/$(basename "$self_path")
  if [ ! -f "$SELF" ] || [ ! -r "$SELF" ]; then
    fail "installer file is not a readable regular file: $SELF"
  fi
}

detect_arch() {
  machine=$(uname -m 2>/dev/null || true)
  case "$machine" in
    x86_64)
      ARCH=x64
      PAYLOAD_FILE=JellyfinKattri-x64.AppImage
      ;;
    aarch64|arm64)
      ARCH=arm64
      if [ "$HAS_ARM64" != "1" ]; then
        fail "this installer has no aarch64 build; use the x86_64 build or build from source"
      fi
      PAYLOAD_FILE=JellyfinKattri-arm64.AppImage
      ;;
    "")
      fail "cannot detect the machine architecture; uname -m returned nothing"
      ;;
    *)
      fail "unsupported architecture '$machine'; this installer carries an x86_64 build only"
      ;;
  esac
  echo "==> detected architecture $machine, using the $ARCH AppImage"
}

extract_payload() {
  case "$PAYLOAD_LINE" in
    ''|*[!0-9]*)
      fail "this installer was built without a valid payload offset"
      ;;
  esac
  if ! command -v tar >/dev/null 2>&1; then
    fail "tar is required to extract the payload"
  fi
  if ! command -v mktemp >/dev/null 2>&1; then
    fail "mktemp is required to create a temporary directory"
  fi
  WORK_DIR=$(mktemp -d "${TMPDIR:-/tmp}/jellyfin-kattri.XXXXXX")
  trap 'if [ -n "${WORK_DIR:-}" ]; then rm -rf "$WORK_DIR"; fi' EXIT
  echo "==> extracting $PAYLOAD_FILE from the installer"
  # A truncated or damaged file makes tar fail, which is the payload integrity check.
  # Archive ownership is never applied: the extracted file is copied into place below,
  # and root cannot chown inside a container or user namespace.
  if ! tail -n +"$PAYLOAD_LINE" "$SELF" | tar --no-same-owner -xzf - -C "$WORK_DIR"; then
    fail "failed to extract the payload; the installer file may be incomplete or damaged"
  fi
  if [ ! -s "$WORK_DIR/$PAYLOAD_FILE" ]; then
    fail "the payload does not contain $PAYLOAD_FILE"
  fi
}

install_app() {
  resolve_self
  detect_arch
  extract_payload
  echo "==> installing Jellyfin Kattri $VERSION into $OPT_DIR"
  mkdir -p "$OPT_DIR" "$DESKTOP_DIR" "$(dirname "$BIN_LINK")"
  # Copy beside the destination and rename, so an AppImage that is running from the
  # old path is replaced atomically instead of being written through.
  cp "$WORK_DIR/$PAYLOAD_FILE" "$APPIMAGE.new"
  chmod 755 "$APPIMAGE.new"
  mv -f "$APPIMAGE.new" "$APPIMAGE"
  # The command name must stay stable across versions, so it is a symbolic link.
  if [ -e "$BIN_LINK" ] && [ ! -L "$BIN_LINK" ]; then
    fail "refusing to overwrite the existing file $BIN_LINK; remove it first"
  fi
  rm -f "$BIN_LINK"
  ln -s "$APPIMAGE" "$BIN_LINK"
  desktop_new=$DESKTOP_FILE.new
  cat > "$desktop_new" <<DESKTOP
[Desktop Entry]
Type=Application
Version=1.4
Name=Jellyfin Kattri
GenericName=Media client
Comment=Native Jellyfin client
Exec="$BIN_LINK" %u
Terminal=false
Categories=AudioVideo;Video;Player;
StartupNotify=true
MimeType=x-scheme-handler/jellyfin;
DESKTOP
  chmod 644 "$desktop_new"
  mv -f "$desktop_new" "$DESKTOP_FILE"
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
  fi
  echo "==> installed $APPIMAGE"
  echo "==> linked $BIN_LINK"
  echo "==> wrote $DESKTOP_FILE"
  if [ "$system_install" -eq 0 ]; then
    case ":${PATH:-}:" in
      *":$HOME/.local/bin:"*) ;;
      *) echo "note: $HOME/.local/bin is not on PATH; add it to use the jellyfin-kattri command" ;;
    esac
  fi
  echo "==> done"
}

uninstall_app() {
  case "$OPT_DIR" in
    ""|/)
      fail "refusing to remove an unsafe installation path: '$OPT_DIR'"
      ;;
  esac
  removed=0
  if [ -f "$DESKTOP_FILE" ] || [ -L "$DESKTOP_FILE" ]; then
    rm -f "$DESKTOP_FILE"
    removed=1
  fi
  if [ -L "$BIN_LINK" ] || [ -f "$BIN_LINK" ]; then
    rm -f "$BIN_LINK"
    removed=1
  fi
  if [ -d "$OPT_DIR" ]; then
    rm -rf "$OPT_DIR"
    removed=1
  fi
  if [ "$removed" -eq 0 ]; then
    echo "==> no installation found for this scope"
    return 0
  fi
  if command -v update-desktop-database >/dev/null 2>&1; then
    update-desktop-database "$DESKTOP_DIR" >/dev/null 2>&1 || true
  fi
  echo "==> removed the installation from $OPT_DIR"
}

main() {
  mode=install
  system_install=0
  while [ $# -gt 0 ]; do
    case "$1" in
      --system)
        system_install=1
        ;;
      --uninstall)
        mode=uninstall
        ;;
      -h|--help)
        usage_text
        exit 0
        ;;
      *)
        echo "error: unknown argument '$1'" >&2
        usage_error
        ;;
    esac
    shift
  done
  if [ "$system_install" -eq 1 ]; then
    if [ "$(id -u)" -ne 0 ]; then
      fail "--system writes to /opt and /usr/local/bin and requires root; re-run it with sudo"
    fi
    OPT_DIR=/opt/jellyfin-kattri
    BIN_LINK=/usr/local/bin/jellyfin-kattri
    DESKTOP_DIR=/usr/share/applications
  else
    if [ -z "${HOME:-}" ]; then
      fail "HOME is not set; cannot choose the per-user installation directory"
    fi
    OPT_DIR=$HOME/.local/opt/jellyfin-kattri
    BIN_LINK=$HOME/.local/bin/jellyfin-kattri
    DESKTOP_DIR=$HOME/.local/share/applications
  fi
  APPIMAGE=$OPT_DIR/jellyfin-kattri.AppImage
  DESKTOP_FILE=$DESKTOP_DIR/jellyfin-kattri.desktop
  if [ "$mode" = uninstall ]; then
    uninstall_app
  else
    install_app
  fi
}

main "$@"
exit 0
# The gzip-compressed tar payload follows on the next line.
KATTRI_HEADER

# The payload line number is baked into the header before the payload is appended.
# Substituting a number, a version or a path never changes the header line count.
if [[ -n "$(tail -c 1 "$WORK/header.tmpl")" ]]; then
  echo "error: the generated installer header does not end with a newline" >&2
  exit 1
fi
sed -e "s|@VERSION@|$VERSION|" "$WORK/header.tmpl" > "$WORK/header.versioned"
HEADER_LINES="$(wc -l < "$WORK/header.versioned")"
PAYLOAD_LINE=$((HEADER_LINES + 1))
sed -e "s|@PAYLOAD_LINE@|$PAYLOAD_LINE|" -e "s|@HAS_ARM64@|$ARM64_PRESENT|" "$WORK/header.versioned" > "$WORK/header.sh"
FINAL_LINES="$(wc -l < "$WORK/header.sh")"
if [[ "$FINAL_LINES" -ne "$HEADER_LINES" ]]; then
  echo "error: the installer header changed length while substituting the payload offset" >&2
  exit 1
fi
if ! sh -n "$WORK/header.sh"; then
  echo "error: the generated installer header is not valid POSIX sh" >&2
  exit 1
fi

OUT="$OUT_DIR/JellyfinKattri-$VERSION-linux.run"
mkdir -p "$OUT_DIR"
rm -f "$OUT"
cat "$WORK/header.sh" "$WORK/payload.tar.gz" > "$OUT"
chmod 755 "$OUT"

# Check the payload through the offset that the header will use at install time.
ENTRIES="$WORK/payload-entries.txt"
tail -n +"$PAYLOAD_LINE" "$OUT" | tar -tzf - > "$ENTRIES"
for name in "$PAYLOAD_X64" "$PAYLOAD_ARM64"; do
  if ! grep -Fxq "$name" "$ENTRIES"; then
    echo "error: payload offset is wrong; the archive does not list $name" >&2
    exit 1
  fi
done

# The header runs before the payload, so these checks never touch the AppImages.
if ! sh "$OUT" --help > /dev/null 2>&1; then
  echo "error: the generated installer failed its --help check" >&2
  exit 1
fi
set +e
sh "$OUT" --not-a-real-option > /dev/null 2>&1
SELF_CHECK_STATUS=$?
set -e
if [[ "$SELF_CHECK_STATUS" -ne 2 ]]; then
  echo "error: the generated installer exited $SELF_CHECK_STATUS on an unknown option instead of 2" >&2
  exit 1
fi

echo "==> built $OUT"
echo "$OUT"
