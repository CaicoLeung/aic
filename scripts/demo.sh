#!/usr/bin/env bash
#
# scripts/demo.sh — reproducible aic hunk-level split demo.
#
# Creates a throwaway git repo (examples/sample-repo), edits ONE file in three
# unrelated ways, and turns it into THREE atomic Conventional Commits — the
# exact "hunk-level, not file-level" headline from the README.
#
# The demo is fully self-contained:
#   * DEFAULT (recorded-fixture mode) needs NO API key and NO network. It
#     replays the exact hunk partition + Conventional Commit messages a real
#     `aic` run produces for this sample, staging each hunk the same way `aic`
#     does (`git apply --cached`), so what you see is what `aic` does.
#   * AIC_DEMO_LIVE=1 runs the real `aic` binary instead (needs a configured
#     provider, e.g. `aic setup`, or a local Ollama server). Use this to
#     re-record the demo GIF against the live tool.
#
# Re-record the cast/GIF: see docs/demo/README.md.
#
# Usage:
#   scripts/demo.sh                  # recorded fixture (no key, no network)
#   AIC_DEMO_LIVE=1 scripts/demo.sh  # run the real aic binary
#   AIC_DEMO_WORKDIR=/tmp/x scripts/demo.sh
#   AIC_BIN=/path/to/aic AIC_DEMO_LIVE=1 scripts/demo.sh
#
set -euo pipefail

# --- locate the repo root (parent of this script's dir) -----------------------
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SAMPLE_SRC="$REPO_ROOT/examples/sample-repo"

if [[ ! -f "$SAMPLE_SRC/src/auth.rs" ]]; then
  echo "demo.sh: sample repo not found at $SAMPLE_SRC/src/auth.rs" >&2
  exit 1
fi

# --- working directory (wiped + rebuilt every run for reproducibility) --------
WORK="${AIC_DEMO_WORKDIR:-$REPO_ROOT/examples/.demo-work/sample-repo}"
rm -rf "$WORK"
mkdir -p "$WORK"

# --- pretty printing ----------------------------------------------------------
if [[ -t 1 ]]; then
  BOLD=$'\033[1m'; DIM=$'\033[2m'; GREEN=$'\033[32m'; CYAN=$'\033[36m'; RESET=$'\033[0m'
else
  BOLD=''; DIM=''; GREEN=''; CYAN=''; RESET=''
fi
step() { printf '\n%s▸ %s%s\n' "$CYAN$BOLD" "$1" "$RESET"; }
note() { printf '%s%s%s\n' "$DIM" "$1" "$RESET"; }

# --- seed the throwaway repo from the checked-in sample -----------------------
step "Create throwaway git repo at $WORK"
cp -R "$SAMPLE_SRC/." "$WORK/"
cd "$WORK"
git init -q
git config user.name "aic demo"
git config user.email "demo@aic.dev"
git config commit.gpgsign false
git add -A
git commit -q -m "chore: baseline auth module"
note "baseline committed ($(git rev-parse --short HEAD))"

# --- apply the three unrelated edits to the WORKING TREE (nothing staged) -----
step "Edit ONE file (src/auth.rs) in three unrelated ways — nothing staged"
git apply edits/01-style.patch
git apply edits/02-fix.patch
git apply edits/03-feat.patch
echo "$DIM--- git diff --stat ---$RESET"
git diff --stat -- src/auth.rs
echo "$DIM--- hunk count in workdir diff ---$RESET"
printf 'hunks: %s\n' "$(git diff -- src/auth.rs | grep -c '^@@')"

# --- BEFORE: what a file-level tool would do ----------------------------------
step "BEFORE — a file-level tool makes ONE mixed-concern commit"
note "aicommits / opencommit / plain \"git commit -am\" would land this as:"
printf '  %supdate src/auth.rs%s   <- mixed concerns, muddy history\n' "$DIM" "$RESET"

# --- the split ----------------------------------------------------------------
if [[ "${AIC_DEMO_LIVE:-0}" == "1" ]]; then
  step "AFTER (LIVE) — run the real 'aic' binary (LLM hunk-level split)"
  # Resolve the aic binary: explicit override, PATH, then local build outputs.
  AIC_BIN="${AIC_BIN:-}"
  if [[ -z "$AIC_BIN" ]]; then
    if command -v aic >/dev/null 2>&1; then
      AIC_BIN="aic"
    elif [[ -x "$REPO_ROOT/target/release/aic" ]]; then
      AIC_BIN="$REPO_ROOT/target/release/aic"
    elif [[ -x "$REPO_ROOT/target/debug/aic" ]]; then
      AIC_BIN="$REPO_ROOT/target/debug/aic"
    else
      echo "demo.sh: AIC_DEMO_LIVE=1 but no 'aic' binary found." >&2
      echo "         Install it (cargo install --path .) or set AIC_BIN, then re-run." >&2
      exit 1
    fi
  elif ! command -v "$AIC_BIN" >/dev/null 2>&1 && [[ ! -x "$AIC_BIN" ]]; then
    echo "demo.sh: AIC_BIN='$AIC_BIN' not found / not executable." >&2
    exit 1
  fi
  # aic reads its config from dirs::config_dir()/aic/config.toml — replicate
  # that resolution so we can fail fast with guidance instead of letting aic
  # hang on retries when no provider is configured.
  if [[ -n "${XDG_CONFIG_HOME:-}" ]]; then
    AIC_CFG="$XDG_CONFIG_HOME/aic/config.toml"
  elif [[ "$(uname)" == "Darwin" ]]; then
    AIC_CFG="$HOME/Library/Application Support/aic/config.toml"
  else
    AIC_CFG="$HOME/.config/aic/config.toml"
  fi
  if [[ ! -f "$AIC_CFG" ]]; then
    echo "demo.sh: AIC_DEMO_LIVE=1 but no aic config found at:" >&2
    echo "         $AIC_CFG" >&2
    echo "         Run 'aic setup' to configure a provider (or start a local" >&2
    echo "         Ollama server and point aic at it), then re-run." >&2
    exit 1
  fi
  note "binary: $AIC_BIN | config: $AIC_CFG"
  # aic with nothing staged batch-splits every hunk into atomic commits.
  "$AIC_BIN"
else
  step "AFTER (aic) — three atomic, hunk-level Conventional Commits"
  note "recorded fixture: same partition + messages a real 'aic' run produces."
  note "staging each hunk via 'git apply --cached' (exactly how aic stages)."
  # Recorded output of a real `aic` run over this exact sample. Regenerate live
  # with AIC_DEMO_LIVE=1; messages may vary slightly by model.
  # Hunk 1 — style
  git apply --cached edits/01-style.patch
  git commit -q -m "style(auth): format Auth constructor"
  # Hunk 2 — fix
  git apply --cached edits/02-fix.patch
  git commit -q -m "fix(auth): allow tokens expiring at current second"
  # Hunk 3 — feat
  git apply --cached edits/03-feat.patch
  git commit -q -m "feat(auth): add OAuth2 login support"
fi

# --- show the result ----------------------------------------------------------
step "Resulting history"
echo "$DIM--- git log --oneline ---$RESET"
git log --oneline
echo "$DIM--- working tree clean? ---$RESET"
if [[ -z "$(git status --porcelain)" ]]; then
  printf '%s✓ clean — every hunk landed in exactly one commit%s\n' "$GREEN" "$RESET"
else
  git status --porcelain
fi

step "Done — 3 atomic commits from 1 file, no manual 'git add -p'."
printf '%sRe-record the GIF:%s see docs/demo/README.md\n' "$DIM" "$RESET"
