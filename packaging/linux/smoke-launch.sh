#!/usr/bin/env bash
set -euo pipefail

if [[ $# -lt 1 || -z "${KATTRI_SMOKE_LOG:-}" ]]; then
  echo "usage: KATTRI_SMOKE_LOG=<path> $0 <application> [arguments...]" >&2
  exit 2
fi

export LIBGL_ALWAYS_SOFTWARE=1
"$@" >"$KATTRI_SMOKE_LOG" 2>&1 &
APP_PID=$!
cleanup() {
  if kill -0 "$APP_PID" 2>/dev/null; then
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
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
xdotool windowclose "$WINDOW_ID"
for _ in $(seq 1 15); do
  if ! kill -0 "$APP_PID" 2>/dev/null; then
    wait "$APP_PID"
    trap - EXIT
    echo "application window closed and process exited normally"
    exit 0
  fi
  sleep 1
done

echo "application did not exit after its window was asked to close" >&2
exit 1
