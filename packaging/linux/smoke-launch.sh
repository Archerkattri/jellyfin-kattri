#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || -z "${KATTRI_SMOKE_LOG:-}" ]]; then
  echo "usage: KATTRI_SMOKE_LOG=<path> $0 <application> [arguments...]" >&2
  exit 2
fi
for tool in xdotool xwininfo; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "Linux window smoke requires $tool" >&2
    exit 2
  fi
done

# Exact paths are used for installed packages. AppImage uses a separate mount-only check because
# the runtime's FUSE mount path is unique per launch and must never match a host /usr/lib path.
MPV_PATH="${KATTRI_SMOKE_EXPECT_LIBMPV_PATH:-}"
JPEG_PATH="${KATTRI_SMOKE_EXPECT_LIBJPEG_PATH:-}"
SANDBOX_LIB_PATH="${KATTRI_SMOKE_EXPECT_SANDBOX_LIB_PATH:-}"
TRACE_LOADER="${KATTRI_SMOKE_TRACE_LOADER:-1}"
APPIMAGE_MOUNT_CHECK="${KATTRI_SMOKE_EXPECT_APPIMAGE_MOUNT:-0}"
if [[ "$APPIMAGE_MOUNT_CHECK" != "0" && "$APPIMAGE_MOUNT_CHECK" != "1" ]]; then
  echo "KATTRI_SMOKE_EXPECT_APPIMAGE_MOUNT must be 0 or 1" >&2
  exit 2
fi
if [[ "$APPIMAGE_MOUNT_CHECK" == "1" && ( -n "$MPV_PATH" || -n "$JPEG_PATH" || -n "$SANDBOX_LIB_PATH" ) ]]; then
  echo "AppImage mount verification cannot be combined with exact library paths" >&2
  exit 2
fi
if [[ -n "$SANDBOX_LIB_PATH" && ( -n "$MPV_PATH" || -n "$JPEG_PATH" ) ]]; then
  echo "Flatpak sandbox verification cannot be combined with host library paths" >&2
  exit 2
fi
if [[ -n "$SANDBOX_LIB_PATH" && "$SANDBOX_LIB_PATH" != /* ]]; then
  echo "KATTRI_SMOKE_EXPECT_SANDBOX_LIB_PATH must be an absolute sandbox path" >&2
  exit 2
fi
if [[ "$APPIMAGE_MOUNT_CHECK" == "1" && "$TRACE_LOADER" != "1" ]]; then
  echo "AppImage mount verification requires loader tracing" >&2
  exit 2
fi

# Xvfb starts without a window manager. wmctrl can only request a normal close
# through one, and destroying the X window instead makes winit panic, so start an
# EWMH window manager whenever wmctrl is the close mechanism.
WM_PID=""
if command -v wmctrl >/dev/null 2>&1; then
  if ! command -v openbox >/dev/null 2>&1; then
    echo "wmctrl needs a window manager; install openbox" >&2
    exit 2
  fi
  openbox --sm-disable >/dev/null 2>&1 &
  WM_PID=$!
  for _ in $(seq 1 20); do
    if wmctrl -m >/dev/null 2>&1; then
      break
    fi
    sleep 0.5
  done
  if ! wmctrl -m >/dev/null 2>&1; then
    echo "window manager did not start" >&2
    exit 1
  fi
fi

export LIBGL_ALWAYS_SOFTWARE=1
if [[ ( -n "$MPV_PATH" || -n "$JPEG_PATH" || "$APPIMAGE_MOUNT_CHECK" == "1" ) && "$TRACE_LOADER" == "1" ]]; then
  LD_DEBUG=libs "$@" >"$KATTRI_SMOKE_LOG" 2>&1 &
else
  "$@" >"$KATTRI_SMOKE_LOG" 2>&1 &
fi
APP_PID=$!
cleanup() {
  if kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
  fi
  if [[ -n "$WM_PID" ]] && kill -0 "$WM_PID" 2>/dev/null; then
    kill "$WM_PID" 2>/dev/null || true
    wait "$WM_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

WINDOW_ID=""
for _ in $(seq 1 60); do
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    echo "application exited before creating a window" >&2
    wait "$APP_PID"
    exit 1
  fi
  WINDOW_ID="$(xdotool search --onlyvisible --name '^Jellyfin Kattri$' 2>/dev/null | head -n 1 || true)"
  if [[ -n "$WINDOW_ID" ]]; then
    break
  fi
  sleep 1
done
if [[ -z "$WINDOW_ID" ]]; then
  echo "application did not create a visible Jellyfin Kattri window within 60 seconds" >&2
  exit 1
fi

xwininfo -id "$WINDOW_ID"
if [[ -n "$WM_PID" ]]; then
  wmctrl -i -c "$(printf '0x%x' "$WINDOW_ID")"
else
  xdotool windowquit "$WINDOW_ID"
fi
for _ in $(seq 1 15); do
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    wait "$APP_PID"
    trap - EXIT
    if [[ -n "$WM_PID" ]] && kill -0 "$WM_PID" 2>/dev/null; then
      kill "$WM_PID" 2>/dev/null || true
      wait "$WM_PID" 2>/dev/null || true
    fi
    for library_path in "$MPV_PATH" "$JPEG_PATH"; do
      if [[ -z "$library_path" ]]; then
        continue
      fi
      expected_path="$(realpath -e -- "$library_path")"
      initialized_path=""
      while IFS= read -r loader_line; do
        [[ "$loader_line" == *"calling init: "* ]] || continue
        candidate_path="${loader_line#*calling init: }"
        if [[ -e "$candidate_path" ]] && [[ "$(realpath -e -- "$candidate_path")" == "$expected_path" ]]; then
          initialized_path="$candidate_path"
          break
        fi
      done <"$KATTRI_SMOKE_LOG"
      if [[ -z "$initialized_path" ]]; then
        echo "application did not initialize the expected bundled library: $library_path" >&2
        tail -n 80 "$KATTRI_SMOKE_LOG" >&2
        exit 1
      fi
    done
    if [[ -n "$SANDBOX_LIB_PATH" ]]; then
      # /app exists only in the Flatpak namespace, so never resolve it on the host.
      if ! awk -v expected="$SANDBOX_LIB_PATH" '
        { sub(/^[[:space:]]*([0-9]+:[[:space:]]*)?/, "") }
        $0 == "calling init: " expected { found = 1 }
        END { exit !found }
      ' "$KATTRI_SMOKE_LOG"; then
        echo "application did not initialize the exact Flatpak sandbox library: $SANDBOX_LIB_PATH" >&2
        tail -n 80 "$KATTRI_SMOKE_LOG" >&2
        exit 1
      fi
    fi
    if [[ -n "$MPV_PATH" ]]; then
      echo "application initialized bundled libmpv from $MPV_PATH"
    fi
    if [[ -n "$JPEG_PATH" ]]; then
      echo "application initialized bundled libjpeg from $JPEG_PATH"
    fi
    if [[ "$APPIMAGE_MOUNT_CHECK" == "1" ]]; then
      for library_pattern in \
        'calling init: /tmp/\.mount_[^/]+/usr/lib/libmpv\.so\.2$' \
        'calling init: /tmp/\.mount_[^/]+/usr/lib/libjpeg\.so\.8$'; do
        if ! grep -Eq "$library_pattern" "$KATTRI_SMOKE_LOG"; then
          echo "AppImage did not initialize the expected library from its /tmp mount: $library_pattern" >&2
          tail -n 80 "$KATTRI_SMOKE_LOG" >&2
          exit 1
        fi
      done
      echo "application initialized bundled libmpv and libjpeg from the AppImage mount"
    fi
    echo "application window closed and process exited normally"
    exit 0
  fi
  sleep 1
done

echo "application did not exit after its window was asked to close" >&2
exit 1
