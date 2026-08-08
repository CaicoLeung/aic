#!/usr/bin/env bash
# docs/demo/demo-run.sh
#
# Runs the canonical aic hunk-split demo: ONE file edited in three unrelated
# ways becomes THREE clean atomic commits. This is the scenario the README
# headline promises, recorded for docs/demo/aic-hunk-split.gif.
#
# The run is fully isolated — a throwaway git repo and a throwaway aic config
# under a scratch HOME — so it never touches your real repositories or your
# real ~/.config/aic (or ~/Library/Application Support/aic) config.
#
# Provider resolution (first match wins):
#   1. AIC_DEMO_BACKEND + AIC_DEMO_API_KEY (+ optional AIC_DEMO_MODEL) env vars
#      — used by docs/demo/record.sh and CI to pin a provider.
#   2. Your existing aic config (copied into the scratch HOME, with the
#      pre-commit confirmation disabled so the run stays non-interactive).
#   3. A running local Ollama (http://localhost:11434) — no API key required.
#   4. Otherwise: print setup instructions and exit non-zero.
#
# Usage:
#   ./docs/demo/demo-run.sh            # run the demo live in your terminal
#   AIC_DEMO_BACKEND=ollama ./docs/demo/demo-run.sh
#
# Exits non-zero if no provider can be resolved or aic fails.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"

# --- locate the aic binary -------------------------------------------------
aic_bin="${AIC_BIN:-}"
if [ -z "$aic_bin" ]; then
  if command -v aic >/dev/null 2>&1; then
    aic_bin="$(command -v aic)"
  elif [ -x "$REPO_ROOT/target/release/aic" ]; then
    aic_bin="$REPO_ROOT/target/release/aic"
  else
    echo "aic not found. Build it first:  cargo build --release" >&2
    echo "(or install it: https://github.com/CaicoLeung/aic#installation)" >&2
    exit 1
  fi
fi

# --- scratch isolation -----------------------------------------------------
SCRATCH="$(mktemp -d -t aic-demo-XXXXXX)"
trap 'rm -rf "$SCRATCH"' EXIT
DEMO_HOME="$SCRATCH/home"
mkdir -p "$DEMO_HOME"
AIC_CFG_DIR="$DEMO_HOME/Library/Application Support/aic"
mkdir -p "$AIC_CFG_DIR"
AIC_CFG="$AIC_CFG_DIR/config.toml"

# --- resolve a provider config into the scratch HOME -----------------------
# NOTE: build the optional `model` line out of the heredoc — `${3:+model =
# "$3"}` would drop the inner quotes (parameter expansion consumes them),
# yielding invalid TOML and a silent fallback to the default provider.
write_cfg() { # backend api_key [model]
  local model_line=""
  [ -n "${3:-}" ] && model_line="model = \"$3\""
  cat > "$AIC_CFG" <<TOML
backend = "$1"
api_key = "$2"
$model_line
TOML
}

# Print the user's config path on macOS (~/Library/Application Support) or
# Linux (~/.config). Returns the path on stdout if it exists.
aic_user_config() {
  for c in \
    "$1/Library/Application Support/aic/config.toml" \
    "$1/.config/aic/config.toml"; do
    if [ -f "$c" ]; then printf '%s' "$c"; return 0; fi
  done
  return 1
}

if [ -n "${AIC_DEMO_BACKEND:-}" ] && [ -n "${AIC_DEMO_API_KEY:-}" ]; then
  write_cfg "$AIC_DEMO_BACKEND" "$AIC_DEMO_API_KEY" "${AIC_DEMO_MODEL:-}"
elif cfg="$(aic_user_config "$HOME" 2>/dev/null)"; then
  # Reuse the user's configured provider, but force non-interactive commits.
  sed 's/^confirm_before_commit = true/confirm_before_commit = false/' "$cfg" \
    > "$AIC_CFG"
elif curl -s --max-time 2 http://localhost:11434/api/tags >/dev/null 2>&1; then
  write_cfg ollama "" "${AIC_DEMO_MODEL:-llama3.3}"
else
  cat >&2 <<'MSG'
No provider configured for the demo. Do ONE of:
  1. Run `aic setup` to configure a provider (OpenAI, Anthropic, DeepSeek, …).
  2. Start Ollama locally (https://ollama.com) and `ollama pull llama3.3`.
  3. Export AIC_DEMO_BACKEND + AIC_DEMO_API_KEY (e.g. for CI / re-recording).
MSG
  exit 1
fi

# --- build the fixture repo: one file, three unrelated edits ---------------
FIXTURE="$SCRATCH/repo"
mkdir -p "$FIXTURE/src"
cd "$FIXTURE"
git init -q
git config user.email "demo@aic.dev"
git config user.name "aic demo"

cat > src/auth.rs <<'RUST'
//! Authentication helpers: token validation and session loading.

use std::collections::HashMap;

/// Build the default session store.
pub fn new_session_store() -> HashMap<String, Session> {
    HashMap::new()
}

/// A user session.
pub struct Session {
    pub user_id: u64,
    pub token: String,
}

/// Validate an access token. Returns false when the token is expired.
pub fn check_token(token: &str) -> bool {
    let expires_at = parse_expiry(token);
    let now = current_time();
    if now > expires_at {
        return false;
    }
    true
}

/// Parse the expiry timestamp encoded in a token.
pub fn parse_expiry(token: &str) -> u64 {
    let mut sum = 0u64;
    for byte in token.bytes() {
        sum = sum.wrapping_add(byte as u64);
    }
    sum
}

/// Return the current wall-clock time as unix seconds.
pub fn current_time() -> u64 {
    1_700_000_000
}

/// Load a session for a user id, if one exists.
pub fn load_session(user_id: u64, store: &HashMap<String, Session>) -> Option<&Session> {
    store.get(&user_id.to_string())
}
RUST
git add -A
git commit -qm "initial: auth helpers"

# Three maximally-unrelated edits, far enough apart that git emits three
# hunks and aic splits them into three atomic commits. Each is a different
# Conventional-Commits type (docs / fix / feat) so the split is unambiguous.
python3 - <<'PY'
from pathlib import Path
p = Path("src/auth.rs")
s = p.read_text()
# hunk 1 — docs: expand the module doc comment (top of file)
s = s.replace(
    "//! Authentication helpers: token validation and session loading.\n",
    "//! Authentication helpers.\n//!\n//! Token validation and session loading for the auth module.\n",
    1,
)
# hunk 2 — fix: the token-expiry boundary (off-by-one: > should be >=)
s = s.replace("if now > expires_at", "if now >= expires_at", 1)
# hunk 3 — feat: add an OAuth2 login provider (new function)
s = s.replace(
    "/// Load a session for a user id, if one exists.",
    '/// Exchange an OAuth2 authorization code for an access token.\n'
    'pub fn oauth2_login(code: &str) -> Result<String, String> {\n'
    '    if code.is_empty() {\n'
    '        return Err("empty code".into());\n'
    '    }\n'
    '    Ok(format!("token-{code}"))\n'
    '}\n\n'
    '/// Load a session for a user id, if one exists.',
    1,
)
p.write_text(s)
PY

# --- the on-screen narrative (recorded by asciinema) -----------------------
bold() { printf '\033[1m%s\033[0m\n' "$*"; }
cmd()  { printf '\n\033[1;32m$\033[0m %s\n' "$*"; }
pause(){ sleep "${1:-1}"; }

bold "aic — hunk-level atomic commits (one file, three unrelated edits)"
pause 1

cmd "git diff --stat"
git --no-pager diff --stat
pause 1

cmd "git --no-pager diff"
git --no-pager diff
pause 2

cmd "aic            # nothing staged → aic splits every hunk into atomic commits"
pause 1
HOME="$DEMO_HOME" "$aic_bin"
pause 1

cmd "git log --oneline"
git --no-pager log --oneline
pause 2

bold "✓ one file in, three atomic commits out — no manual git add -p."
