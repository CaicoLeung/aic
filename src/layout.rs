//! Shared terminal-geometry and text-layout primitives.
//!
//! The one place the run's two output surfaces agree on *where text lands and
//! how it wraps*: the static panel engine ([`crate::display::Display`]) and
//! the live progress surface ([`crate::progress`]). Both consume the shared
//! 2-column [`MARGIN`] so every line of stderr — a committed batch's ✓ row, a
//! spinner glyph, a reasoning line — sits at one uniform inset, never flush
//! with the terminal edge. Both resolve the terminal's column count through
//! the single [`resolve_cols`] (`0` → [`FALLBACK_COLS`], capped at
//! [`HARD_CAP`]): the panel engine subtracts its own margins
//! ([`crate::display::Display::text_width`]), the progress surface floors at
//! its own minimum ([`crate::progress`]). And both wrap prose with the single
//! [`wrap_line`] so CJK bodies and long tokens break identically everywhere.
//!
//! These used to live at the bottom of `display.rs`; the progress surface grew
//! up around them and the locality was wrong (the spinner reasoned about
//! `MARGIN`, which is not about panels), so they got their own module (AIC-19).

use console::Term;

/// The actual prefix string for the run's shared 2-column left inset (two
/// spaces), kept as a `&str` so every emitter can prepend it without
/// allocating on every line. Re-exported crate-wide so the spinner templates
/// and the reasoning rows share this single source of truth instead of
/// re-hardcoding the literal. The matching column count lives on the panel
/// engine as its private `LEFT_MARGIN`.
pub(crate) const MARGIN: &str = "  ";

/// Hard ceiling on total line width regardless of how wide the terminal is, so
/// body prose doesn't sprawl into 300-col spaghetti on ultrawide screens.
const HARD_CAP: usize = 100;

/// Terminal width assumed when the real width is unknown (`cols == 0`, i.e.
/// piped / non-TTY output). Matches console's own unix default. Shared by
/// [`resolve_cols`] (the resolution core) and the panel engine's injectable
/// sink constructor ([`crate::display::Display::with`]) so a non-terminal sink
/// lands on the same width the resolver would.
pub(crate) const FALLBACK_COLS: usize = 80;

/// Resolve a raw terminal column count into a usable width — the single
/// resolution shared by the panel engine's text width and the progress
/// surface's [`terminal_width`]. `cols == 0` (non-TTY / piped, where
/// `Term::stderr()` reports no size) falls back to [`FALLBACK_COLS`]; the
/// result is capped at [`HARD_CAP`] so ultrawide terminals don't sprawl body
/// prose. Consumers add their own tail: the panel engine subtracts its
/// margins, the progress surface floors at its minimum usable width.
pub(crate) fn resolve_cols(cols: usize) -> usize {
    let cols = if cols == 0 { FALLBACK_COLS } else { cols };
    cols.min(HARD_CAP)
}

/// Greedy word-wrap of a single line (no embedded newlines) to `width` display
/// columns, counted in `char`s (not bytes) so CJK commit bodies wrap correctly.
///
/// The author's structure is preserved: this only breaks a line *further* —
/// callers feed it one source line at a time so existing newlines stay as hard
/// breaks. A single token longer than `width` (e.g. a long URL) is hard-broken
/// at the boundary; one ugly wrap beats a horizontal overflow.
///
/// `width == 0` disables wrapping entirely — the line returns as one piece,
/// unchanged. The panel engine's `text_width` yields `0` on a sub-margin
/// terminal, so this guard is load-bearing there, not dead code.
///
/// Returns at least one piece; an empty input yields `vec![""]` so blank
/// source lines round-trip as a single empty piece.
pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<char> = Vec::with_capacity(width);
    for word in line.split_whitespace() {
        let w: Vec<char> = word.chars().collect();
        // If the running line can't accept " <word>", flush it first.
        if !cur.is_empty() && cur.len() + 1 + w.len() > width {
            out.push(cur.iter().collect());
            cur.clear();
        }
        if cur.is_empty() {
            // Word starts a new line — hard-break it if it alone exceeds width.
            let mut idx = 0;
            while w.len() - idx > width {
                let chunk: String = w[idx..idx + width].iter().collect();
                out.push(chunk);
                idx += width;
            }
            cur.extend(&w[idx..]);
        } else {
            cur.push(' ');
            cur.extend(&w);
        }
    }
    out.push(cur.iter().collect());
    out
}

/// Read the real terminal's column count and resolve it through
/// [`resolve_cols`] (`0` → [`FALLBACK_COLS`] on a non-TTY / pipe, capped at
/// [`HARD_CAP`]). The single geometry entry point for in-place rendering: the
/// progress surface's spinner + reasoning feed consume this, then apply their
/// own policy (a minimum-usable floor) on top. Pure geometry — no policy — so
/// the panel engine and the progress surface share one notion of "how wide is
/// the terminal" without one inheriting the other's margins or floors.
pub(crate) fn terminal_width() -> usize {
    resolve_cols(Term::stderr().size().1 as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_line_hard_breaks_long_token() {
        // A single token longer than the width is hard-broken at the boundary.
        let pieces = wrap_line("abcdefghij", 4);
        assert_eq!(pieces, vec!["abcd", "efgh", "ij"]);
        // "short" (5 chars) > width 4 → also hard-broken.
        assert_eq!(wrap_line("short", 4), vec!["shor", "t"]);
        // Empty input yields one empty piece (blank line round-trips).
        assert_eq!(wrap_line("", 4), vec![""]);
    }

    #[test]
    fn wrap_line_handles_cjk_by_char_count() {
        // 6 CJK chars at width 4 → two lines of 4 and 2 (not byte-based wrap,
        // which would wrap every ~1.3 chars and corrupt output).
        let pieces = wrap_line("你好世界测试", 4);
        assert_eq!(pieces, vec!["你好世界", "测试"]);
    }

    /// A raw column count resolves to itself up to the hard cap, `0` falls
    /// back to the non-TTY default, and anything over the cap is clamped — the
    /// single contract shared by the panel engine and the progress surface.
    #[test]
    fn resolve_cols_falls_back_and_caps() {
        assert_eq!(resolve_cols(0), FALLBACK_COLS);
        assert_eq!(resolve_cols(60), 60);
        assert_eq!(resolve_cols(HARD_CAP), HARD_CAP);
        assert_eq!(resolve_cols(400), HARD_CAP);
    }

    /// [`terminal_width`] is [`resolve_cols`] applied to the real terminal:
    /// always `<= HARD_CAP` (the cap is load-bearing) and never `0` on a TTY
    /// (`0` is mapped to [`FALLBACK_COLS`]). [`resolve_cols`] does *not* floor
    /// non-zero readings at [`FALLBACK_COLS`] — a real narrow terminal can
    /// report below it — so only the hard cap is asserted here. We can't assert
    /// an exact value because it reads the live terminal.
    #[test]
    fn terminal_width_stays_within_resolved_bounds() {
        let w = terminal_width();
        assert!(
            w <= HARD_CAP,
            "terminal_width {w} exceeds hard cap {HARD_CAP}"
        );
    }
}
