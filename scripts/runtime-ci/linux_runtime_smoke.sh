#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 2 ]]; then
  echo "usage: $0 <AppImage> <debug-host-executable>" >&2
  exit 2
fi
: "${GITHUB_SHA:?expected a GitHub Actions source revision}"

if [[ "${GITHUB_ACTIONS:-}" != "true" || "${RUNNER_OS:-}" != "Linux" ||
      -z "${RUNNER_TEMP:-}" || ! -d "$RUNNER_TEMP" || -z "${DISPLAY:-}" ]]; then
  echo "runtime smoke is restricted to an ephemeral GitHub Linux display runner" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
appimage="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
debug_host="$(cd "$(dirname "$2")" && pwd)/$(basename "$2")"
expected_appimage="$repo_root/dist/linux/$(basename "$appimage")"
if [[ "$appimage" != "$expected_appimage" || ! -x "$appimage" ||
      "$debug_host" != "$repo_root/target/debug/jellyfin-kattri" || ! -x "$debug_host" ]]; then
  echo "runtime smoke received an unexpected package or debug executable" >&2
  exit 1
fi

evidence_dir="$repo_root/runtime-evidence/linux"
if [[ -e "$evidence_dir" ]]; then
  echo "evidence directory already exists; refusing stale-run output" >&2
  exit 2
fi
mkdir -p "$evidence_dir"

export XDG_CONFIG_HOME="$RUNNER_TEMP/kattri-runtime-config"
export XDG_DATA_HOME="$RUNNER_TEMP/kattri-runtime-data"
export XDG_CACHE_HOME="$RUNNER_TEMP/kattri-runtime-cache"
export XDG_STATE_HOME="$RUNNER_TEMP/kattri-runtime-state"
profile_name=acceptance-linux-ci
profile_tool_target="$RUNNER_TEMP/kattri-runtime-profile-target"
cargo run --locked --offline \
  --manifest-path "$repo_root/scripts/runtime-ci/profile_tool/Cargo.toml" \
  --target-dir "$profile_tool_target" -- initialize-linux-runner "$profile_name"

fixture="$RUNNER_TEMP/kattri-synthetic-frame.ppm"
python3 "$repo_root/scripts/runtime-ci/make_synthetic_ppm.py" "$fixture" \
  | tee "$RUNNER_TEMP/kattri-fixture-generation.txt"

unset JELLYFIN_TEST_URL JELLYFIN_TEST_USER JELLYFIN_TEST_PASSWORD \
  KATTRI_DEBUG_URL KATTRI_DEBUG_USER KATTRI_DEBUG_PASSWORD || true
package_app_log="$RUNNER_TEMP/kattri-appimage.log"
package_window_log="$RUNNER_TEMP/kattri-appimage-window.txt"
TMPDIR=/tmp \
KATTRI_SMOKE_EXPECT_APPIMAGE_MOUNT=1 \
KATTRI_SMOKE_LOG="$package_app_log" \
  "$repo_root/packaging/linux/smoke-launch.sh" "$appimage" \
  --profile "$profile_name" > "$package_window_log"

appdir_root="$RUNNER_TEMP/kattri-appimage-content"
if [[ -e "$appdir_root" ]]; then
  echo "AppImage extraction directory already exists; refusing stale-run output" >&2
  exit 2
fi
mkdir -p "$appdir_root"
(
  cd "$appdir_root"
  "$appimage" --appimage-extract >/dev/null
)
appdir="$appdir_root/squashfs-root"
bundled_mpv="$appdir/usr/lib/libmpv.so.2"
bundled_jpeg="$appdir/usr/lib/libjpeg.so.8"
if [[ ! -s "$bundled_mpv" || ! -s "$bundled_jpeg" ]]; then
  echo "AppImage is missing its bundled media libraries" >&2
  exit 1
fi

fixture_dir="$RUNNER_TEMP/kattri-frame-capture"
if [[ -e "$fixture_dir" ]]; then
  echo "frame capture directory already exists; refusing stale-run output" >&2
  exit 2
fi
mkdir -p "$fixture_dir"
frame_loader_log="$RUNNER_TEMP/kattri-frame-loader.log"
frame_profile_log="$RUNNER_TEMP/kattri-frame-host.log"
window_manager_pid=""
app_pid=""

cleanup() {
  if [[ -n "$app_pid" ]] && kill -0 "$app_pid" 2>/dev/null; then
    window_id="$(xdotool search --onlyvisible --name '^Jellyfin Kattri$' 2>/dev/null | head -n 1 || true)"
    if [[ -n "$window_id" ]]; then
      wmctrl -i -c "$(printf '0x%x' "$window_id")" >/dev/null 2>&1 || true
    fi
    for _ in $(seq 1 10); do
      kill -0 "$app_pid" 2>/dev/null || break
      sleep 0.5
    done
    if kill -0 "$app_pid" 2>/dev/null; then
      kill "$app_pid" 2>/dev/null || true
    fi
    wait "$app_pid" 2>/dev/null || true
  fi
  if [[ -n "$window_manager_pid" ]] && kill -0 "$window_manager_pid" 2>/dev/null; then
    kill "$window_manager_pid" 2>/dev/null || true
    wait "$window_manager_pid" 2>/dev/null || true
  fi
}
trap cleanup EXIT

openbox --sm-disable >/dev/null 2>&1 &
window_manager_pid=$!
for _ in $(seq 1 20); do
  wmctrl -m >/dev/null 2>&1 && break
  sleep 0.25
done
if ! wmctrl -m >/dev/null 2>&1; then
  echo "software-render window manager did not start" >&2
  exit 1
fi

export KATTRI_DEBUG_DIR="$fixture_dir"
export KATTRI_DEBUG_STREAM_ONLY=1
export KATTRI_LOG=error
export LIBGL_ALWAYS_SOFTWARE=1
export LD_LIBRARY_PATH="$appdir/usr/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
LD_DEBUG=libs "$debug_host" --profile "$profile_name" "$fixture" \
  > "$frame_loader_log" 2>&1 &
app_pid=$!

window_id=""
for _ in $(seq 1 60); do
  if ! kill -0 "$app_pid" 2>/dev/null; then
    echo "debug host exited before creating a window" >&2
    tail -n 50 "$frame_loader_log" >&2 || true
    exit 1
  fi
  window_id="$(xdotool search --onlyvisible --name '^Jellyfin Kattri$' 2>/dev/null | head -n 1 || true)"
  [[ -n "$window_id" ]] && break
  sleep 0.5
done
if [[ -z "$window_id" ]]; then
  echo "debug host did not create a visible window" >&2
  tail -n 50 "$frame_loader_log" >&2 || true
  exit 1
fi

debug_log="$fixture_dir/kattri-debug.log"
media_ready=0
for _ in $(seq 1 60); do
  if [[ -f "$debug_log" ]] && grep -Fq 'Media is loaded in libmpv' "$debug_log"; then
    media_ready=1
    break
  fi
  if ! kill -0 "$app_pid" 2>/dev/null; then
    echo "debug host exited before libmpv loaded the synthetic frame" >&2
    tail -n 50 "$frame_loader_log" >&2 || true
    exit 1
  fi
  sleep 0.5
done
if [[ "$media_ready" -ne 1 ]]; then
  echo "libmpv did not load the synthetic frame within 30 seconds" >&2
  exit 1
fi

frame_check_log="$RUNNER_TEMP/kattri-frame-check.txt"
frame_ok=0
for _ in $(seq 1 20); do
  rm -f -- "$fixture_dir/video-screenshot.png"
  touch "$fixture_dir/screenshot.request"
  for _ in $(seq 1 10); do
    [[ -s "$fixture_dir/video-screenshot.png" ]] && break
    sleep 0.1
  done
  if [[ -s "$fixture_dir/video-screenshot.png" ]] &&
     python3 "$repo_root/scripts/runtime-ci/verify_video_frame.py" \
       "$fixture_dir/video-screenshot.png" > "$frame_check_log" 2>&1; then
    frame_ok=1
    break
  fi
  sleep 0.25
done
if [[ "$frame_ok" -ne 1 ]]; then
  cat "$frame_check_log" >&2 || true
  echo "no nonblank synthetic video frame was verified" >&2
  exit 1
fi

if ! grep -Fq "calling init: $bundled_mpv" "$frame_loader_log"; then
  echo "debug host did not initialize libmpv from the extracted AppImage payload" >&2
  exit 1
fi
frame_loader_bytes="$(stat -c '%s' "$frame_loader_log")"
if [[ "$frame_loader_bytes" -gt 50000000 ]]; then
  echo "dynamic-loader evidence exceeded the 50 MB bound" >&2
  exit 1
fi

gl_renderer="$(LIBGL_ALWAYS_SOFTWARE=1 glxinfo -B 2>/dev/null |
  awk -F: '/OpenGL renderer string:/ {sub(/^[[:space:]]+/, "", $2); print $2; exit}')"
if [[ -z "$gl_renderer" ]] || ! grep -Eqi 'llvmpipe|softpipe|swrast' <<< "$gl_renderer"; then
  echo "the frame check did not report a Mesa software renderer" >&2
  exit 1
fi

window_id="$(xdotool search --onlyvisible --name '^Jellyfin Kattri$' 2>/dev/null | head -n 1 || true)"
if [[ -n "$window_id" ]]; then
  wmctrl -i -c "$(printf '0x%x' "$window_id")"
fi
for _ in $(seq 1 30); do
  if ! kill -0 "$app_pid" 2>/dev/null; then
    break
  fi
  sleep 0.5
done
if kill -0 "$app_pid" 2>/dev/null; then
  echo "debug host did not exit after a normal window close" >&2
  exit 1
fi
wait "$app_pid"
app_pid=""

cargo run --locked --offline \
  --manifest-path "$repo_root/scripts/runtime-ci/profile_tool/Cargo.toml" \
  --target-dir "$profile_tool_target" -- verify-linux-runner "$profile_name" \
  | tee "$frame_profile_log"

cp "$fixture_dir/video-screenshot.png" "$evidence_dir/video-screenshot.png"
cat "$frame_check_log" > "$evidence_dir/frame-check.txt"

appimage_bytes="$(stat -c '%s' "$appimage")"
appimage_sha="$(sha256sum "$appimage" | awk '{print $1}')"
mpv_bytes="$(stat -c '%s' "$bundled_mpv")"
mpv_sha="$(sha256sum "$bundled_mpv" | awk '{print $1}')"
fixture_bytes="$(stat -c '%s' "$fixture")"
fixture_sha="$(sha256sum "$fixture" | awk '{print $1}')"
frame_bytes="$(stat -c '%s' "$evidence_dir/video-screenshot.png")"
frame_sha="$(sha256sum "$evidence_dir/video-screenshot.png" | awk '{print $1}')"
if [[ "$appimage_bytes" -gt 1000000000 || "$mpv_bytes" -gt 500000000 ||
      "$fixture_bytes" -gt 1000000 || "$frame_bytes" -gt 16777216 ]]; then
  echo "one or more runtime inputs/evidence files exceeded their size bounds" >&2
  exit 1
fi

cat > "$evidence_dir/evidence.txt" <<EOF
kind=runtime-acceptance-evidence
source_commit=$GITHUB_SHA
runner_os=$RUNNER_OS
runner_arch=$(uname -m)
package_format=AppImage (ephemeral smoke build; not a release)
appimage_size_bytes=$appimage_bytes
appimage_sha256=$appimage_sha
acceptance_profile=$profile_name
saved_default_profile=default
saved_active_profile=default
appimage_window_close=pass
appimage_mount_libmpv_initialization=pass
appimage_mount_libjpeg_initialization=pass
extracted_bundle_libmpv_initialization=pass
extracted_bundled_libmpv_size_bytes=$mpv_bytes
extracted_bundled_libmpv_sha256=$mpv_sha
software_gl_renderer=$gl_renderer
synthetic_fixture=generated P6 color pattern, no audio track
synthetic_fixture_size_bytes=$fixture_bytes
synthetic_fixture_sha256=$fixture_sha
video_frame_readback=pass
video_frame_size_bytes=$frame_bytes
video_frame_sha256=$frame_sha
audio_output=unobserved
server_access=not_measured
network_activity=not_captured
EOF

evidence_total_bytes="$(du -sb "$evidence_dir" | awk '{print $1}')"
if [[ "$evidence_total_bytes" -gt 20000000 ]]; then
  echo "Linux runtime evidence exceeded its 20 MB artifact bound" >&2
  exit 1
fi
cat "$evidence_dir/evidence.txt"
cat "$evidence_dir/frame-check.txt"
