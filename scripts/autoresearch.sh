#!/usr/bin/env bash
# Autoresearch harness: measures code conciseness of src/ after Rust upgrades.
# Gates: cargo test must pass; metric only counts when the suite is green.
set -euo pipefail
cd "$(dirname "$0")/.."

echo "== build =="
cargo build --quiet

echo "== tests (gate) =="
TEST_OUT=$(cargo test --quiet 2>&1)
echo "$TEST_OUT"
TEST_COUNT=$(awk '/test result: ok/ {s+=$4} END {print s+0}' <<<"$TEST_OUT")

echo "== clippy (gate, must be warning-free) =="
cargo clippy --all-targets --quiet -- -D warnings

echo "== metrics =="
cargo build --release --quiet
BINARY_BYTES=$(wc -c < target/release/aic | tr -d ' ')
echo "METRIC binary_bytes=$BINARY_BYTES"
SRC_LOC=$(find src -name '*.rs' -print0 | xargs -0 wc -l | tail -1 | awk '{print $1}')
echo "METRIC src_loc=$SRC_LOC"
echo "METRIC test_count=$TEST_COUNT"
echo "METRIC rust_minor=$(rustc --version | awk '{print $2}' | cut -d. -f2)"
