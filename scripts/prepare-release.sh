#!/usr/bin/env bash
# scripts/prepare-release.sh — the mutate half of the release flow.
#
# Bumps Cargo.toml + Cargo.lock and regenerates CHANGELOG.md via git-cliff for
# the target tag, then commits all three as `chore(release): vX.Y.Z`. Commits
# only — it does NOT tag. The tag (irreversible: it triggers the release
# workflow, and branch protection now blocks force-push) is left to
# scripts/release.sh, so you get a checkpoint to review the generated CHANGELOG
# and version bump before shipping.
#
# Usage:
#   scripts/prepare-release.sh 0.1.6              # bump + changelog + commit
#   scripts/prepare-release.sh 0.1.6 --dry-run    # preconditions + plan only
#
# See RELEASING.md for the full procedure and the rollback runbook.

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
usage: scripts/prepare-release.sh <version> [--dry-run]

  <version>    target version, e.g. 0.1.6 (leading 'v' optional). Must be
               strictly higher than the current Cargo.toml version.
  --dry-run    run preconditions and print the plan; do not mutate or commit.
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

  local version cargo_version root cargo_toml
  version="$(normalize_version "$raw_version")"

  root="$(git rev-parse --show-toplevel)" || fail "not inside a git repo"
  cargo_toml="${root}/Cargo.toml"
  [[ -f "$cargo_toml" ]] || fail "${cargo_toml} not found"
  cd "$root"

  echo "→ preparing release ${version}"

  # 1. version must be cargo-dist-compatible
  is_valid_version "$version" \
    || fail "'${version}' is not a valid major.minor.patch version (a prerelease suffix like -rc.1 is allowed)"

  # 2. must differ from current and be strictly higher (no accidental downgrade)
  cargo_version="$(parse_cargo_version "$cargo_toml")" \
    || fail "could not parse version from Cargo.toml"
  [[ "$cargo_version" == "$version" ]] \
    && fail "Cargo.toml is already at ${version} — nothing to prepare"
  is_version_higher "$version" "$cargo_version" \
    || fail "target ${version} is not higher than current ${cargo_version} — downgrade rejected"

  # 3. clean tree, on main, synced with origin/main — same preconditions as
  #    release.sh, so the bump commit lands on a clean, up-to-date main.
  git diff --quiet --ignore-submodules \
    && git diff --cached --quiet --ignore-submodules \
    || fail "working tree dirty — commit or stash first"
  local branch
  branch="$(git rev-parse --abbrev-ref HEAD)"
  [[ "$branch" == "main" ]] || fail "on '${branch}', must be on 'main'"
  git fetch --quiet origin main || fail "could not fetch origin/main (offline?)"
  local local_sha remote_sha
  local_sha="$(git rev-parse HEAD)"
  remote_sha="$(git rev-parse origin/main)" \
    || fail "origin/main not found — set an 'origin' remote"
  [[ "$local_sha" == "$remote_sha" ]] \
    || fail "HEAD (${local_sha:0:8}) != origin/main (${remote_sha:0:8}) — push/pull first"

  # 4. git-cliff regenerates CHANGELOG.md
  command -v git-cliff >/dev/null 2>&1 \
    || fail "git-cliff not found — 'cargo install git-cliff' first"

  echo "  ✓ ${cargo_version} → ${version}, clean main synced with origin"

  if $dry_run; then
    echo "→ dry-run: would bump Cargo.toml, refresh Cargo.lock, regenerate"
    echo "           CHANGELOG.md for v${version}, and commit as"
    echo "           'chore(release): v${version}'."
    echo "           then hand off: scripts/release.sh ${version}"
    exit 0
  fi

  # 5. bump Cargo.toml, then refresh Cargo.lock (cargo check rewrites the lock
  #    for the new root version without bumping unrelated deps).
  set_cargo_version "$cargo_toml" "$version" || fail "failed to bump Cargo.toml"
  cargo check --quiet \
    || fail "cargo check failed after version bump (Cargo.lock not refreshed?)"

  # 6. regenerate CHANGELOG.md for the target tag, before the tag exists.
  #    git-cliff rebuilds the file deterministically from the full tag history.
  git-cliff --tag "v${version}" -o CHANGELOG.md \
    || fail "git-cliff failed to regenerate CHANGELOG.md"

  # 7. commit the three files together.
  git add Cargo.toml Cargo.lock CHANGELOG.md
  git diff --cached --quiet \
    && fail "nothing staged after bump — unexpected (did the version actually change?)"
  git commit -q -m "chore(release): v${version}"

  echo "  ✓ committed chore(release): v${version}"
  echo
  echo "✅ prepared ${version}. Review the commit, then tag and push:"
  echo "   scripts/release.sh ${version}"
}

main "$@"
