#!/usr/bin/env bash
# Real end-to-end smoke test: one commit-message generation call per provider.
#
# For each provider whose API key is present in the environment, this script
# builds a throwaway git repo, stages a small change, runs `aic` (the default
# staged-files → one-commit path) against the provider, and asserts a commit
# with a Conventional-Commits-shaped subject landed.
#
#   SKIP/NO_KEY   — provider key not set, skipped
#   PASS          — aic generated a message and committed
#   FAIL          — aic errored or produced no conventional subject
#
# Usage:
#   scripts/smoke-test-providers.sh [path-to-aic-binary]
#
# Example (verify the five Phase-1 providers + the openai-compatible escape
# hatch; openai-compatible points at DeepSeek's OpenAI-compatible endpoint):
#   export XAI_API_KEY=... MISTRAL_API_KEY=... OPENROUTER_API_KEY=... \
#          PERPLEXITY_API_KEY=... TOGETHER_API_KEY=...
#   scripts/smoke-test-providers.sh
set -euo pipefail

AIC_BIN="${1:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/aic}"
if [[ ! -x "$AIC_BIN" ]]; then
  echo "error: aic binary not found at $AIC_BIN (build first: cargo build)" >&2
  exit 2
fi

WORK=$(mktemp -d)
trap 'rm -rf "$WORK"' EXIT

# provider <name> <LLM_BACKEND> <model> [extra env assignments...]
run_one() {
  local name="$1" backend="$2" model="$3"
  shift 3

  local key_var=""
  case "$backend" in
    xai) key_var="XAI_API_KEY" ;;
    mistral) key_var="MISTRAL_API_KEY" ;;
    openrouter) key_var="OPENROUTER_API_KEY" ;;
    perplexity) key_var="PERPLEXITY_API_KEY" ;;
    together) key_var="TOGETHER_API_KEY" ;;
    deepseek) key_var="DEEPSEEK_API_KEY" ;;
    openai-compatible) key_var="DEEPSEEK_API_KEY" ;; # reuses DeepSeek key
  esac

  if [[ -z "${!key_var:-}" ]]; then
    printf 'SKIP  %-18s (%s not set)\n' "$name" "$key_var"
    return
  fi

  local repo="$WORK/$name"
  mkdir -p "$repo"
  git -C "$repo" init -q -b main
  git -C "$repo" config user.email smoke@aic.test
  git -C "$repo" config user.name "aic smoke test"
  git -C "$repo" config commit.gpgsign false
  printf 'fn main() {\n    println!("hello");\n}\n' > "$repo/main.rs"
  git -C "$repo" add main.rs
  git -C "$repo" commit -q -m "chore: initial"

  # a small, message-worthy change
  printf 'fn main() {\n    println!("hello, world");\n    println!("bye");\n}\n' > "$repo/main.rs"
  git -C "$repo" add main.rs

  local log subject regex
  regex='^(feat|fix|chore|docs|refactor|perf|test|build|ci|style|revert)(\([^)]*\))?!?:'
  if log=$(cd "$repo" && \
      env -u LLM_BACKEND -u LLM_MODEL -u LLM_API_KEY -u LLM_BASE_URL \
          HOME="$WORK/home" \
          LLM_BACKEND="$backend" LLM_MODEL="$model" \
          LLM_API_KEY="${!key_var}" "$@" \
          "$AIC_BIN" 2>&1); then
    subject=$(git -C "$repo" log -1 --format=%s)
    if [[ "$subject" =~ $regex ]]; then
      printf 'PASS  %-18s subject="%s"\n' "$name" "$subject"
    else
      printf 'FAIL  %-18s non-conventional subject: %s\n' "$name" "$subject"
      printf '      %s\n' "$log" | sed 's/^/      /' | head -5
    fi
  else
    printf 'FAIL  %-18s aic exited non-zero\n' "$name"
    printf '      %s\n' "$log" | sed 's/^/      /' | head -8
  fi
}

# Phase-1 verification targets: xAI, Mistral, OpenRouter, Perplexity, Together,
# plus the OpenAI-compatible escape hatch (exercised against DeepSeek's
# OpenAI-compatible endpoint — the only key this machine has).
run_one "xAI (grok-4.3)"          xai              "grok-4.3"
run_one "Mistral"                  mistral          "mistral-small-latest"
run_one "OpenRouter (explicit)"    openrouter       "openai/gpt-4o-mini"
run_one "Perplexity"               perplexity       "sonar"
run_one "Together"                 together         "meta-llama/Llama-3.3-70B-Instruct-Turbo"

run_one "OpenAI-compatible→DeepSeek" openai-compatible "deepseek-v4-flash" \
  LLM_BASE_URL="https://api.deepseek.com"

# Sanity: the already-verified providers still work through the same binary.
run_one "DeepSeek (baseline)"     deepseek         "deepseek-v4-flash"

printf '\nDone. SKIP = key not set (run with the provider key exported to exercise it).\n'
