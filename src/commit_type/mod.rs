//! The Conventional Commits **vocabulary**: the [`CommitType`] enum, its
//! string parsing, and [`ParsedMessage`] — the single decomposition of a
//! subject line. Purely lexical; color lives in [`crate::palette`]
//! (`CommitType::color_for` is defined there, next to the palette data it
//! reads).

/// Conventional Commit type with associated display color.
///
/// Covers the standard Conventional Commits types plus the common community
/// additions (`build`, `ci`, `revert`, `release`, `deps`, `wip`, `hotfix`,
/// `security`, `improvement`). Anything else maps to [`CommitType::Unknown`],
/// which `CommitType::color_for` (in [`crate::palette`]) then resolves via a
/// deterministic hash fallback (non-empty) or neutral gray (empty) — so
/// unrecognized types still get a stable, readable color instead of
/// collapsing to gray.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitType {
    Feat,
    Fix,
    Chore,
    Docs,
    Style,
    Refactor,
    Perf,
    Test,
    Improvement,
    Ci,
    Build,
    Revert,
    Release,
    Deps,
    Wip,
    Hotfix,
    Security,
    Unknown,
}

/// Decomposed parts of a conventional-commit subject line.
///
/// Produced by [`CommitType::parse_message`]; the single source of truth for
/// type/scope/description so renderers never re-parse the raw message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedMessage<'a> {
    pub commit_type: CommitType,
    /// Type token as it appeared in the message (original case preserved).
    pub type_name: &'a str,
    /// Scope content without parentheses, e.g. `"auth"` in `fix(auth):`.
    /// `None` when no scope is present.
    pub scope: Option<&'a str>,
    /// Description after the first colon, leading whitespace trimmed.
    /// `None` when the message has no colon.
    pub description: Option<&'a str>,
}

impl CommitType {
    /// Parse a commit type string (case-insensitive).
    ///
    /// Recognizes standard Conventional Commits types; anything else maps to
    /// `Unknown`. Empty or whitespace-only strings also map to `Unknown`.
    pub fn parse(input: &str) -> Self {
        match input.trim().to_lowercase().as_str() {
            "feat" => CommitType::Feat,
            "fix" => CommitType::Fix,
            "chore" => CommitType::Chore,
            "docs" => CommitType::Docs,
            "style" => CommitType::Style,
            "refactor" => CommitType::Refactor,
            "perf" => CommitType::Perf,
            "test" => CommitType::Test,
            "improvement" => CommitType::Improvement,
            "ci" => CommitType::Ci,
            "build" => CommitType::Build,
            "revert" => CommitType::Revert,
            "release" => CommitType::Release,
            "deps" => CommitType::Deps,
            "wip" => CommitType::Wip,
            "hotfix" => CommitType::Hotfix,
            "security" => CommitType::Security,
            _ => CommitType::Unknown,
        }
    }

    /// Decompose a conventional-commit subject into typed parts.
    ///
    /// The text before the first colon is the type (with an optional scope in
    /// parentheses); the remainder is the description. Messages without a colon
    /// map to [`CommitType::Unknown`] with `description: None`.
    ///
    /// This is the single parser for the codebase — renderers should consume
    /// the returned [`ParsedMessage`] instead of re-splitting the message.
    ///
    /// # Examples
    /// ```
    /// use aic::commit_type::{CommitType, ParsedMessage};
    ///
    /// let p = CommitType::parse_message("fix(auth): correct token check");
    /// assert_eq!(p.commit_type, CommitType::Fix);
    /// assert_eq!(p.type_name, "fix");
    /// assert_eq!(p.scope, Some("auth"));
    /// assert_eq!(p.description, Some("correct token check"));
    ///
    /// let p = CommitType::parse_message("no colon message");
    /// assert_eq!(p.commit_type, CommitType::Unknown);
    /// assert_eq!(p.description, None);
    /// ```
    pub fn parse_message(message: &str) -> ParsedMessage<'_> {
        match message.split_once(':') {
            Some((type_part, desc)) => {
                let (type_name, scope) = match type_part.split_once('(') {
                    Some((name, rest)) => {
                        // Drop a trailing ')' so callers render the scope without
                        // having to know the original delimiter.
                        let scope = rest.trim_end_matches(')');
                        (name, (!scope.is_empty()).then_some(scope))
                    }
                    None => (type_part, None),
                };
                ParsedMessage {
                    commit_type: CommitType::parse(type_name),
                    type_name,
                    scope,
                    description: Some(desc.trim_start()),
                }
            }
            None => ParsedMessage {
                commit_type: CommitType::Unknown,
                type_name: "",
                scope: None,
                description: None,
            },
        }
    }

    /// Extract just the [`CommitType`] from a conventional-commit message.
    ///
    /// Thin convenience wrapper over [`CommitType::parse_message`] for callers
    /// that don't need the scope or description.
    ///
    /// # Examples
    /// ```
    /// use aic::commit_type::CommitType;
    ///
    /// assert_eq!(CommitType::from_message("feat: add thing"), CommitType::Feat);
    /// assert_eq!(CommitType::from_message("FEAT: add thing"), CommitType::Feat);
    /// assert_eq!(CommitType::from_message("fix: bug"), CommitType::Fix);
    /// assert_eq!(CommitType::from_message("fix(auth): correct token check"), CommitType::Fix);
    /// assert_eq!(CommitType::from_message("no colon message"), CommitType::Unknown);
    /// ```
    pub fn from_message(message: &str) -> Self {
        CommitType::parse_message(message).commit_type
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_recognizes_all_types() {
        // The 8 original Conventional Commits types.
        assert_eq!(CommitType::parse("feat"), CommitType::Feat);
        assert_eq!(CommitType::parse("fix"), CommitType::Fix);
        assert_eq!(CommitType::parse("chore"), CommitType::Chore);
        assert_eq!(CommitType::parse("docs"), CommitType::Docs);
        assert_eq!(CommitType::parse("style"), CommitType::Style);
        assert_eq!(CommitType::parse("refactor"), CommitType::Refactor);
        assert_eq!(CommitType::parse("perf"), CommitType::Perf);
        assert_eq!(CommitType::parse("test"), CommitType::Test);
        // The 9 expanded types (build/ci/revert + community additions).
        assert_eq!(CommitType::parse("improvement"), CommitType::Improvement);
        assert_eq!(CommitType::parse("ci"), CommitType::Ci);
        assert_eq!(CommitType::parse("build"), CommitType::Build);
        assert_eq!(CommitType::parse("revert"), CommitType::Revert);
        assert_eq!(CommitType::parse("release"), CommitType::Release);
        assert_eq!(CommitType::parse("deps"), CommitType::Deps);
        assert_eq!(CommitType::parse("wip"), CommitType::Wip);
        assert_eq!(CommitType::parse("hotfix"), CommitType::Hotfix);
        assert_eq!(CommitType::parse("security"), CommitType::Security);
    }

    #[test]
    fn parse_is_case_insensitive() {
        assert_eq!(CommitType::parse("FEAT"), CommitType::Feat);
        assert_eq!(CommitType::parse("Fix"), CommitType::Fix);
        assert_eq!(CommitType::parse("ChOrE"), CommitType::Chore);
        assert_eq!(CommitType::parse("  DOCS  "), CommitType::Docs);
    }

    #[test]
    fn parse_unknown_types() {
        // `wip` is now a named type — use genuinely unrecognized tokens here.
        assert_eq!(CommitType::parse("blob"), CommitType::Unknown);
        assert_eq!(CommitType::parse("bogus"), CommitType::Unknown);
        assert_eq!(CommitType::parse(""), CommitType::Unknown);
        assert_eq!(CommitType::parse("   "), CommitType::Unknown);
    }

    #[test]
    fn from_message_extracts_type_before_colon() {
        assert_eq!(
            CommitType::from_message("feat: add thing"),
            CommitType::Feat
        );
        assert_eq!(
            CommitType::from_message("fix(auth): correct token check"),
            CommitType::Fix
        );
        assert_eq!(
            CommitType::from_message("chore: update deps"),
            CommitType::Chore
        );
    }

    #[test]
    fn from_message_handles_case_variants() {
        assert_eq!(
            CommitType::from_message("FEAT: add thing"),
            CommitType::Feat
        );
        assert_eq!(
            CommitType::from_message("Feat: add thing"),
            CommitType::Feat
        );
    }

    #[test]
    fn from_message_handles_scope_with_colon() {
        // Scope before type: "fix(auth)" — should parse correctly
        assert_eq!(
            CommitType::from_message("fix(auth): correct token check"),
            CommitType::Fix
        );
        assert_eq!(
            CommitType::from_message("feat(api): add endpoint"),
            CommitType::Feat
        );
    }

    #[test]
    fn from_message_returns_unknown_for_no_colon() {
        assert_eq!(
            CommitType::from_message("no colon message"),
            CommitType::Unknown
        );
        // A colon-bearing but unrecognized type still resolves to Unknown
        // (the renderer then routes it through the hash fallback — see
        // `color_for`).
        assert_eq!(CommitType::from_message("blob: thing"), CommitType::Unknown);
        assert_eq!(CommitType::from_message(""), CommitType::Unknown);
    }

    #[test]
    fn from_message_handles_multiple_colons() {
        // Only first colon separates type from description
        assert_eq!(
            CommitType::from_message("feat: thing: with: colons"),
            CommitType::Feat
        );
    }

    #[test]
    fn parse_message_extracts_scope_and_type() {
        let p = CommitType::parse_message("fix(auth): correct token check");
        assert_eq!(p.commit_type, CommitType::Fix);
        assert_eq!(p.type_name, "fix");
        assert_eq!(p.scope, Some("auth"));
        assert_eq!(p.description, Some("correct token check"));
    }

    #[test]
    fn parse_message_preserves_type_case() {
        let p = CommitType::parse_message("FEAT(api): add endpoint");
        assert_eq!(p.commit_type, CommitType::Feat);
        assert_eq!(p.type_name, "FEAT");
        assert_eq!(p.scope, Some("api"));
    }

    #[test]
    fn parse_message_without_scope() {
        let p = CommitType::parse_message("feat: add thing");
        assert_eq!(p.commit_type, CommitType::Feat);
        assert_eq!(p.scope, None);
        assert_eq!(p.description, Some("add thing"));
    }

    #[test]
    fn parse_message_empty_scope_is_none() {
        let p = CommitType::parse_message("refactor(): noop");
        assert_eq!(p.commit_type, CommitType::Refactor);
        assert_eq!(p.scope, None);
        assert_eq!(p.description, Some("noop"));
    }

    #[test]
    fn parse_message_no_colon_is_unknown() {
        let p = CommitType::parse_message("no colon message");
        assert_eq!(p.commit_type, CommitType::Unknown);
        assert_eq!(p.type_name, "");
        assert_eq!(p.scope, None);
        assert_eq!(p.description, None);
    }
}
