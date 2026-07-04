#!/usr/bin/env bash
# scripts/release_lib.sh — pure helpers for scripts/release.sh.
#
# No side effects, no main(). Safe to source under any shell for unit testing
# (see scripts/test_release.sh). Keep this side-effect-free.

# Strip a leading 'v' from a version argument.
normalize_version() {
  printf '%s' "${1#v}"
}

# Read the package version from a Cargo.toml file path. Matches only the bare
# `version = "..."` key under [package], ignoring `cargo-dist-version` etc.
parse_cargo_version() {
  local cargo_toml="$1"
  grep -m1 '^version = "' "$cargo_toml" \
    | sed -E 's/^version = "([^"]+)".*/\1/'
}

# Predicate: is the (already-normalized) version cargo-dist-compatible?
# Accepts major.minor.patch with an optional prerelease/build suffix (-foo / +foo),
# but rejects a trailing numeric segment like 0.1.5.7, which is not valid SemVer.
is_valid_version() {
  [[ "$1" =~ ^[0-9]+\.[0-9]+\.[0-9]+([-+].*)?$ ]]
}
