#!/usr/bin/env bash
# scripts/release_lib.sh — pure helpers for scripts/release.sh and
# scripts/prepare-release.sh.
#
# Side-effect-free to source: no main(), no top-level mutation. Functions do
# I/O only on arguments the caller supplies, so the lib is unit-testable by
# sourcing (see scripts/test_release.sh).

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

# Predicate: is version $1 strictly greater than version $2?
# Compares the major.minor.patch core only — prerelease/build suffixes are
# stripped — so prepare-release.sh can reject accidental downgrades without
# caring about "-rc.1" vs "-rc.2" ordering.
is_version_higher() {
  local a="${1%%-*}" b="${2%%-*}"
  local a_major a_minor a_patch b_major b_minor b_patch
  IFS=. read -r a_major a_minor a_patch <<<"$a"
  IFS=. read -r b_major b_minor b_patch <<<"$b"
  [[ "$a_major" =~ ^[0-9]+$ ]] || a_major=0
  [[ "$a_minor" =~ ^[0-9]+$ ]] || a_minor=0
  [[ "$a_patch" =~ ^[0-9]+$ ]] || a_patch=0
  [[ "$b_major" =~ ^[0-9]+$ ]] || b_major=0
  [[ "$b_minor" =~ ^[0-9]+$ ]] || b_minor=0
  [[ "$b_patch" =~ ^[0-9]+$ ]] || b_patch=0
  (( a_major > b_major )) && return 0
  (( a_major < b_major )) && return 1
  (( a_minor > b_minor )) && return 0
  (( a_minor < b_minor )) && return 1
  (( a_patch > b_patch )) && return 0
  return 1
}

# Write a new version into the bare `version = "..."` key of a Cargo.toml file
# path. Mirrors parse_cargo_version's match (ignores cargo-dist-version etc.).
# Writes via a temp file + mv so it is portable across GNU and BSD sed (no -i).
set_cargo_version() {
  local cargo_toml="$1" version="$2"
  local tmp
  tmp="$(mktemp -t aic-release-set.XXXXXX)" || return 1
  if sed -E 's/^version = "[^"]*"/version = "'"$version"'"/' "$cargo_toml" > "$tmp"; then
    mv "$tmp" "$cargo_toml"
  else
    rm -f "$tmp"
    return 1
  fi
}
