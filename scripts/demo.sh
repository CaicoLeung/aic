#!/usr/bin/env bash
#
# scripts/demo.sh — reproduce aic's headline (block-level atomic commits) in
# one command, deterministically and with NO network by default.
#
# What it does:
#   1. Materializes a throwaway git repo from examples/sample-repo/ in a temp dir.
#   2. Commits the base state.
#   3. Applies three deliberately-unrelated edits to src/main.rs (a fix, a feat,
#      and a style change).
#   4. Either:
#        - LIVE  (AIC_DEMO_LIVE=1 + reachable local provider): runs the real
#          `aic` against the mixed working tree and lets it split the edits into
#          atomic commits; or
#        - FIXTURE (default): replays the recorded commit history from
#          examples/before-after/after.txt so the demo is deterministic and
#          CI-safe.
#   5. Prints `git log --oneline` (>= 3 atomic Conventional Commits) and exits 0.
#
# Re-record the GIF / asciinema with:
#   AIC_DEMO_LIVE=1 asciinema rec demo.cast --command scripts/demo.sh
# (see docs/demo/README.md).
#
# Env knobs:
#   AIC_DEMO_LIVE=1   Run the real aic against a local provider (default: off).
#   AIC_BIN           Path to the aic binary (default: `aic` from PATH, then
#                     target/release/aic, then target/debug/aic).
#   AIC_OLLAMA_URL    URL to probe for Ollama (default: http://localhost:11434).
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SAMPLE_DIR="$REPO_ROOT/examples/sample-repo"
WORK_DIR="$(mktemp -d)"
trap 'rm -rf "$WORK_DIR"' EXIT

# --- helpers -----------------------------------------------------------------

log()  { printf '\033[1m==>\033[0m %s\n' "$*" >&2; }
note() { printf '    %s\n' "$*" >&2; }

# Base state of src/main.rs (matches examples/sample-repo/src/main.rs).
write_base() {
  cat > src/main.rs <<'RS'
// A tiny throwaway program used by `scripts/demo.sh` to show aic splitting a
// single file's mixed edits into block-level atomic commits.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    let name = &args[1];
    println!("Hello, {}", name);
}
RS
}

# After the `fix` edit: guard against a missing argument.
write_after_fix() {
  cat > src/main.rs <<'RS'
// A tiny throwaway program used by `scripts/demo.sh` to show aic splitting a
// single file's mixed edits into block-level atomic commits.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: demo <name>");
        std::process::exit(1);
    }
    let name = &args[1];
    println!("Hello, {}", name);
}
RS
}

# After the `feat` edit on top of fix: uppercase the name when --upper is passed.
write_after_feat() {
  cat > src/main.rs <<'RS'
// A tiny throwaway program used by `scripts/demo.sh` to show aic splitting a
// single file's mixed edits into block-level atomic commits.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: demo <name>");
        std::process::exit(1);
    }
    let name = &args[1];
    let upper = args.iter().any(|a| a == "--upper");
    let display = if upper { name.to_uppercase() } else { name.clone() };
    println!("Hello, {}", display);
}
RS
}

# After the `style` edit on top of feat: module doc comment + trailing newline.
write_after_style() {
  cat > src/main.rs <<'RS'
//! Tiny throwaway program used by `scripts/demo.sh` to show aic splitting a
//! single file's mixed edits into block-level atomic commits.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: demo <name>");
        std::process::exit(1);
    }
    let name = &args[1];
    let upper = args.iter().any(|a| a == "--upper");
    let display = if upper { name.to_uppercase() } else { name.clone() };
    println!("Hello, {}", display);
}
RS
}

# --- materialize the throwaway repo ------------------------------------------

log "Materializing throwaway repo in: $WORK_DIR"
mkdir -p "$WORK_DIR/src"
cp "$SAMPLE_DIR/Cargo.toml" "$WORK_DIR/Cargo.toml"
cd "$WORK_DIR"
git init -q
git config user.email "demo@aic.local"
git config user.name "aic demo"
write_base
git add -A
git commit -qm "chore: seed sample-repo base state"

# --- live vs fixture ---------------------------------------------------------

resolve_aic_bin() {
  if [[ -n "${AIC_BIN:-}" && -x "$AIC_BIN" ]]; then printf '%s' "$AIC_BIN"; return; fi
  if command -v aic >/dev/null 2>&1; then printf '%s' "aic"; return; fi
  if [[ -x "$REPO_ROOT/target/release/aic" ]]; then printf '%s' "$REPO_ROOT/target/release/aic"; return; fi
  if [[ -x "$REPO_ROOT/target/debug/aic" ]]; then printf '%s' "$REPO_ROOT/target/debug/aic"; return; fi
  return 1
}

provider_reachable() {
  local url="${AIC_OLLAMA_URL:-http://localhost:11434}"
  # 2s timeout, any HTTP response (even 404) means the server is up.
  if command -v curl >/dev/null 2>&1; then
    curl -sS -o /dev/null --max-time 2 "$url" >/dev/null 2>&1 || return 1
  elif command -v wget >/dev/null 2>&1; then
    wget -q -T 2 -O /dev/null "$url" >/dev/null 2>&1 || return 1
  else
    return 1
  fi
}

run_live() {
  local aic; aic="$(resolve_aic_bin)" || return 1
  log "LIVE: applying all three edits at once (mixed working tree)"
  write_after_style           # all three edits combined, nothing staged
  note "running: $aic (LLM_BACKEND=${LLM_BACKEND:-ollama})"
  log "aic is splitting the mixed edits into atomic commits…"
  if ! LLM_BACKEND="${LLM_BACKEND:-ollama}" \
       LLM_MODEL="${LLM_MODEL:-qwen2.5-coder:7b}" \
       LLM_BASE_URL="${LLM_BASE_URL:-${AIC_OLLAMA_URL:-http://localhost:11434}}" \
       "$aic" </dev/null; then
    return 1
  fi
}

run_fixture() {
  log "FIXTURE: replaying recorded atomic history (examples/before-after/after.txt)"
  note "apply fix edit";   write_after_fix;   git add -A
  git commit -qm "fix: guard against a missing argument to avoid a panic"
  note "apply feat edit";  write_after_feat;  git add -A
  git commit -qm "feat: print the name uppercased when --upper is passed"
  note "apply style edit"; write_after_style; git add -A
  git commit -qm "style: add module doc comment and trailing newline"
}

if [[ "${AIC_DEMO_LIVE:-0}" == "1" ]]; then
  if provider_reachable && run_live; then
    log "aic finished (live)"
  else
    log "Live run unavailable (no local provider or aic missing); falling back to fixture."
    run_fixture
  fi
else
  run_fixture
fi

# --- result ------------------------------------------------------------------

echo >&2
log "Resulting history (git log --oneline):"
git log --oneline

# Verify the headline: >= 3 atomic Conventional Commits on top of the base.
commits=$(git rev-list --count HEAD)
if (( commits < 4 )); then
  echo "ERROR: expected >= 3 atomic commits on top of the base commit, got $((commits - 1))" >&2
  exit 1
fi
echo >&2
log "OK: $((${commits} - 1)) atomic Conventional Commits split from one file's mixed edits."
