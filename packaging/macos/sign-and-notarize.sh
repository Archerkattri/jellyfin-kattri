#!/usr/bin/env bash
# Signs, notarizes and staples a macOS artifact (.app or .dmg).
#
# Usage: packaging/macos/sign-and-notarize.sh <path-to-app-or-dmg>
#
# Environment (set only in CI, from repository secrets; never commit values):
#   DEVELOPER_ID    Signing identity name, for example
#                   "Developer ID Application: Example Name (TEAMID)".
#                   When unset, the script prints that the artifact is unsigned and
#                   exits 0. Unsigned artifacts are a supported output.
#   NOTARY_PROFILE  Name of a notarytool keychain profile created in the CI job with
#                   `xcrun notarytool store-credentials`. Required when DEVELOPER_ID
#                   is set. Without it the script exits 1 rather than ship a signed
#                   artifact that was never notarized.
#
# Order for a release: build-app.sh, then this script on the .app, then make-dmg.sh,
# then this script on the .dmg. A .app is submitted inside a zip made with ditto;
# a .dmg is submitted as is. Both are stapled after acceptance.
#
# Signing uses the hardened runtime (--options runtime) and a secure timestamp.
# Nested Mach-O files (Contents/MacOS, Contents/Frameworks) are signed before the
# bundle itself. --deep is not used.

set -euo pipefail

usage() {
  echo "usage: $0 <path-to-app-or-dmg>" >&2
  exit 2
}

[[ $# -eq 1 ]] || usage
ARTIFACT="$1"

if [[ ! -e "$ARTIFACT" ]]; then
  echo "error: '$ARTIFACT' does not exist" >&2
  exit 1
fi

case "$ARTIFACT" in
  *.app | *.dmg) ;;
  *)
    echo "error: '$ARTIFACT' must be an .app bundle or a .dmg image" >&2
    exit 2
    ;;
esac

if [[ -z "${DEVELOPER_ID:-}" ]]; then
  echo "UNSIGNED: DEVELOPER_ID is not set. $ARTIFACT is not signed and not notarized."
  exit 0
fi

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "error: signing requires macOS (codesign, notarytool, stapler)" >&2
  exit 1
fi

if [[ -z "${NOTARY_PROFILE:-}" ]]; then
  echo "error: DEVELOPER_ID is set but NOTARY_PROFILE is not. Refusing to sign without notarizing." >&2
  exit 1
fi

sign_path() {
  local path="$1"
  codesign --force --options runtime --timestamp --sign "$DEVELOPER_ID" "$path"
}

if [[ "$ARTIFACT" == *.app ]]; then
  echo "==> signing nested code in $ARTIFACT"
  while IFS= read -r -d '' nested; do
    sign_path "$nested"
  done < <(find "$ARTIFACT/Contents/Frameworks" "$ARTIFACT/Contents/MacOS" \
    -type f \( -name '*.dylib' -o -perm -u+x \) -print0 2>/dev/null)

  echo "==> signing bundle $ARTIFACT"
  sign_path "$ARTIFACT"
  codesign --verify --strict --verbose=2 "$ARTIFACT"

  SUBMIT_PATH="$(mktemp -d "${TMPDIR:-/tmp}/kattri-notary.XXXXXX")/$(basename "$ARTIFACT").zip"
  ditto -c -k --keepParent "$ARTIFACT" "$SUBMIT_PATH"
else
  echo "==> signing disk image $ARTIFACT"
  sign_path "$ARTIFACT"
  codesign --verify --verbose=2 "$ARTIFACT"
  SUBMIT_PATH="$ARTIFACT"
fi

echo "==> submitting $SUBMIT_PATH to the notary service"
NOTARY_JSON="$(mktemp "${TMPDIR:-/tmp}/kattri-notary-json.XXXXXX")"
set +e
xcrun notarytool submit "$SUBMIT_PATH" \
  --keychain-profile "$NOTARY_PROFILE" \
  --wait \
  --output-format json > "$NOTARY_JSON"
SUBMIT_STATUS=$?
set -e

SUBMIT_ID="$(sed -nE 's/.*"id"[[:space:]]*:[[:space:]]*"([^"]+)".*/\1/p' "$NOTARY_JSON" | head -n 1)"

if [[ $SUBMIT_STATUS -ne 0 ]] || ! grep -Eq '"status"[[:space:]]*:[[:space:]]*"Accepted"' "$NOTARY_JSON"; then
  echo "error: notarization was not accepted (exit $SUBMIT_STATUS, submission ${SUBMIT_ID:-unknown})" >&2
  if [[ -n "$SUBMIT_ID" ]]; then
    xcrun notarytool log "$SUBMIT_ID" --keychain-profile "$NOTARY_PROFILE" || true
  fi
  rm -f "$NOTARY_JSON"
  exit 1
fi
rm -f "$NOTARY_JSON"
echo "    notarization accepted (submission $SUBMIT_ID)"

echo "==> stapling $ARTIFACT"
xcrun stapler staple "$ARTIFACT"
xcrun stapler validate "$ARTIFACT"

if [[ "$ARTIFACT" == *.app ]]; then
  rm -rf "$(dirname "$SUBMIT_PATH")"
  spctl --assess --type execute --verbose=2 "$ARTIFACT" || true
fi

echo "==> signed and notarized $ARTIFACT"
