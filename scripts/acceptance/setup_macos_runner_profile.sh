#!/usr/bin/env bash
set -euo pipefail

: "${PROFILE_NAME:?set PROFILE_NAME to a dedicated acceptance-* profile name}"
if [[ ! "$PROFILE_NAME" =~ ^acceptance-[a-z0-9-]{1,21}$ ]]; then
  echo "refusing profile name outside the acceptance-* policy" >&2
  exit 2
fi
if [[ "${GITHUB_ACTIONS:-}" != "true" || "${RUNNER_OS:-}" != "macOS" ||
      -z "${RUNNER_TEMP:-}" || ! -d "$RUNNER_TEMP" ]]; then
  echo "first-run profile setup is restricted to an ephemeral GitHub macOS runner" >&2
  exit 2
fi

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
profiles_dir="$HOME/Library/Application Support/org.Jellyfin.Kattri/profiles"
if [[ -e "$profiles_dir" ]]; then
  echo "runner profile directory already exists; refusing to initialize unknown state" >&2
  exit 2
fi

cargo run --locked \
  --manifest-path "$repo_root/scripts/acceptance/profile_tool/Cargo.toml" \
  --target-dir "$RUNNER_TEMP/kattri-profile-tool-target" \
  -- initialize-macos-runner "$profiles_dir" "$PROFILE_NAME"
