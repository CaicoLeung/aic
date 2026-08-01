use console::Style;

/// Conventional Commit type with associated display color.
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
            _ => CommitType::Unknown,
        }
    }

    /// Display color for this commit type.
    pub fn color(self) -> Style {
        match self {
            // Green for features — positive change
            CommitType::Feat => Style::new().true_color(74, 222, 128),
            // Yellow/Orange for fixes — attention, matches hash theme
            CommitType::Fix => Style::new().true_color(251, 191, 36),
            // Gray for chores — neutral, low-priority
            CommitType::Chore => Style::new().true_color(156, 163, 175),
            // Blue for docs — information, reference
            CommitType::Docs => Style::new().true_color(96, 165, 250),
            // Purple for style — cosmetic change
            CommitType::Style => Style::new().true_color(167, 139, 250),
            // Cyan for refactor — structural change
            CommitType::Refactor => Style::new().true_color(34, 211, 238),
            // Red for perf — speed, urgency
            CommitType::Perf => Style::new().true_color(248, 113, 113),
            // Pink for test — quality, validation
            CommitType::Test => Style::new().true_color(244, 114, 182),
            // Gray fallback for unknown types
            CommitType::Unknown => Style::new().true_color(156, 163, 175),
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
    /// use aic::types::{CommitType, ParsedMessage};
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
    /// use aic::types::CommitType;
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
        assert_eq!(CommitType::parse("feat"), CommitType::Feat);
        assert_eq!(CommitType::parse("fix"), CommitType::Fix);
        assert_eq!(CommitType::parse("chore"), CommitType::Chore);
        assert_eq!(CommitType::parse("docs"), CommitType::Docs);
        assert_eq!(CommitType::parse("style"), CommitType::Style);
        assert_eq!(CommitType::parse("refactor"), CommitType::Refactor);
        assert_eq!(CommitType::parse("perf"), CommitType::Perf);
        assert_eq!(CommitType::parse("test"), CommitType::Test);
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
        assert_eq!(CommitType::parse("wip"), CommitType::Unknown);
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
        assert_eq!(CommitType::from_message("WIP: thing"), CommitType::Unknown);
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
