#!/usr/bin/env bash
# Sets up a temp git repo with a real merge conflict for the VHS resolve demo.
# Usage: source demo/resolve-setup.sh
set -euo pipefail

DEMO_DIR=$(mktemp -d /tmp/aic-resolve-demo-XXXXXX)
trap 'rm -rf "$DEMO_DIR" /tmp/aic-resolve-demo-path' EXIT
BIN_DIR="$DEMO_DIR/bin"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$BIN_DIR" "$DEMO_DIR/src"

# Fake aic → resolve simulation (ignores args, just plays the resolve flow)
cp "$SCRIPT_DIR/resolve-sim.sh" "$BIN_DIR/aic"
chmod +x "$BIN_DIR/aic"

git init -q "$DEMO_DIR"
git -C "$DEMO_DIR" symbolic-ref HEAD refs/heads/main
cd "$DEMO_DIR"
git config user.email "dev@example.com"
git config user.name "Demo Dev"
echo "bin/" > .gitignore

# Shared base: a config + parser
mkdir -p src
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

export PATH="$BIN_DIR:$PATH"
export PS1='❯ '
echo "$DEMO_DIR" > /tmp/aic-resolve-demo-path
set +e +u +o pipefail
