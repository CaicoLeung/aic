//! One-shot command output — the shared inset for every line `aic` prints
//! outside a Run/resolve frame (ADR 0018 §8: the one-shot commands adopt the
//! Run's symbols, colors, and spacing — not its timeline).
//!
//! Four geometry-only printers: [`out`]/[`err`] seat a primary line at the
//! same column as the timeline's Step markers ([`MARGIN`]); the `_detail`
//! variants seat explanatory rows one level deeper, on the sub-row column —
//! the same two-level grid the Run timeline uses. Styling goes through the
//! palette's standalone probes ([`styled_stdout`]/[`styled_stderr`]) so
//! pipes and `NO_COLOR` degrade to plain text. Markers (`✓`, `·`, `✗`) are
//! composed by the caller, mirroring how the timeline composes its own
//! markers over `MARGIN`; a row that mixes roles within one line (the `list`
//! table's muted keys beside default-fg values) passes
//! [`palette::plain`](crate::render::palette::plain) and embeds the styled
//! spans itself — a plain style applies nothing, so the embedded bytes
//! survive verbatim.
//!
//! Blank separator lines stay bare at the call site (a `println!()` of
//! nothing): an empty line carries no content and must never gain trailing
//! spaces. No-newline prompts (the y/n marker, "Press Enter…") compose
//! [`MARGIN`]/[`DETAIL_INDENT`] directly — the inset is the invariant, these
//! functions are the convenience.

use console::Style;

use crate::render::layout::MARGIN;
use crate::render::palette::{styled_stderr, styled_stdout};

/// One level deeper than [`MARGIN`] — the column the Run timeline's sub-rows
/// occupy (Step markers at column 2, sub-rows at column 4). Primary rows
/// hang on the marker column; detail rows (paths, hints, scope disclosures)
/// hang on the sub-row grid.
pub(crate) const DETAIL_INDENT: &str = "  ";

/// Print one primary line to stdout at the shared inset.
pub fn out(text: &str, style: Style) {
    println!("{}{}", MARGIN, styled_stdout(text, style));
}

/// Print one detail line to stdout — one level deeper than [`out`], on the
/// timeline's sub-row column.
pub fn out_detail(text: &str, style: Style) {
    println!("{}{}{}", MARGIN, DETAIL_INDENT, styled_stdout(text, style));
}

/// The stderr sibling of [`out`] — result and notice lines that belong to
/// the terminal chrome rather than stdout's piped results.
pub fn err(text: &str, style: Style) {
    eprintln!("{}{}", MARGIN, styled_stderr(text, style));
}

/// The stderr sibling of [`out_detail`].
pub fn err_detail(text: &str, style: Style) {
    eprintln!("{}{}{}", MARGIN, DETAIL_INDENT, styled_stderr(text, style));
}
