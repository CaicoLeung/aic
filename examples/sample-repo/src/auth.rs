//! Tiny authentication module used by the aic demo sample repo.
//!
//! It deliberately carries three unrelated concerns (a style cleanup, a bug
//! fix, and a missing feature) so that `aic` can split them into three
//! atomic commits. Each concern lives in its own region of the file, far
//! enough apart that git emits a separate hunk per change.

use std::collections::HashMap;

/// In-memory token store keyed by token string, mapping to a unix expiry.
pub struct Auth {
    tokens: HashMap<String, u64>,
}

impl Auth {
    /// Build an empty store.
    pub fn new() -> Self {
        Self {
            tokens: HashMap::new(),
        }
    }

    // ------------------------------------------------------------------
    // Token validity
    // ------------------------------------------------------------------

    /// Check whether a token is still valid. Returns false for unknown or
    /// expired tokens.
    ///
    /// An expiry is treated as the last second the token is good for: a token
    /// whose expiry equals the current second is still accepted.
    pub fn is_valid(&self, token: &str) -> bool {
        let now = current_unix_time();
        match self.tokens.get(token) {
            // BUG: an expiring token that equals `now` is still valid this
            // second, but the strict `<` below rejects it one second early.
            Some(expiry) => now < *expiry,
            None => false,
        }
    }

    /// Remember a token together with the unix second at which it expires.
    pub fn store(&mut self, token: String, expiry: u64) {
        self.tokens.insert(token, expiry);
    }

    // ------------------------------------------------------------------
    // OAuth2
    // ------------------------------------------------------------------

    // (OAuth2 support is not implemented yet.)
}

fn current_unix_time() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
