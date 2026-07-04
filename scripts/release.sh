#!/usr/bin/env bash
# scripts/release.sh — safe local release tagger for aic.
#
# Catches the tag/Cargo.toml mismatch that cargo-dist would otherwise only
# reject AFTER the tag is already public. Asserts working copy, branch,
# Cargo.toml version, and tag all agree, then creates an annotated tag and
# pushes it.
#
# Usage:
#   scripts/release.sh 0.1.6              # assert, tag, push main + tag
#   scripts/release.sh 0.1.6 --dry-run    # assert only, print the plan
#
# Does NOT bump Cargo.toml or commit — edit and commit those yourself first.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# shellcheck source=release_lib.sh
source "${SCRIPT_DIR}/release_lib.sh"

fail() {
  echo "error: $*" >&2
  exit 1
}

usage() {
  cat >&2 <<EOF
usage: scripts/release.sh <version> [--dry-run]

  <version>    version to release, e.g. 0.1.6 (leading 'v' optional)
  --dry-run    run all checks and print the plan; tag and push nothing
EOF
  exit 2
}

main() {
  local raw_version="" dry_run=false
  while [[ $# -gt 0 ]]; do
    case "$1" in
      --dry-run) dry_run=true; shift ;;
      -h | --help) usage ;;
      *)
        if [[ -z "$raw_version" ]]; then
          raw_version="$1"
        else
          fail "unexpected argument '$1'"
        fi
        shift
        ;;
    esac
  done
  [[ -n "$raw_version" ]] || usage

  local version tag cargo_version cargo_toml root
  version="$(normalize_version "$raw_version")"
  tag="v${version}"

  root="$(git rev-parse --show-toplevel)" || fail "not inside a git repo"
  cargo_toml="${root}/Cargo.toml"
  [[ -f "$cargo_toml" ]] || fail "${cargo_toml} not found"

  echo "→ validating release ${version}"

  # 1. version must be cargo-dist-compatible (major.minor.patch + optional suffix)
  is_valid_version "$version" \
    || fail "'${version}' is not a valid major.minor.patch version (a prerelease suffix like -rc.1 is allowed)"

  # 2. Cargo.toml version must match exactly
  cargo_version="$(parse_cargo_version "$cargo_toml")" \
    || fail "could not parse version from Cargo.toml"
  [[ "$cargo_version" == "$version" ]] \
    || fail "Cargo.toml is '${cargo_version}', requested '${version}' — bump Cargo.toml first"

  # 3. working tree must be clean (the version bump must already be committed)
  git diff --quiet --ignore-submodules \
    && git diff --cached --quiet --ignore-submodules \
    || fail "working tree dirty — commit first"

  # 4. must be on main
  local branch
  branch="$(git rev-parse --abbrev-ref HEAD)"
  [[ "$branch" == "main" ]] || fail "on '${branch}', must be on 'main'"

  # 5. must be in sync with origin/main
  git fetch --quiet origin main || fail "could not fetch origin/main (offline?)"
  local local_sha remote_sha
  local_sha="$(git rev-parse HEAD)"
  remote_sha="$(git rev-parse origin/main)" \
    || fail "origin/main not found — set an 'origin' remote"
  [[ "$local_sha" == "$remote_sha" ]] \
    || fail "HEAD (${local_sha:0:8}) != origin/main (${remote_sha:0:8}) — push/pull first"

  # 6. tag must not already exist
  [[ -z "$(git tag -l "$tag")" ]] || fail "tag ${tag} already exists"

  echo "  ✓ Cargo.toml = ${version}"
  echo "  ✓ clean tree on main, synced with origin"
  echo "  ✓ ${tag} is new"

  if $dry_run; then
    echo "→ dry-run: would create annotated tag ${tag} and push main + ${tag}"
    exit 0
  fi

  # 7. annotated tag (not lightweight) — required for tag-protection rules
  #    and consumed correctly by git-cliff.
  git tag -a "$tag" -m "Release ${tag}"
  echo "  ✓ created annotated tag ${tag}"

  # 8. push main + tag together
  git push origin main "$tag"
  echo "  ✓ pushed main + ${tag}"
  echo
  echo "✅ ${tag} pushed — watch the Release workflow:"
  echo "   https://github.com/CaicoLeung/aic/actions/workflows/release.yml"
}

main "$@"
