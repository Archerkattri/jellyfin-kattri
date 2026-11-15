#!/usr/bin/env bash
set -euo pipefail

: "${APP_PATH:?set APP_PATH to the built app bundle}"
: "${PROFILE_NAME:?set PROFILE_NAME to the acceptance profile}"
: "${GITHUB_SHA:?expected a GitHub Actions source revision}"
: "${RUNNER_OS:?expected a GitHub-hosted runner}"

if [[ "${GITHUB_ACTIONS:-}" != "true" || "$RUNNER_OS" != "macOS" ||
      -z "${RUNNER_TEMP:-}" || ! -d "$RUNNER_TEMP" ]]; then
  echo "evidence recording is restricted to the ephemeral GitHub macOS runner" >&2
  exit 2
fi
if [[ ! "$PROFILE_NAME" =~ ^acceptance-[a-z0-9-]{1,21}$ ]]; then
  echo "refusing a profile name outside the acceptance-* policy" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
app_path="$(cd "$(dirname "$APP_PATH")" && pwd)/$(basename "$APP_PATH")"
executable="$app_path/Contents/MacOS/jellyfin-kattri"
libmpv="$app_path/Contents/Frameworks/libmpv.2.dylib"
evidence_dir="$repo_root/runtime-evidence/macos"
profiles_dir="$HOME/Library/Application Support/org.Jellyfin.Kattri/profiles"
profile_index="$profiles_dir/index.toml"

if [[ "$app_path" != "$repo_root/dist/macos/Jellyfin Kattri.app" ||
      ! -x "$executable" || ! -s "$libmpv" ]]; then
  echo "the expected packaged executable or bundled libmpv is missing" >&2
  exit 1
fi
mkdir -p "$evidence_dir"

if pgrep -x jellyfin-kattri >/dev/null 2>&1; then
  echo "the packaged app must exit before profile postflight verification" >&2
  exit 1
fi
if [[ ! -f "$profile_index" ]]; then
  echo "the macOS runner profile index is missing after the scheme check" >&2
  exit 1
fi
cargo run --locked \
  --manifest-path "$repo_root/scripts/acceptance/profile_tool/Cargo.toml" \
  --target-dir "$RUNNER_TEMP/kattri-macos-profile-verify-target" \
  -- verify "$profiles_dir" "$PROFILE_NAME" default \
  | tee "$evidence_dir/profile-postflight.txt"
if ! grep -Eq '^default[[:space:]]*=[[:space:]]*"default"$' "$profile_index" ||
   ! grep -Eq '^active[[:space:]]*=[[:space:]]*"default"$' "$profile_index"; then
  echo "the saved default and active profile pointers changed during URL delivery" >&2
  exit 1
fi

app_bytes="$(du -sk "$app_path" | awk '{printf "%.0f", $1 * 1024}')"
file_count="$(find "$app_path" -type f | wc -l | tr -d '[:space:]')"
if [[ ! "$app_bytes" =~ ^[0-9]+$ || "$app_bytes" -gt 1000000000 ||
      ! "$file_count" =~ ^[0-9]+$ || "$file_count" -gt 10000 ]]; then
  echo "the app bundle is outside the evidence size bounds" >&2
  exit 1
fi

exe_bytes="$(stat -f '%z' "$executable")"
mpv_bytes="$(stat -f '%z' "$libmpv")"
exe_sha="$(shasum -a 256 "$executable" | awk '{print $1}')"
mpv_sha="$(shasum -a 256 "$libmpv" | awk '{print $1}')"
cat > "$evidence_dir/evidence.txt" <<EOF
kind=runtime-acceptance-evidence
source_commit=$GITHUB_SHA
runner_os=$RUNNER_OS
runner_arch=$(uname -m)
check=packaged universal macOS app bundle and Jellyfin URL delivery
profile=$PROFILE_NAME
saved_default_profile=default
saved_active_profile=default
profile_postflight=verified_after_app_exit
url_delivery=pass
app_bundle_size_bytes=$app_bytes
app_bundle_file_count=$file_count
app_executable_size_bytes=$exe_bytes
app_executable_sha256=$exe_sha
bundled_libmpv_size_bytes=$mpv_bytes
bundled_libmpv_sha256=$mpv_sha
audio_output=unobserved
server_access=not_measured
network_activity=not_captured
EOF

evidence_bytes="$(wc -c < "$evidence_dir/evidence.txt" | tr -d '[:space:]')"
evidence_total_bytes="$(du -sk "$evidence_dir" | awk '{printf "%.0f", $1 * 1024}')"
evidence_file_count="$(find "$evidence_dir" -type f | wc -l | tr -d '[:space:]')"
if [[ "$evidence_bytes" -gt 32768 || "$evidence_total_bytes" -gt 1048576 ||
      "$evidence_file_count" -gt 8 ]]; then
  echo "macOS evidence exceeded its size bound" >&2
  exit 1
fi
cat "$evidence_dir/evidence.txt"
