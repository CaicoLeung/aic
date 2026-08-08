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
# Re-record the GIF / asciinema with the LIVE path (see docs/demo/README.md):
#   AIC_DEMO_PROVIDER=ollama AIC_DEMO_LIVE=1 \
#     asciinema rec docs/demo/aic-hunk-split.cast --command scripts/demo.sh
#
# Env knobs:
#   AIC_DEMO_LIVE=1        Run the real aic against a provider (default: off,
#                          falls back to the deterministic fixture on failure).
#   AIC_DEMO_PROVIDER       Provider for the live run (default: ollama). Any aic
#                          provider name: ollama, openai, deepseek, groq, ...
#   AIC_DEMO_KEY            API key for the live provider (not needed for
#                          ollama).
#   AIC_DEMO_MODEL          Model id override for the live run.
#   AIC_DEMO_BASE_URL       Base URL override (ollama / openai-compatible).
#   AIC_BIN                Path to the aic binary (default: `aic` from PATH,
#                          then target/release/aic, then target/debug/aic).
#   AIC_OLLAMA_URL          URL to probe for a default local Ollama
#                          (default: http://localhost:11434).
#
# NOTE: aic reads its provider from ~/.config/aic/config.toml, NOT from env
# vars (those were removed in the config-single-source-of-truth refactor). For
# the live run this script writes an isolated config to a throwaway HOME so
# your real aic config is never touched.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SAMPLE_DIR="$REPO_ROOT/examples/sample-repo"
WORK_DIR="$(mktemp -d)"
FAKE_HOME=""
cleanup() {
  rm -rf "$WORK_DIR"
  [[ -n "$FAKE_HOME" ]] && rm -rf "$FAKE_HOME"
  :
}
trap cleanup EXIT

# --- helpers -----------------------------------------------------------------

log()  { printf '\033[1m==>\033[0m %s\n' "$*" >&2; }
note() { printf '    %s\n' "$*" >&2; }

# Base state of src/main.rs (matches examples/sample-repo/src/main.rs).
write_base() {
  cat > src/main.rs <<'RS'
// Tiny throwaway program for scripts/demo.sh: shows aic splitting one file's
// mixed, unrelated edits into block-level atomic commits.
//
// Three concerns live in three functions, spaced far enough apart that git
// emits one hunk per concern.

/// Program entry: parse args and print a greeting.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    // BUG: indexing args[1] panics when no name is given.
    let name = &args[1];
    println!("{}", greet(name));
}

/// A decorative separator used when printing headings.
fn divider() -> String {
    let bar = "=".repeat(20);
    bar
}

/// Build a friendly greeting for a name.
fn greet(name: &str) -> String {
    format!("Hello, {}", name)
}

/// Wrap a heading with a divider on each side.
fn banner(heading: &str) -> String {
    let bar = divider();
    let body = heading.to_string();
    format!("{}\n{}\n{}", bar, body, bar)
}

/// Emit a one-line access record for a caller.
fn log_access(who: &str) {
    let message = who.to_string();
    let stamped = format!("[access] {}", message);
    println!("{}", stamped);
}
RS
}

# After the `fix` edit: guard against a missing argument (in main).
write_after_fix() {
  cat > src/main.rs <<'RS'
// Tiny throwaway program for scripts/demo.sh: shows aic splitting one file's
// mixed, unrelated edits into block-level atomic commits.
//
// Three concerns live in three functions, spaced far enough apart that git
// emits one hunk per concern.

/// Program entry: parse args and print a greeting.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: demo <name>");
        std::process::exit(1);
    }
    let name = &args[1];
    println!("{}", greet(name));
}

/// A decorative separator used when printing headings.
fn divider() -> String {
    let bar = "=".repeat(20);
    bar
}

/// Build a friendly greeting for a name.
fn greet(name: &str) -> String {
    format!("Hello, {}", name)
}

/// Wrap a heading with a divider on each side.
fn banner(heading: &str) -> String {
    let bar = divider();
    let body = heading.to_string();
    format!("{}\n{}\n{}", bar, body, bar)
}

/// Emit a one-line access record for a caller.
fn log_access(who: &str) {
    let message = who.to_string();
    let stamped = format!("[access] {}", message);
    println!("{}", stamped);
}
RS
}

# After the `feat` edit on top of fix: uppercase the name with --upper (in greet).
write_after_feat() {
  cat > src/main.rs <<'RS'
// Tiny throwaway program for scripts/demo.sh: shows aic splitting one file's
// mixed, unrelated edits into block-level atomic commits.
//
// Three concerns live in three functions, spaced far enough apart that git
// emits one hunk per concern.

/// Program entry: parse args and print a greeting.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: demo <name>");
        std::process::exit(1);
    }
    let name = &args[1];
    println!("{}", greet(name));
}

/// A decorative separator used when printing headings.
fn divider() -> String {
    let bar = "=".repeat(20);
    bar
}

/// Build a friendly greeting for a name.
fn greet(name: &str) -> String {
    let upper = std::env::args().any(|a| a == "--upper");
    let display = if upper { name.to_uppercase() } else { name.to_string() };
    format!("Hello, {}", display)
}

/// Wrap a heading with a divider on each side.
fn banner(heading: &str) -> String {
    let bar = divider();
    let body = heading.to_string();
    format!("{}\n{}\n{}", bar, body, bar)
}

/// Emit a one-line access record for a caller.
fn log_access(who: &str) {
    let message = who.to_string();
    let stamped = format!("[access] {}", message);
    println!("{}", stamped);
}
RS
}

# After the `style` edit on top of feat: inline the access-log formatting (in log_access).
write_after_style() {
  cat > src/main.rs <<'RS'
// Tiny throwaway program for scripts/demo.sh: shows aic splitting one file's
// mixed, unrelated edits into block-level atomic commits.
//
// Three concerns live in three functions, spaced far enough apart that git
// emits one hunk per concern.

/// Program entry: parse args and print a greeting.
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("usage: demo <name>");
        std::process::exit(1);
    }
    let name = &args[1];
    println!("{}", greet(name));
}

/// A decorative separator used when printing headings.
fn divider() -> String {
    let bar = "=".repeat(20);
    bar
}

/// Build a friendly greeting for a name.
fn greet(name: &str) -> String {
    let upper = std::env::args().any(|a| a == "--upper");
    let display = if upper { name.to_uppercase() } else { name.to_string() };
    format!("Hello, {}", display)
}

/// Wrap a heading with a divider on each side.
fn banner(heading: &str) -> String {
    let bar = divider();
    let body = heading.to_string();
    format!("{}\n{}\n{}", bar, body, bar)
}

/// Emit a one-line access record for a caller.
fn log_access(who: &str) {
    println!("[access] {who}");
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

# Print the config dir aic uses for a given $HOME (mirrors the `dirs` crate:
# ~/Library/Application Support on macOS, ~/.config everywhere else).
config_dir_for_home() {
  case "$(uname -s)" in
    Darwin) printf '%s/Library/Application Support/aic' "$1" ;;
    *)      printf '%s/.config/aic' "$1" ;;
  esac
}

# Write an isolated aic config.toml for the demo provider into a throwaway
# HOME and print that HOME. aic no longer reads env vars, so a config file is
# the only way to pin a provider for the live run. Your real config is untouched.
ensure_demo_config() {
  local provider="${AIC_DEMO_PROVIDER:-ollama}"
  local key="${AIC_DEMO_KEY:-}"
  local model="${AIC_DEMO_MODEL:-}"
  local base_url="${AIC_DEMO_BASE_URL:-}"
  if [[ "$provider" == "ollama" ]]; then
    base_url="${base_url:-${AIC_OLLAMA_URL:-http://localhost:11434}}"
  fi

  FAKE_HOME="$(mktemp -d)"
  local cfgdir; cfgdir="$(config_dir_for_home "$FAKE_HOME")"
  mkdir -p "$cfgdir"
  {
    printf 'backend = "%s"\n' "$provider"
    [[ -n "$key" ]]      && printf 'api_key = "%s"\n' "$key"
    [[ -n "$model" ]]    && printf 'model = "%s"\n' "$model"
    [[ -n "$base_url" ]] && printf 'base_url = "%s"\n' "$base_url"
  } > "$cfgdir/config.toml"
  printf '%s' "$FAKE_HOME"
}

run_live() {
  local aic; aic="$(resolve_aic_bin)" || { note "aic binary not found"; return 1; }

  local provider="${AIC_DEMO_PROVIDER:-ollama}"
  local run_home
  if [[ "$provider" == "ollama" && -z "${AIC_DEMO_KEY:-}" ]]; then
    note "LIVE provider: local Ollama at ${AIC_OLLAMA_URL:-http://localhost:11434}"
    provider_reachable || { note "Ollama not reachable."; return 1; }
  else
    note "LIVE provider: $provider (model=${AIC_DEMO_MODEL:-<default>})"
  fi
  run_home="$(ensure_demo_config)" || return 1

  log "LIVE: applying all three edits at once (mixed working tree)"
  write_after_style           # all three edits combined, nothing staged
  log "aic is splitting the mixed edits into atomic commits…"
  HOME="$run_home" "$aic" </dev/null
}

run_fixture() {
  log "FIXTURE: replaying recorded atomic history (examples/before-after/after.txt)"
  note "apply fix edit";   write_after_fix;   git add -A
  git commit -qm "fix(main): guard against missing name argument"
  note "apply feat edit";  write_after_feat;  git add -A
  git commit -qm "feat(main): add --upper flag to greet"
  note "apply refactor edit"; write_after_style; git add -A
  git commit -qm "refactor(main): simplify log_access with inline formatting"
}

if [[ "${AIC_DEMO_LIVE:-0}" == "1" ]]; then
  if run_live; then
    log "aic finished (live)"
  else
    log "Live run unavailable (no provider or aic missing); falling back to fixture."
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
log "OK: $((commits - 1)) atomic Conventional Commits split from one file's mixed edits."
