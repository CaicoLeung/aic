#!/usr/bin/env bash
# Sets up a temp git repo + fake `aic` for the VHS demo.
# Usage: source demo/setup.sh  (modifies PATH + cds into the demo repo)
set -euo pipefail

DEMO_DIR=$(mktemp -d /tmp/aic-demo-XXXXXX)
BIN_DIR="$DEMO_DIR/bin"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

mkdir -p "$BIN_DIR" "$DEMO_DIR/src"

# Fake aic → simulation script
cp "$SCRIPT_DIR/aic-sim.sh" "$BIN_DIR/aic"
chmod +x "$BIN_DIR/aic"

# Mini git repo with one file that has 3 distinct changes
git init -q "$DEMO_DIR"
cd "$DEMO_DIR"
git config user.email "dev@example.com"
git config user.name "Demo Dev"

cat > src/auth.rs <<'RUST'
use std::collections::HashMap;

pub fn check_token(token: &str) -> bool {
    let expiry = get_expiry(token);
    if expiry > now() {
        return false;
    }
    true
}

fn get_expiry(token: &str) -> u64 {
    0
}

fn now() -> u64 {
    0
}

pub fn login(user: &str, pass: &str) -> Option<String> {
    None
}
RUST

git add -A
git commit -q -m "feat(auth): initial authentication module"

# Now make 3 unrelated changes to the same file
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

fn get_expiry(token: &str) -> u64 {
    0
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn login(user: &str, pass: &str) -> Option<String> {
    let token = verify_credentials(user, pass)?;
    Some(token)
}

fn verify_credentials(user: &str, pass: &str) -> Option<String> {
    None
}

pub fn login_oauth2(provider: &str, code: &str) -> Option<String> {
    let token = exchange_code(provider, code)?;
    Some(token)
}

fn exchange_code(provider: &str, code: &str) -> Option<String> {
    None
}
RUST

# Leave changes unstaged — aic will detect and split them
export PATH="$BIN_DIR:$PATH"
export PS1='❯ '
echo "$DEMO_DIR" > /tmp/aic-demo-path
