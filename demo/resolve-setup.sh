#!/usr/bin/env bash
# Sets up ~/aic-resolve — a scratch git repo with a real merge conflict —
# for the VHS resolve demo GIF, driven by the REAL aic binary.
# Usage: bash demo/resolve-setup.sh
#   then: export PATH="$HOME/aic-resolve/bin:$PATH" && cd ~/aic-resolve
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_ROOT/target/release/aic"
if [ ! -x "$BIN" ]; then
  echo "demo needs target/release/aic — run: cargo build --release" >&2
  exit 1
fi

DEMO_DIR="$HOME/aic-resolve"
rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR/bin" "$DEMO_DIR/src"
cp "$BIN" "$DEMO_DIR/bin/aic"

git init -q "$DEMO_DIR"
git -C "$DEMO_DIR" symbolic-ref HEAD refs/heads/main
cd "$DEMO_DIR"
git config user.email "dev@example.com"
git config user.name "Demo Dev"
echo "bin/" > .gitignore

# Shared base: a config + parser
cat > src/config.rs <<'RUST'
pub struct Config {
    timeout_secs: u32,
    retries: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config { timeout_secs: 30, retries: 2 }
    }
}
RUST

cat > src/parser.rs <<'RUST'
pub fn parse(input: &str) -> Vec<String> {
    let tokens = tokenize(input);
    tokens.iter().map(|t| t.to_string()).collect()
}
RUST

git add -A
git commit -q -m "feat: add config and parser"

# Branch A: change timeout + error handling
git checkout -q -b feature-a
cat > src/config.rs <<'RUST'
pub struct Config {
    timeout_secs: u32,
    retries: u32,
}

impl Default for Config {
    fn default() -> Self {
        Config { timeout_secs: 60, retries: 2 }
    }
}
RUST

cat > src/parser.rs <<'RUST'
pub fn parse(input: &str) -> Result<Config> {
    let tokens = tokenize(input);
    Ok(Config::from_tokens(tokens)?)
}
RUST

git add -A
git commit -q -m "refactor: improve timeout and error handling"

# Back to main: different changes to same files
git checkout -q main
cat > src/config.rs <<'RUST'
pub struct Config {
    timeout_secs: u32,
    retries: u32,
    confirm_before_commit: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config { timeout_secs: 30, retries: 2, confirm_before_commit: true }
    }
}
RUST

cat > src/parser.rs <<'RUST'
pub fn parse(input: &str) -> Vec<String> {
    let tokens = tokenize(input);
    tokens.iter().rev().map(|t| t.to_string()).collect()
}
RUST

git add -A
git commit -q -m "feat: add confirm flag and reverse parse order"

# Merge → conflict
git merge -q feature-a 2>/dev/null || true
