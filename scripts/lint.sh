#!/usr/bin/env bash
# Run the same checks that .github/workflows/ci.yml runs, locally, so a PR can
# be checked before pushing. Mirrors the `check` job: fmt, clippy, test.
#
# Usage:
#   scripts/lint.sh            # check only (matches CI)
#   scripts/lint.sh --fix      # cargo fmt (write) first, then the checks
#   scripts/lint.sh --no-test  # skip the test step (lint only)
set -uo pipefail

cd "$(dirname "$0")/.."

FIX=0
RUN_TESTS=1
for arg in "$@"; do
  case "$arg" in
    --fix) FIX=1 ;;
    --no-test) RUN_TESTS=0 ;;
    *) echo "unknown arg: $arg" >&2; exit 2 ;;
  esac
done

fail=0
step() {
  local name="$1"
  shift
  echo "== $name =="
  if "$@"; then
    echo "-- $name: ok"
  else
    echo "!! $name: FAILED"
    fail=1
  fi
  echo
}

[[ $FIX -eq 1 ]] && step "cargo fmt (write)" cargo fmt --all

step "cargo fmt --check" cargo fmt --all --check
step "cargo clippy" cargo clippy --workspace --all-targets -- -D warnings
[[ $RUN_TESTS -eq 1 ]] && step "cargo test" cargo test --workspace

if [[ $fail -eq 0 ]]; then
  echo "All checks passed."
else
  echo "Checks FAILED. Run 'scripts/lint.sh --fix' to auto-format, then fix remaining clippy/test issues."
fi
exit $fail
