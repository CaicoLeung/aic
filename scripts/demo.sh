#!/usr/bin/env bash
#
# scripts/demo.sh — reproducible aic demo.
#
# Builds a tiny throwaway git repo from examples/sample-repo, applies three
# deliberately-unrelated edits (a fix, a feat, a style tweak), and runs `aic`.
# The headline behavior: one file mixing three concerns is split into three
# atomic Conventional-Commits.
#
# Two modes:
#
#   default (no flag)  — OFFLINE / FIXTURE mode. `aic` reads recorded responses
#                        from examples/fixtures (AIC_FIXTURE_DIR), so the demo is
#                        fully deterministic and needs no network, no API key,
#                        and no local model. aic still runs its real parse →
#                        validate → stage-by-hunk → commit pipeline, so the
#                        demo shows genuine splitting, not a replay. This is what
#                        the README GIF is recorded from and what CI runs.
#
#   --live             — LIVE mode. `aic` is pointed at a local Ollama server
#                        via a throwaway config file (ADR 0008: aic reads only
#                        the config file, no env vars). Same sample repo, but a
#                        real model does the splitting + message writing.
#                        Requires `ollama` running and a model pulled
#                        (e.g. `ollama pull llama3.3`).
#
# Usage:
#   scripts/demo.sh [--live] [path-to-aic-binary]
#
# Exit 0 on success with >=3 atomic Conventional-Commits in the sample repo's
# history; non-zero (with a diagnostic) otherwise. No state is left behind: the
# sample repo lives in a temp directory removed on exit.
set -euo pipefail

MODE="fixture"
AIC_BIN_OVERRIDE=""
for arg in "$@"; do
  case "$arg" in
    --live) MODE="live" ;;
    --fixture) MODE="fixture" ;;
    -h|--help)
      sed -n '2,30p' "$0"; exit 0 ;;
    *)
      AIC_BIN_OVERRIDE="$arg" ;;
  esac
done

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
EXAMPLES="$REPO_ROOT/examples"
FIXTURES="$EXAMPLES/fixtures"

# --- locate the aic binary -------------------------------------------------
AIC_BIN="${AIC_BIN_OVERRIDE:-$REPO_ROOT/target/debug/aic}"
if [[ ! -x "$AIC_BIN" ]]; then
  echo "→ building aic (debug)…" >&2
  (cd "$REPO_ROOT" && cargo build --quiet) >&2
  AIC_BIN="$REPO_ROOT/target/debug/aic"
fi
[[ -x "$AIC_BIN" ]] || { echo "error: aic binary not found at $AIC_BIN" >&2; exit 2; }

# --- throwaway sample repo -------------------------------------------------
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
REPO="$WORK/sample-repo"
mkdir -p "$REPO/src"
cp "$EXAMPLES/sample-repo/src/calculator.rs" "$REPO/src/calculator.rs"

git -C "$REPO" init -q -b main
git -C "$REPO" config user.email demo@aic.test
git -C "$REPO" config user.name "aic demo"
git -C "$REPO" config commit.gpgsign false
git -C "$REPO" add src/calculator.rs
git -C "$REPO" commit -q -m "chore: seed calculator"

# Apply the three unrelated edits, unstaged — exactly the messy working tree a
# developer would hand to `aic`.
for patch in 01-fix 02-feat 03-style; do
  git -C "$REPO" apply "$EXAMPLES/sample-repo/patches/$patch.patch"
done

echo "── sample repo (3 unrelated edits, unstaged) ──────────────────────────"
git -C "$REPO" diff --stat
echo

# --- run aic ---------------------------------------------------------------
# Isolate HOME so the developer's real aic config never leaks into the demo
# (mirrors scripts/smoke-test-providers.sh). aic reads only the config file.
HOME_ISOLATED="$WORK/home"
mkdir -p "$HOME_ISOLATED"

case "$MODE" in
  fixture)
    [[ -f "$FIXTURES/fixtures.json" ]] || {
      echo "error: $FIXTURES/fixtures.json missing (fixture mode needs it)" >&2
      exit 2
    }
    echo "── running aic (OFFLINE fixture mode — no network, deterministic) ───"
    ( cd "$REPO" && \
      AIC_FIXTURE_DIR="$FIXTURES" \
      HOME="$HOME_ISOLATED" \
      "$AIC_BIN" )
    ;;
  live)
    if ! command -v ollama >/dev/null 2>&1; then
      echo "error: --live needs the 'ollama' CLI on PATH (install from ollama.com)" >&2
      exit 2
    fi
    MODEL="${AIC_DEMO_MODEL:-llama3.3}"
    # aic reads only the config file (ADR 0008); write a throwaway one into the
    # isolated HOME for both macOS and Linux config locations.
    CFG="backend = \"ollama\"
model = \"$MODEL\"
base_url = \"http://localhost:11434\"
"
    mkdir -p "$HOME_ISOLATED/.config/aic" "$HOME_ISOLATED/Library/Application Support/aic"
    printf '%s' "$CFG" > "$HOME_ISOLATED/.config/aic/config.toml"
    printf '%s' "$CFG" > "$HOME_ISOLATED/Library/Application Support/aic/config.toml"
    echo "── running aic (LIVE mode — ollama / $MODEL) ────────────────────────"
    ( cd "$REPO" && HOME="$HOME_ISOLATED" "$AIC_BIN" )
    ;;
esac

# --- verify the headline: >=3 atomic Conventional-Commits ------------------
echo
echo "── resulting history ─────────────────────────────────────────────────"
git -C "$REPO" log --oneline --no-decorate

# Portable across macOS system bash (3.2) and Linux (bash 4+): avoid `mapfile`.
SUBJECTS=()
while IFS= read -r line; do
  SUBJECTS+=("$line")
done < <(git -C "$REPO" log --format=%s --no-decorate | tail -n +2)
count="${#SUBJECTS[@]}"
regex='^(feat|fix|chore|docs|refactor|perf|test|build|ci|style|revert)(\([^)]*\))?!?:'

conventional=0
for s in "${SUBJECTS[@]}"; do
  [[ "$s" =~ $regex ]] && conventional=$((conventional + 1))
done

echo
if [[ "$count" -ge 3 && "$conventional" -ge 3 ]]; then
  echo "✓ demo OK: $count atomic commits, $conventional Conventional-Commits-shaped"
  exit 0
else
  echo "✗ demo FAILED: expected ≥3 Conventional-Commits, got $count commits ($conventional conventional)" >&2
  exit 1
fi
