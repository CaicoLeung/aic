#!/usr/bin/env bash
# Unit tests for the pure helpers in scripts/release_lib.sh.
# Run: bash scripts/test_release.sh
#
# Covers the parts of the release guard whose correctness is not obvious from
# reading: version normalization, Cargo.toml parsing, and version format
# validation. The git-state checks are exercised manually via --dry-run.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck shell=bash
# shellcheck source=release_lib.sh
source "${SCRIPT_DIR}/release_lib.sh"

passed=0
failed=0

assert_eq() {
  local name="$1" expected="$2" actual="$3"
  if [[ "$expected" == "$actual" ]]; then
    echo "  ok   ${name}"
    passed=$((passed + 1))
  else
    echo "  FAIL ${name}: expected '${expected}', got '${actual}'"
    failed=$((failed + 1))
  fi
}

assert_valid() {
  local name="$1" v="$2"
  if is_valid_version "$v"; then
    echo "  ok   ${name}"
    passed=$((passed + 1))
  else
    echo "  FAIL ${name}: '${v}' should be valid"
    failed=$((failed + 1))
  fi
}

assert_invalid() {
  local name="$1" v="$2"
  if ! is_valid_version "$v"; then
    echo "  ok   ${name}"
    passed=$((passed + 1))
  else
    echo "  FAIL ${name}: '${v}' should be invalid"
    failed=$((failed + 1))
  fi
}

echo "normalize_version"
assert_eq "strips a leading v"   "0.1.6" "$(normalize_version v0.1.6)"
assert_eq "no-op when no v"      "0.1.6" "$(normalize_version 0.1.6)"
assert_eq "empty stays empty"    ""      "$(normalize_version '')"

echo "parse_cargo_version"
fixture="$(mktemp -t aic-release-test.XXXXXX)"
trap 'rm -f "$fixture"' EXIT
cat > "$fixture" <<'EOF'
[package]
name = "aic"
version = "0.2.3"
edition = "2024"

[dist]
cargo-dist-version = "0.31.0"
EOF
assert_eq "reads package version, ignores cargo-dist-version" "0.2.3" \
  "$(parse_cargo_version "$fixture")"

echo "is_valid_version"
assert_valid   "plain x.y.z"              "0.1.5"
assert_valid   "large x.y.z"              "10.20.30"
assert_valid   "prerelease suffix"        "0.1.5-rc.1"
assert_valid   "build metadata"           "0.1.5+build.7"
assert_invalid "four numeric segments"    "0.1.5.7"
assert_invalid "missing patch"            "1.2"
assert_invalid "major only"               "2"
assert_invalid "non-numeric"              "abc"
assert_invalid "empty"                    ""

echo "is_version_higher"
# Dedicated boolean asserts: assert_valid/assert_invalid wrap is_valid_version
# and can't be reused for is_version_higher's result.
assert_higher() {
  local name="$1" a="$2" b="$3"
  if is_version_higher "$a" "$b"; then
    echo "  ok   ${name}"; passed=$((passed + 1))
  else
    echo "  FAIL ${name}: expected ${a} > ${b}"; failed=$((failed + 1))
  fi
}
assert_not_higher() {
  local name="$1" a="$2" b="$3"
  if ! is_version_higher "$a" "$b"; then
    echo "  ok   ${name}"; passed=$((passed + 1))
  else
    echo "  FAIL ${name}: expected ${a} NOT > ${b}"; failed=$((failed + 1))
  fi
}
assert_higher     "patch bump"                                 "0.1.6"      "0.1.5"
assert_higher     "minor bump"                                 "0.2.0"      "0.1.9"
assert_higher     "major bump"                                 "1.0.0"      "0.9.9"
assert_higher     "prerelease over prior stable"               "0.1.6-rc.1" "0.1.5"
assert_not_higher "lower patch rejected"                       "0.1.5"      "0.1.6"
assert_not_higher "equal rejected"                             "0.1.5"      "0.1.5"
assert_not_higher "stable not higher than newer prerelease"    "0.1.5"      "0.1.6-rc.1"

echo "set_cargo_version"
set_fixture="$(mktemp -t aic-release-set.XXXXXX)"
cat > "$set_fixture" <<'EOF'
[package]
name = "aic"
version = "0.1.5"
edition = "2024"

[dist]
cargo-dist-version = "0.31.0"
EOF
set_cargo_version "$set_fixture" "0.2.3"
assert_eq "updates the package version" "0.2.3" "$(parse_cargo_version "$set_fixture")"
# cargo-dist-version must be untouched (the sed anchors on `^version = "`).
if grep -q '^cargo-dist-version = "0.31.0"' "$set_fixture"; then
  echo "  ok   cargo-dist-version untouched"; passed=$((passed + 1))
else
  echo "  FAIL cargo-dist-version untouched"; failed=$((failed + 1))
fi

echo
if [[ "$failed" -eq 0 ]]; then
  echo "all ${passed} assertions passed"
  exit 0
fi
echo "${failed} assertion(s) failed"
exit 1
