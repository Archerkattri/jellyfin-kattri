#!/usr/bin/env bash
# Format, lint, and test gate for the Rust workspace.
# Runs every step even if an earlier one fails, then prints a summary.
# Exit 0 only when all steps pass.

set -u

repo_root="$(cd "$(dirname "$0")/.." && pwd)" || exit 1
cd "$repo_root" || exit 1

names=()
codes=()

run_step() {
    local name="$1"
    shift
    echo
    echo "==> [$name] $*"
    "$@"
    local rc=$?
    names+=("$name")
    codes+=("$rc")
}

run_step "fmt" cargo fmt --all -- --check
run_step "clippy" cargo clippy --workspace --all-targets -- -D warnings
run_step "test" cargo test --workspace

echo
echo "================ check summary ================"
overall=0
for i in "${!names[@]}"; do
    if [ "${codes[$i]}" -eq 0 ]; then
        printf 'PASS  %s\n' "${names[$i]}"
    else
        printf 'FAIL  %s (exit %s)\n' "${names[$i]}" "${codes[$i]}"
        overall=1
    fi
done
echo "==============================================="
if [ "$overall" -eq 0 ]; then
    echo "RESULT: PASS"
else
    echo "RESULT: FAIL"
fi
exit "$overall"
