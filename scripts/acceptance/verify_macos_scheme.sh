#!/usr/bin/env bash
set -euo pipefail

: "${APP_PATH:?set APP_PATH to the packaged Jellyfin Kattri.app path}"
: "${PROFILE_NAME:?set PROFILE_NAME to a pre-existing acceptance-* profile}"

if [[ ! "$PROFILE_NAME" =~ ^acceptance-[a-z0-9-]{1,21}$ ]]; then
  echo "refusing a profile name outside the acceptance-* policy" >&2
  exit 2
fi

APP_PATH="$(cd "$(dirname "$APP_PATH")" && pwd)/$(basename "$APP_PATH")"
EXECUTABLE="$APP_PATH/Contents/MacOS/jellyfin-kattri"
PROFILE_ROOT="$HOME/Library/Application Support/org.Jellyfin.Kattri/profiles"
PROFILE_SETTINGS="$PROFILE_ROOT/$PROFILE_NAME/settings.toml"
PROFILE_INDEX="$PROFILE_ROOT/index.toml"
LOG_FILE="$HOME/Library/Application Support/org.Jellyfin.Kattri/logs/kattri.log"
BUNDLE_ID='org.kattri.jellyfin'
app_started=0

cleanup() {
  if [[ "$app_started" -eq 1 ]] && pgrep -x jellyfin-kattri >/dev/null 2>&1; then
    osascript -e "tell application id \"$BUNDLE_ID\" to quit" >/dev/null 2>&1 || true
    for _ in $(seq 1 40); do
      if ! pgrep -x jellyfin-kattri >/dev/null 2>&1; then
        return
      fi
      sleep 0.25
    done
    echo "Kattri did not quit after the normal AppleScript request; close it manually" >&2
  fi
}
trap cleanup EXIT

if [[ ! -d "$APP_PATH" || ! -x "$EXECUTABLE" ]]; then
  echo "packaged app or executable is missing" >&2
  exit 2
fi
profiles_block=''
if [[ -f "$PROFILE_INDEX" ]]; then
  profiles_block="$(sed -n '/^profiles[[:space:]]*=[[:space:]]*\[/,/^[[:space:]]*\]/p' "$PROFILE_INDEX")"
fi
if [[ ! -f "$PROFILE_SETTINGS" || -z "$profiles_block" ]] ||
   ! printf '%s\n' "$profiles_block" | grep -Eq "^[[:space:]]*\"${PROFILE_NAME}\"([,[:space:]]|$)"; then
  echo "pre-existing acceptance profile is missing or not indexed" >&2
  exit 2
fi
if ! grep -Eq '^default[[:space:]]*=[[:space:]]*"default"$' "$PROFILE_INDEX" ||
   ! grep -Eq '^active[[:space:]]*=[[:space:]]*"default"$' "$PROFILE_INDEX"; then
  echo "refusing to run unless the saved default and active profiles remain default" >&2
  exit 2
fi
if pgrep -x jellyfin-kattri >/dev/null 2>&1; then
  echo "refusing to target an existing Jellyfin Kattri process" >&2
  exit 2
fi

baseline_bytes=0
if [[ -f "$LOG_FILE" ]]; then
  baseline_bytes="$(wc -c < "$LOG_FILE" | tr -d '[:space:]')"
fi

# This placeholder URL only pre-fills the review intent; it is never authenticated or played.
item_url='jellyfin://kattri/v1/item/0123456789abcdef0123456789abcdef?server=https%3A%2F%2Fmedia.example'
open -n -a "$APP_PATH" --args --profile "$PROFILE_NAME"
app_started=1

app_ready=0
for _ in $(seq 1 60); do
  if pgrep -x jellyfin-kattri >/dev/null 2>&1; then
    app_ready=1
    break
  fi
  sleep 0.25
done
if [[ "$app_ready" -ne 1 ]]; then
  echo "packaged app did not appear as a running process" >&2
  exit 1
fi

registration_marker='registered macOS Jellyfin URL Apple event handler'
registration_ready=0
for _ in $(seq 1 80); do
  if [[ -f "$LOG_FILE" ]] && tail -c +$((baseline_bytes + 1)) "$LOG_FILE" | grep -Fq "$registration_marker"; then
    registration_ready=1
    break
  fi
  sleep 0.25
done
if [[ "$registration_ready" -ne 1 ]]; then
  echo "FAIL: no new Apple-event handler registration marker appeared in the product log" >&2
  exit 1
fi

open "$item_url"
marker='accepted macOS Jellyfin URL Apple event'
for _ in $(seq 1 80); do
  if [[ -f "$LOG_FILE" ]] && tail -c +$((baseline_bytes + 1)) "$LOG_FILE" | grep -Fq "$marker"; then
    echo "PASS: packaged app accepted the Jellyfin URL Apple event"
    exit 0
  fi
  sleep 0.25
done

echo "FAIL: no new Apple-event acceptance marker appeared in the product log" >&2
exit 1
