#!/usr/bin/env bash
# SPDX-License-Identifier: MIT

# Production line-coverage gate for mlxtop.
#
# Runs every unit test under cargo-llvm-cov and fails when production line
# coverage drops below COVERAGE_MIN_LINES (default: 90).
#
# Denominator policy:
#   * All tests live in dedicated files under src/tests/ (each production
#     module declares `#[cfg(test)] #[path = "tests/<module>.rs"] mod tests;`).
#   * TEST_FILE_REGEX below excludes exactly those test files and nothing
#     else. No production file, function, or line is excluded, and no
#     `cfg(coverage)` switches are used.
#   * The gate refuses to run if a production file uses `cfg(coverage)`,
#     `coverage(off)` or `cfg(not(test))`, or has a `#[cfg(test)]` item other
#     than a `#[path = "tests/..."]` test-module declaration, so test-only
#     code cannot hide inside the measured files.
#   * The build uses `--no-cfg-coverage`, so the measured binary is compiled
#     exactly like `cargo test`.
#   * Code behind `cfg(target_os = "macos")` is only measured when the gate
#     runs on macOS; run the gate on each supported platform.
#
# Outputs (in COVERAGE_DIR, default target/coverage):
#   coverage.json   llvm-cov JSON export (production files only)
#   summary.txt     per-file region/function/line table
#   uncovered.txt   uncovered production lines per file
#
# Requirements: cargo-llvm-cov (`cargo install cargo-llvm-cov --locked`) and
# the llvm-tools component (`rustup component add llvm-tools-preview`).

set -Eeuo pipefail
IFS=$'\n\t'

SCRIPT_DIR="$(cd -- "$(dirname -- "$0")" && pwd -P)"
PROJECT_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd -P)"
readonly SCRIPT_DIR PROJECT_ROOT

readonly TEST_FILE_REGEX='(^|/)src/tests/[^/]+\.rs$'
min_lines="${COVERAGE_MIN_LINES:-90}"
out_dir="${COVERAGE_DIR:-$PROJECT_ROOT/target/coverage}"

if ! cargo llvm-cov --version >/dev/null 2>&1; then
  echo "error: cargo-llvm-cov is not installed" >&2
  echo "  cargo install cargo-llvm-cov --locked" >&2
  echo "  rustup component add llvm-tools-preview" >&2
  exit 2
fi

cd "$PROJECT_ROOT"

check_denominator() {
  local status=0 file
  for file in src/*.rs; do
    if grep -nE 'cfg\((not\()?coverage|coverage\(off\)|cfg\(not\(test\)\)' "$file"; then
      echo "error: $file disables code under coverage or outside tests" >&2
      status=1
    fi
    if ! awk -v file="$file" '
      pending { if ($0 !~ /^#\[path = "tests\/[^"]+\.rs"\]$/) { bad = 1; print file ":" NR - 1 ": #[cfg(test)] item outside src/tests/" } pending = 0 }
      /^[[:space:]]*#\[cfg\(test\)\]/ { pending = 1 }
      END { exit bad }
    ' "$file"; then
      status=1
    fi
  done
  return "$status"
}

if ! check_denominator; then
  echo "error: move test-only code into src/tests/ instead" >&2
  exit 2
fi

mkdir -p "$out_dir"
cargo llvm-cov clean --workspace
cargo llvm-cov --locked --all-targets --no-cfg-coverage --no-report

report() {
  cargo llvm-cov report --ignore-filename-regex "$TEST_FILE_REGEX" "$@"
}

report --json --output-path "$out_dir/coverage.json"
report --summary-only | tee "$out_dir/summary.txt"
report --show-missing-lines --summary-only >"$out_dir/uncovered.txt"

echo "Coverage reports written to $out_dir"
report --summary-only --fail-under-lines "$min_lines" >/dev/null
echo "Production line coverage meets the ${min_lines}% gate"
