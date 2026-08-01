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

    /// Extract the commit type from a conventional commit message.
    ///
    /// Parses the text before the first colon as the type. Handles conventional
    /// commit scopes like "fix(auth):" by extracting just the type before any
    /// parentheses. If no colon is present, returns `Unknown`.
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
        message
            .split_once(':')
            .map(|(type_part, _)| {
                // Strip scope if present: "fix(auth)" -> "fix"
                let type_only = type_part.split('(').next().unwrap_or(type_part);
                CommitType::parse(type_only)
            })
            .unwrap_or(CommitType::Unknown)
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
}
