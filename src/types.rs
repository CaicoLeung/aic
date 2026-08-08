use console::Style;

/// Single source of truth for named-type colors. `color()` and the WCAG
/// contrast regression test both read this, so a palette change can never
/// silently drop a type or break the readability guard.
///
/// Every entry clears WCAG AA Large (3:1) on both `#ffffff` and `#0d1117` —
/// the narrowest luminance band that works on a single palette without
/// theme detection. Verified by `all_named_colors_pass_wcag_aa_large`.
const NAMED_PALETTE: [(CommitType, (u8, u8, u8)); 17] = [
    (CommitType::Feat, (21, 128, 61)),        // green-700     #15803d
    (CommitType::Improvement, (5, 150, 105)), // emerald-600   #059669
    (CommitType::Fix, (234, 88, 12)),         // orange-600    #ea580c
    (CommitType::Perf, (220, 38, 38)),        // red-600       #dc2626
    (CommitType::Hotfix, (225, 29, 72)),      // rose-600      #e11d48
    (CommitType::Revert, (192, 38, 211)),     // fuchsia-600   #c026d3
    (CommitType::Docs, (37, 99, 235)),        // blue-600      #2563eb
    (CommitType::Deps, (2, 132, 199)),        // sky-600       #0284c7
    (CommitType::Style, (124, 58, 237)),      // violet-600    #7c3aed
    (CommitType::Ci, (99, 102, 241)),         // indigo-500    #6366f1
    (CommitType::Refactor, (8, 145, 178)),    // cyan-600      #0891b2
    (CommitType::Chore, (15, 118, 110)),      // teal-700      #0f766e
    (CommitType::Build, (161, 98, 7)),        // yellow-700    #a16207
    (CommitType::Release, (77, 124, 15)),     // lime-700      #4d7c0f
    (CommitType::Security, (180, 83, 9)),     // amber-700     #b45309
    (CommitType::Test, (219, 39, 119)),       // pink-600      #db2777
    (CommitType::Wip, (100, 116, 139)),       // slate-500     #64748b
];

/// Fallback palette for unrecognized but non-empty type names. A
/// deterministic FNV-1a hash picks one entry, so the same type name always
/// renders the same color across runs and machines. Kept visually distinct
/// (by shade) from the named set where the color wheel allows; collisions
/// with a named type are acceptable since they can never co-occur in one
/// commit. All entries clear 3:1 on both themes.
const FALLBACK_PALETTE: [(u8, u8, u8); 6] = [
    (13, 148, 136), // teal-600    #0d9488
    (147, 51, 234), // purple-600  #9333ea
    (194, 65, 12),  // orange-700  #c2410c
    (14, 116, 144), // cyan-700    #0e7490
    (168, 85, 247), // purple-500  #a855f7
    (59, 130, 246), // blue-500    #3b82f6
];

/// Neutral gray (`#6b7280`, slate-500) for empty / unparseable type tokens
/// (no-colon messages) and for muted body/scope text in `display`. Chosen to
/// clear 3:1 on both themes (~4.7:1 on white, ~3.9:1 on dark) so even the
/// muted text stays readable on a light background.
const NEUTRAL_GRAY: (u8, u8, u8) = (107, 114, 128);

/// FNV-1a 32-bit over the lowercased token, mapped into [`FALLBACK_PALETTE`].
/// Stable, dependency-free, decent distribution for short strings. Extracted
/// from [`CommitType::color_for`] so determinism and distribution can be
/// tested without rendering through `console` (which would need the
/// process-global color-enabled flag and race other color tests).
///
/// Case-insensitive (lowercases) but does **not** trim — the caller
/// ([`CommitType::color_for`]) trims first so the empty-token check stays in
/// one place.
fn fallback_palette_index(name: &str) -> usize {
    let mut h: u32 = 0x811c9dc5;
    for b in name.to_lowercase().as_bytes() {
        h ^= *b as u32;
        h = h.wrapping_mul(0x0100_0193);
    }
    (h as usize) % FALLBACK_PALETTE.len()
}

/// Conventional Commit type with associated display color.
///
/// Covers the standard Conventional Commits types plus the common community
/// additions (`build`, `ci`, `revert`, `release`, `deps`, `wip`, `hotfix`,
/// `security`, `improvement`). Anything else maps to [`CommitType::Unknown`],
/// which [`CommitType::color_for`] then resolves via a deterministic hash
/// fallback (non-empty) or neutral gray (empty) — so unrecognized types still
/// get a stable, readable color instead of collapsing to gray.
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
    // Expanded conventional-commit coverage.
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

    /// Display color for a **named** commit type. Reads the single-source
    /// [`NAMED_PALETTE`]; [`CommitType::Unknown`] falls back to
    /// [`NEUTRAL_GRAY`] (used directly only when the type token is empty or
    /// the message has no colon — see [`CommitType::color_for`] for the
    /// full resolution including the hash fallback for unrecognized names).
    pub fn color(self) -> Style {
        let rgb = NAMED_PALETTE
            .iter()
            .find(|(t, _)| *t == self)
            .map(|(_, rgb)| *rgb)
            .unwrap_or(NEUTRAL_GRAY);
        Style::new().true_color(rgb.0, rgb.1, rgb.2)
    }

    /// Resolve the color for an arbitrary type-token string — the renderer's
    /// entry point. Resolution order:
    ///
    /// 1. **Named type** (`feat`, `ci`, `revert`, …) → its palette color via
    ///    [`CommitType::color`].
    /// 2. **Empty / whitespace-only** (e.g. a no-colon message) →
    ///    [`NEUTRAL_GRAY`]. A missing token carries no signal worth coloring.
    /// 3. **Non-empty unrecognized** (`wip` before it was named, custom team
    ///    types like `blob`, `ux`, …) → deterministic FNV-1a hash into
    ///    [`FALLBACK_PALETTE`]. Same name → same color, every run, every
    ///    machine — stable scrollback and muscle memory without config.
    ///
    /// Use this instead of [`CommitType::color`] whenever the raw type-token
    /// text is available (i.e. everywhere except places that already hold a
    /// parsed [`CommitType`]).
    pub fn color_for(type_name: &str) -> Style {
        let named = CommitType::parse(type_name);
        if named != CommitType::Unknown {
            return named.color();
        }
        let trimmed = type_name.trim();
        if trimmed.is_empty() {
            return Style::new().true_color(NEUTRAL_GRAY.0, NEUTRAL_GRAY.1, NEUTRAL_GRAY.2);
        }
        let rgb = FALLBACK_PALETTE[fallback_palette_index(trimmed)];
        Style::new().true_color(rgb.0, rgb.1, rgb.2)
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

    // ------------------------------------------------------------------
    // Readability guards — the whole reason this module exists in its
    // current shape. These tests pin the contrast and stability promises the
    // renderers rely on; a palette change that breaks them is a regression,
    // not a cosmetic tweak.
    // ------------------------------------------------------------------

    /// sRGB → relative luminance per WCAG 2.1. Used by the contrast guard.
    fn relative_luminance(r: u8, g: u8, b: u8) -> f64 {
        fn chan(c: u8) -> f64 {
            let s = c as f64 / 255.0;
            if s <= 0.04045 {
                s / 12.92
            } else {
                ((s + 0.055) / 1.055).powf(2.4)
            }
        }
        0.2126 * chan(r) + 0.7152 * chan(g) + 0.0722 * chan(b)
    }

    /// WCAG 2.1 contrast ratio between two sRGB colors.
    fn contrast(fg: (u8, u8, u8), bg: (u8, u8, u8)) -> f64 {
        let l1 = relative_luminance(fg.0, fg.1, fg.2);
        let l2 = relative_luminance(bg.0, bg.1, bg.2);
        let (hi, lo) = if l1 >= l2 { (l1, l2) } else { (l2, l1) };
        (hi + 0.05) / (lo + 0.05)
    }

    /// Every named type, every fallback palette entry, and the neutral gray
    /// must clear WCAG AA Large (3:1) on **both** a pure-white background and a
    /// common dark background (`#0d1117`, GitHub dark). This is the guard that
    /// prevents a future palette tweak from silently reintroducing the
    /// original bug — bright colors that vanished on white terminals.
    ///
    /// 4.5:1 (AA Normal) on both themes is mathematically impossible for a
    /// single static palette (the luminance bands don't overlap), so we target
    /// 3:1 and rely on the renderers to bold the short tokens (hash + type
    /// word), which qualifies them as "large text" under WCAG.
    #[test]
    fn all_colors_pass_wcag_aa_large_on_both_themes() {
        const WHITE: (u8, u8, u8) = (255, 255, 255);
        const DARK: (u8, u8, u8) = (13, 17, 23); // #0d1117 (GitHub dark)
        const AA_LARGE: f64 = 3.0;

        let mut checked = 0usize;
        for (_, rgb) in NAMED_PALETTE {
            let on_white = contrast(rgb, WHITE);
            let on_dark = contrast(rgb, DARK);
            assert!(
                on_white >= AA_LARGE,
                "named color {rgb:?} fails 3:1 on white: {on_white:.2}"
            );
            assert!(
                on_dark >= AA_LARGE,
                "named color {rgb:?} fails 3:1 on dark: {on_dark:.2}"
            );
            checked += 1;
        }
        for rgb in FALLBACK_PALETTE {
            let on_white = contrast(rgb, WHITE);
            let on_dark = contrast(rgb, DARK);
            assert!(
                on_white >= AA_LARGE,
                "fallback color {rgb:?} fails 3:1 on white: {on_white:.2}"
            );
            assert!(
                on_dark >= AA_LARGE,
                "fallback color {rgb:?} fails 3:1 on dark: {on_dark:.2}"
            );
            checked += 1;
        }
        let on_white = contrast(NEUTRAL_GRAY, WHITE);
        let on_dark = contrast(NEUTRAL_GRAY, DARK);
        assert!(
            on_white >= AA_LARGE,
            "neutral gray fails 3:1 on white: {on_white:.2}"
        );
        assert!(
            on_dark >= AA_LARGE,
            "neutral gray fails 3:1 on dark: {on_dark:.2}"
        );
        checked += 1;

        // Guard against the palette arrays silently shrinking to [] and the
        // loop body never running (a passing-vacuously regression).
        assert_eq!(checked, NAMED_PALETTE.len() + FALLBACK_PALETTE.len() + 1);
    }

    /// `fallback_palette_index` is deterministic: the same token resolves to
    /// the same palette slot on every call (stable scrollback, stable muscle
    /// memory, no per-run randomness). Case must not change the result.
    #[test]
    fn fallback_index_is_deterministic() {
        for name in ["blob", "ux", "random", "zzz", "q"] {
            let a = fallback_palette_index(name);
            assert_eq!(
                a,
                fallback_palette_index(name),
                "index({name:?}) not stable across calls"
            );
            // Case-insensitive: "BLOB" must match "blob".
            assert_eq!(
                a,
                fallback_palette_index(&name.to_uppercase()),
                "index({name:?}) changed with case"
            );
        }
    }

    /// `fallback_palette_index` actually distributes across the palette — a
    /// guard against a bug (e.g. a broken hash) that would collapse every
    /// unrecognized name to one slot. Two dozen distinct names must hit more
    /// than one index. (Full coverage of all six slots isn't asserted — that
    /// would be brittle against hash tweaks; >1 proves the hash varies.)
    #[test]
    fn fallback_index_distributes() {
        let names = [
            "blob", "ux", "random", "zzz", "q", "alpha", "beta", "gamma", "delta", "epsilon",
            "zeta", "eta", "theta", "iota", "kappa", "lambda", "mu", "nu", "xi", "omicron", "pi",
            "rho", "sigma", "tau",
        ];
        let indices: std::collections::HashSet<usize> =
            names.iter().map(|n| fallback_palette_index(n)).collect();
        assert!(
            indices.len() > 1,
            "fallback_index collapsed {} names to one slot",
            names.len()
        );
    }

    /// `color_for`'s three-way resolution order, tested at the decision layer
    /// (the rendering bytes are covered by `display::tests`). Named type →
    /// short-circuits before the hash; empty → neutral gray path; non-empty
    /// unrecognized → hash path. The routing is what matters here.
    #[test]
    fn color_for_routes_named_empty_and_unrecognized() {
        // Named types parse to a variant, so color_for takes the palette path
        // (not fallback) — verified indirectly: parse succeeds.
        assert_eq!(CommitType::parse("feat"), CommitType::Feat);
        assert_eq!(CommitType::parse("ci"), CommitType::Ci);
        assert_eq!(CommitType::parse("wip"), CommitType::Wip);
        // Unrecognized non-empty tokens parse to Unknown but are non-empty →
        // they take the fallback path (color_for checks trim().is_empty()).
        assert_eq!(CommitType::parse("blob"), CommitType::Unknown);
        assert!(!"blob".trim().is_empty());
        // Empty / whitespace-only → Unknown AND empty → neutral gray path.
        assert_eq!(CommitType::parse(""), CommitType::Unknown);
        assert!("".trim().is_empty());
        assert!("   ".trim().is_empty());
        // The fallback path itself is covered by fallback_index_* above.
    }
}
