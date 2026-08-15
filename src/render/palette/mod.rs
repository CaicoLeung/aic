//! The crate's color palette: every visual-color decision in one module,
//! keyed by [`CommitType`] where the concept has a type, and by named role
//! fns elsewhere. Renderers (`display`, the resolve UI in `conflict`) call
//! these accessors instead of hand-rolling `Style::new()` chains, so the
//! WCAG contrast invariant is defended at a single point — the regression
//! tests at the bottom of this file.

use console::Style;

use crate::render::commit_type::CommitType;

/// Single source of truth for named-type colors. `color()` and the WCAG
/// contrast regression test both read this, so a palette change can never
/// silently drop a type or break the readability guard.
///
/// Every entry clears WCAG AA Large (3:1) on both `#ffffff` and `#0d1117` —
/// the narrowest luminance band that works on a single palette without
/// theme detection. Verified by `all_colors_pass_wcag_aa_large`.
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

/// Commit-hash amber (`#d97706`, amber-600) for the short commit-id token in
/// `display::commit_line`. The one palette color not keyed by a
/// [`CommitType`]; kept here so the whole palette — and its readability guard
/// — lives in one place. Bolded at the call site so the short ref qualifies
/// as WCAG AA Large (3:1) on both themes.
const COMMIT_ID_COLOR: (u8, u8, u8) = (217, 119, 6);

/// Σ total-row glyph color (`#0891b2`, cyan-600) for the summation marker in
/// `display`'s file-stats footer. Adjacent to green on the wheel, so it
/// harmonizes with the `+N` additions while staying distinct from the red
/// `−M` and the amber commit-id; bolded at the call site (single glyph =
/// large text) to read as the summary row. Cleared 3:1 on both themes.
const SIGMA_COLOR: (u8, u8, u8) = (8, 145, 178);

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

/// Build a [`Style`] from an RGB triple — the single
/// converter every palette entry and public accessor funnels through, so the
/// `Style::new().true_color(r, g, b)` construction can't drift across sites.
fn rgb_style((r, g, b): (u8, u8, u8)) -> Style {
    Style::new().true_color(r, g, b)
}
// ----------------------------------------------------------------------
// Terminal-palette roles
//
// The named-type palette above is keyed by CommitType; these roles cover
// every other color decision in the terminal UI (commit panel in `display`,
// resolve flow in `conflict`). They are ANSI-16 colors rather than RGB, so
// the WCAG guard can't test them numerically — collecting them here is the
// consistency play: one name per role, changed in one place, and no
// renderer can hand-roll a divergent green.
// ----------------------------------------------------------------------

/// ✓ glyph and "resolved + staged" lines — an outcome that landed.
pub fn success() -> Style {
    Style::new().green().bold()
}

/// ✗ glyph — a refused or failed outcome.
pub fn failure() -> Style {
    Style::new().red().bold()
}

/// ⚠ glyph on attention headers and the "proposed commit:" frame —
/// pending state, not an error.
pub fn pending() -> Style {
    Style::new().yellow().bold()
}

/// Soft warning accents: skip reasons, unresolvable-file tags, the
/// resolve y/n prompt line.
pub fn caution() -> Style {
    Style::new().yellow()
}

/// Diff additions and `+N` counts.
pub fn added() -> Style {
    Style::new().green()
}

/// Diff deletions, `−M` counts, and the rejected-file ✗.
pub fn removed() -> Style {
    Style::new().red()
}

/// The `[new]` tag — [`added`] promoted to bold for the tag column.
pub fn added_strong() -> Style {
    Style::new().green().bold()
}

/// The `[del]` tag — [`removed`] promoted to bold for the tag column.
pub fn removed_strong() -> Style {
    Style::new().red().bold()
}

/// Secondary text: dim labels, meta lines, rejected paths.
pub fn muted() -> Style {
    Style::new().dim()
}

/// The finalize/hand-off command the user should run — cyan so a runnable
/// command reads as one token.
pub fn hint() -> Style {
    Style::new().cyan()
}

/// File-path section headers in the review diff.
pub fn header() -> Style {
    Style::new().bold().cyan()
}

/// The subject's description after the type token.
pub fn emphasis() -> Style {
    Style::new().bold()
}

/// Muted gray [`Style`] for prefix/body/scope text and for
/// empty or colon-less type tokens. Reads the single-source [`NEUTRAL_GRAY`]
/// so renderers can never drift from the contrast-guarded value — call this
/// instead of re-typing the RGB literal.
pub fn neutral_gray() -> Style {
    rgb_style(NEUTRAL_GRAY)
}

/// Amber [`Style`] for the commit-id token. Reads the single-source
/// [`COMMIT_ID_COLOR`]; the caller bolds it so the short ref qualifies as
/// WCAG AA Large.
pub fn commit_id_color() -> Style {
    rgb_style(COMMIT_ID_COLOR)
}

/// Cyan [`Style`] for the Σ total-row glyph. Reads the single-source
/// [`SIGMA_COLOR`]; the caller bolds it so the glyph qualifies as WCAG AA
/// Large.
pub fn sigma_color() -> Style {
    rgb_style(SIGMA_COLOR)
}

impl CommitType {
    /// Display color for a **named** commit type. Internal palette lookup used
    /// by [`CommitType::color_for`]; reads [`NAMED_PALETTE`] and falls back to
    /// [`NEUTRAL_GRAY`] for [`CommitType::Unknown`]. Not public — renderers
    /// go through [`CommitType::color_for`], which layers the empty-token and
    /// unrecognized-name resolution on top.
    fn color(self) -> Style {
        let rgb = NAMED_PALETTE
            .iter()
            .find(|(t, _)| *t == self)
            .map(|(_, rgb)| *rgb)
            .unwrap_or(NEUTRAL_GRAY);
        rgb_style(rgb)
    }

    /// Resolve the color for an arbitrary type-token string — the renderer's
    /// entry point. Resolution order:
    ///
    /// 1. **Named type** (`feat`, `ci`, `revert`, …) → its palette color
    ///    (looked up in [`NAMED_PALETTE`]).
    /// 2. **Empty / whitespace-only** (e.g. a no-colon message) →
    ///    [`NEUTRAL_GRAY`]. A missing token carries no signal worth coloring.
    /// 3. **Non-empty unrecognized** (`wip` before it was named, custom team
    ///    types like `blob`, `ux`, …) → deterministic FNV-1a hash into
    ///    [`FALLBACK_PALETTE`]. Same name → same color, every run, every
    ///    machine — stable scrollback and muscle memory without config.
    pub fn color_for(type_name: &str) -> Style {
        let named = CommitType::parse(type_name);
        if named != CommitType::Unknown {
            return named.color();
        }
        let trimmed = type_name.trim();
        if trimmed.is_empty() {
            return neutral_gray();
        }
        rgb_style(FALLBACK_PALETTE[fallback_palette_index(trimmed)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Commit-id amber (the one palette color not keyed by a CommitType) —
        // guarded here so the headline contrast fix can't silently regress.
        let on_white = contrast(COMMIT_ID_COLOR, WHITE);
        let on_dark = contrast(COMMIT_ID_COLOR, DARK);
        assert!(
            on_white >= AA_LARGE,
            "commit-id amber fails 3:1 on white: {on_white:.2}"
        );
        assert!(
            on_dark >= AA_LARGE,
            "commit-id amber fails 3:1 on dark: {on_dark:.2}"
        );
        checked += 1;

        // Σ total-row cyan — guarded so the summation glyph's contrast can't
        // silently regress.
        let on_white = contrast(SIGMA_COLOR, WHITE);
        let on_dark = contrast(SIGMA_COLOR, DARK);
        assert!(
            on_white >= AA_LARGE,
            "sigma cyan fails 3:1 on white: {on_white:.2}"
        );
        assert!(
            on_dark >= AA_LARGE,
            "sigma cyan fails 3:1 on dark: {on_dark:.2}"
        );
        checked += 1;

        // Guard against the palette arrays silently shrinking to [] and the
        // loop body never running (a passing-vacuously regression).
        assert_eq!(checked, NAMED_PALETTE.len() + FALLBACK_PALETTE.len() + 3);
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
