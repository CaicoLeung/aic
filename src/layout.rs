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

/// Terminal row count assumed when the real height is unknown (`rows == 0`,
/// i.e. piped / non-TTY output). Mirrors [`FALLBACK_COLS`] for the vertical
/// budget the reasoning window sizes against.
const FALLBACK_ROWS: usize = 24;

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

/// Resolve a raw terminal row count into a usable height — the vertical
/// counterpart of [`resolve_cols`]. `rows == 0` (non-TTY / piped, where
/// `Term::stderr()` reports no size) falls back to [`FALLBACK_ROWS`]; unlike
/// columns there is no hard cap — an oversized reading is harmless (the
/// reasoning window caps itself at its own hard ceiling).
/// Pure, so the fallback is unit-testable independently of the live terminal.
fn resolve_rows(rows: usize) -> usize {
    if rows == 0 { FALLBACK_ROWS } else { rows }
}

/// Greedy word-wrap *geometry*: pack `words` (each a `Vec` of display-width-1
/// units) into rows of ≤`width`, hard-breaking a word longer than `width` at
/// the boundary. Returns each row as the sequence of word-slices that compose
/// it — callers collect (plain text) or style-emit (ANSI) without re-deriving
/// the breaks. `width` must be ≥ 1; callers short-circuit `0`.
///
/// This is the single wrap-geometry source: [`wrap_line`] (plain `char`
/// words, for the panel engine) and the Markdown renderer's tagged wrap
/// (`markdown::wrap_runs`, `(char, tag)` words) share these exact breaks, so a
/// fix to the greedy/hard-break policy reaches both paths. It never sees a
/// style byte — width is the element count — which is what keeps styling
/// applied *after* wrap ANSI-blind (ADR 0013).
pub(crate) fn wrap_words<T>(words: &[Vec<T>], width: usize) -> Vec<Vec<&[T]>> {
    debug_assert!(width > 0, "callers short-circuit width == 0");
    let mut rows: Vec<Vec<&[T]>> = Vec::new();
    let mut cur: Vec<&[T]> = Vec::new();
    let mut cur_len = 0usize;
    for w in words {
        let wlen = w.len();
        // If the running line can't accept " <word>", flush it first.
        if !cur.is_empty() && cur_len + 1 + wlen > width {
            rows.push(std::mem::take(&mut cur));
            cur_len = 0;
        }
        if cur.is_empty() {
            // Word starts a new line — hard-break it if it alone exceeds width.
            let mut idx = 0;
            while wlen - idx > width {
                rows.push(vec![&w[idx..idx + width]]);
                idx += width;
            }
            if idx < wlen {
                cur.push(&w[idx..]);
                cur_len = wlen - idx;
            }
        } else {
            cur.push(&w[..]);
            cur_len += 1 + wlen;
        }
    }
    rows.push(cur);
    rows
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
/// A thin collector over [`wrap_words`]: the break geometry lives once there,
/// shared with the Markdown renderer, and this just rejoins each row's
/// word-slices with single spaces into plain `String`s.
///
/// Returns at least one piece; an empty input yields `vec![""]` so blank
/// source lines round-trip as a single empty piece.
pub(crate) fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![line.to_string()];
    }
    let words: Vec<Vec<char>> = line
        .split_whitespace()
        .map(|w| w.chars().collect())
        .collect();
    wrap_words(&words, width)
        .into_iter()
        .map(|row| {
            let mut s = String::new();
            for (i, slice) in row.iter().enumerate() {
                if i > 0 {
                    s.push(' ');
                }
                s.extend(slice.iter());
            }
            s
        })
        .collect()
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

/// Read the real terminal's row count via [`resolve_rows`] (`0` →
/// [`FALLBACK_ROWS`] on a non-TTY/pipe — parity with width's [`resolve_cols`]).
/// No cap: unlike columns, an oversized reading is harmless (the reasoning
/// window caps itself at its own hard ceiling, and the
/// renderer only uses the height to find the bottom margin). The single
/// geometry entry point for in-place rendering's vertical budget, shared by
/// the reasoning window sizing and the renderer's bottom-margin detection.
pub(crate) fn terminal_height() -> usize {
    resolve_rows(Term::stderr().size().0 as usize)
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

    /// [`resolve_rows`] is [`terminal_height`]'s resolution core: `0` falls
    /// back to the non-TTY default, a real reading passes through uncapped
    /// (the reasoning window applies its own cap). Mirrors the column-
    /// resolution test — the fallback is the function's only logic and is now
    /// asserted directly rather than inferred from a live-terminal bounds check.
    #[test]
    fn resolve_rows_falls_back_uncapped() {
        assert_eq!(resolve_rows(0), FALLBACK_ROWS);
        assert_eq!(resolve_rows(24), 24);
        assert_eq!(resolve_rows(200), 200); // no hard cap — oversized is harmless
    }
}
