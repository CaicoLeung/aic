#!/usr/bin/env bash
# Sets up ~/aic-demo — a scratch git repo whose unstaged diff the REAL aic
# binary splits into three atomic commits — for the VHS demo GIF.
# Usage: bash demo/setup.sh
#   then: export PATH="$HOME/aic-demo/bin:$PATH" && cd ~/aic-demo
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO_ROOT/target/release/aic"
if [ ! -x "$BIN" ]; then
  echo "demo needs target/release/aic — run: cargo build --release" >&2
  exit 1
fi

DEMO_DIR="$HOME/aic-demo"
rm -rf "$DEMO_DIR"
mkdir -p "$DEMO_DIR/bin" "$DEMO_DIR/src"
cp "$BIN" "$DEMO_DIR/bin/aic"

git init -q "$DEMO_DIR"
git -C "$DEMO_DIR" symbolic-ref HEAD refs/heads/main
cd "$DEMO_DIR"
git config user.email "dev@example.com"
git config user.name "Demo Dev"
echo "bin/" > .gitignore

cat > src/auth.rs <<'RUST'
use std::collections::HashMap;

pub fn check_token(token: &str) -> bool {
    let expiry = get_expiry(token);
    if expiry > now() {
        return false;
    }
    true
}

fn get_expiry(token: &str) -> u64 { 0 }
fn now() -> u64 { 0 }
RUST

git add -A
git commit -q -m "feat(auth): initial authentication module"

# Now make 3 unrelated changes to the same file (fix + feat + style)
cat > src/auth.rs <<'RUST'
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

pub fn check_token(token: &str) -> bool {
    let expiry = get_expiry(token);
    if expiry < now() {
        return false;
    }
    true
}

fn get_expiry(token: &str) -> u64 { 0 }
fn now() -> u64 { 0 }

pub fn login_oauth2(provider: &str) -> Option<String> { None }
RUST

# Leave changes unstaged — aic will detect and split them
