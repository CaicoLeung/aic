//! The run's live progress surface: an in-place spinner for one-shot waits
//! ([`with_spinner`]) and a flicker-free streaming reasoning feed
//! ([`ReasoningRenderer`] + [`ThinkingView`]).
//!
//! Split out of `display.rs` (AIC-19): `display` is the *static* panel engine
//! — commit lines, conflict summaries, the things that print once and stay.
//! This module is the *moving* surface — glyphs that spin, rows that redraw in
//! place and then vanish so nothing lingers in the scrollback. The two share
//! only the geometry primitives in [`crate::layout`] (the [`crate::layout::MARGIN`]
//! inset, [`crate::layout::wrap_line`], [`crate::layout::terminal_width`]); everything
//! that animates lives here.

use console::Term;
use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use crate::layout::{MARGIN, terminal_width, wrap_line};

/// Minimum usable width for in-place progress rendering: the spinner glyph +
/// its label need at least this much room, so a pathologically narrow terminal
/// (or a misreported size) doesn't crush the spinner. The progress surface's
/// own policy on top of the shared [`terminal_width`]: the panel engine instead
/// subtracts its margins (see [`crate::display`]).
const MIN_PROGRESS_WIDTH: usize = 20;

/// Animation frame rate for every spinner in the run: the braille tick
/// advances once per tick, so a shorter interval is a smoother spin. 50 ms ≈
/// 20 fps, down from the previous 80 ms.
pub(crate) const SPINNER_TICK: Duration = Duration::from_millis(50);

/// Grace window before the silent-CLI notice appears in the loading frame.
/// Streaming CLIs (claude `-p` w/ `stream-json`, codex exec, pi) usually emit
/// their first reasoning line within 1–3 s of first token, but **first token
/// itself** is gated by the CLI's cold start — claude loads SessionStart
/// hooks, an `init` payload, MCP handshakes, then pays a network TTFT, often
/// 6–10 s in total before any `thinking_delta`. A backend still entirely
/// silent past this deadline is assumed to be a non-streaming CLI (or a
/// wedged one) and the loading frame gains an explanatory notice so the user
/// is not left staring at a bare spinner. Streaming-capable backends
/// ([`Encoding::ClaudeStreamJson`]) get a different, cold-start notice past
/// the same deadline — they ARE streaming, just not reasoning yet. Tunable:
/// the only effect of changing it is how long a slow-to-first-token backend
/// lingers in the generic loading state before the notice appears — the first
/// delta cancels loading regardless.
pub(crate) const LOADING_GRACE: Duration = Duration::from_secs(5);
/// How long the final reasoning frame lingers after the analysis completes,
/// before [`ReasoningRenderer::finish`] erases it — a guaranteed minimum dwell
/// so the last lines are readable. The live stream is NOT paced to reading
/// speed (that would block the commit — the tool's actual job — on
/// decoration), so this tail is the only forced pause: small, bounded, and
/// paid only when reasoning actually streamed.
pub(crate) const READ_TAIL: Duration = Duration::from_millis(1500);

/// Minimum rows the reasoning window keeps visible even on a short terminal,
/// so a small screen still shows a usable slice of the chain-of-thought.
const MIN_REASONING_ROWS: usize = 4;

/// Hard ceiling on the reasoning window. A very tall terminal could otherwise
/// paint dozens of rows, pushing the in-place region toward the bottom edge
/// where further growth would scroll into the scrollback (breaking the discard
/// model — reasoning must never linger). Well past any comfortable reading
/// window, so real terminals are essentially never clamped here in practice.
const MAX_REASONING_ROWS: usize = 40;

/// How many reasoning rows stay visible at once — a *rendered-row* cap, now
/// sized to the real terminal instead of a fixed 12.
///
/// The old fixed 12 was the root cause of "content scrolls past too fast to
/// read": at 12 rows a line is visible for only ~12 emission-intervals before
/// it rolls off the top of the window. Enlarging the window to the terminal
/// height makes each line linger proportionally longer — the window *is* the
/// "invisible area" content was scrolling out of, so growing it to fill the
/// screen is the direct fix. The block is still in-place and erased on
/// [`ReasoningRenderer::finish`], so the region must stay below the terminal
/// height or older rows scroll into the scrollback; the budget therefore
/// reserves the spinner row plus a safety margin and is clamped to
/// [`MIN_REASONING_ROWS`]..[`MAX_REASONING_ROWS`].
pub(crate) fn reasoning_window_rows() -> usize {
    // `Term::size()` -> (rows, cols); height is `.0`. 0 on a non-TTY/pipe ->
    // assume a conventional 24-row terminal (parity with width's fallback).
    let h = Term::stderr().size().0 as usize;
    let h = if h == 0 { 24 } else { h };
    h.saturating_sub(3) // spinner row + 2-row safety margin
        .clamp(MIN_REASONING_ROWS, MAX_REASONING_ROWS)
}

/// Shared indicatif spinner style: a braille tick and a prefix matching
/// [`MARGIN`] so the spinner glyph sits at the same 2-column inset as the rest
/// of the run's stderr block — not flush against the edge. One place to
/// change the inset or tick animation for every spinner in the run.
pub(crate) fn spinner_style() -> anyhow::Result<indicatif::ProgressStyle> {
    Ok(indicatif::ProgressStyle::default_spinner()
        .template(&format!("{MARGIN}{{spinner}} {{msg}}"))?
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"))
}

/// Run `fut` behind an in-place spinner labeled `msg`; the spinner is cleared
/// when the future completes, success or error.
pub(crate) async fn with_spinner<F, T>(msg: &str, fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let pb = indicatif::ProgressBar::new_spinner();
    pb.set_style(spinner_style()?);
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(SPINNER_TICK);

    let result = fut.await;
    pb.disable_steady_tick();
    pb.finish_and_clear();
    result
}

/// A rolling window over the model's streamed reasoning, sized to the caller's
/// `cap` logical lines (the retention window; the rendered-row cap lives in
/// [`reasoning_rows`]). The caller renders [`push`](Self::push)'s returned
/// window into the spinner's in-place multi-line message, so the block redraws
/// in place — never printed as permanent lines that linger in the scrollback —
/// and is erased once thinking ends ([`ReasoningRenderer::finish`]), leaving
/// the terminal clean for the rest of the run. The cap keeps the block bounded
/// while it streams, so no unbounded region ever accumulates.
///
/// `cap` is sized by the caller from [`reasoning_window_rows`], so a larger
/// terminal retains more reasoning instead of aging it out at a fixed rate.
/// Completed lines (terminated by `\n`) roll into the window; the in-progress
/// partial line (no trailing `\n` yet) is shown as the window's last line while
/// it builds and counts against the same budget. Blank lines are dropped to
/// keep the feed information-dense.
pub(crate) struct ThinkingView {
    lines: VecDeque<String>,
    cur: String,
    cap: usize,
}

impl ThinkingView {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(cap),
            cur: String::new(),
            cap,
        }
    }

    /// Ingest a reasoning delta (may be a partial line, many lines, or empty)
    /// and return the current window: the newest completed lines plus the
    /// in-progress partial line, oldest-first, capped to `cap` lines. A delta
    /// that ends mid-line leaves the partial buffered and shown as the window's
    /// last line until the next `\n`.
    pub(crate) fn push(&mut self, delta: &str) -> Vec<String> {
        for ch in delta.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.cur);
                if !line.trim().is_empty() {
                    self.push_line(line);
                }
            } else {
                self.cur.push(ch);
            }
        }
        self.window()
    }

    /// Append a completed line, then bound *storage* to `cap` — not the
    /// visible window. This stops a long chain-of-thought from retaining every
    /// completed line forever; the visible cap lives in [`window`](Self::window),
    /// which also accounts for the in-progress partial row.
    fn push_line(&mut self, line: String) {
        self.lines.push_back(line);
        if self.lines.len() > self.cap {
            self.lines.pop_front();
        }
    }

    /// The line-level window: the newest `cap` lines, completed lines
    /// oldest->newest then the in-progress partial as the last line. The
    /// partial counts against the same budget, so a full window trims its
    /// oldest completed line here to make room. This bounds retention
    /// (memory); the *visible* row cap is applied by [`reasoning_rows`] after
    /// wrapping, since one line can render to several rows.
    fn window(&self) -> Vec<String> {
        let mut rows: Vec<String> = self.lines.iter().cloned().collect();
        if !self.cur.trim().is_empty() {
            rows.push(self.cur.clone());
        }
        let start = rows.len().saturating_sub(self.cap);
        rows[start..].to_vec()
    }
}

/// Braille frames for the analysis spinner. Advanced once per reasoning
/// delta (see [`ReasoningRenderer`]) so the glyph spins while reasoning flows
/// and freezes when it stalls — incidental animation that needs no background
/// ticker, since a steady tick is exactly what forced indicatif's flickering
/// full-block repaints.
const REASONING_GLYPHS: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// Build the visual rows for one reasoning frame: the spinner+label row on
/// top, then each retained reasoning line greedy-wrapped under the shared
/// `│ ` indent. Pure (no I/O) so the layout is unit-testable; the renderer
/// paints exactly what this returns.
///
/// `feed_width` is the per-piece wrap budget. An empty `window` yields just
/// the spinner row. The reasoning rows (everything below the spinner row) are
/// capped to [`REASONING_WINDOW`]: a long line wraps to several rows, so the
/// newest [`REASONING_WINDOW`] *rendered rows* are kept and the oldest drop
/// out — the top row may start mid-line, exactly like a terminal tail window.
fn reasoning_rows(
    glyph: &str,
    label: &str,
    window: &[String],
    feed_width: usize,
    elapsed: Option<Duration>,
) -> Vec<String> {
    let mut rows = Vec::with_capacity(window.len() + 1);
    // Spinner row: when `elapsed` is `Some` (the live feed is ticking), append
    // a rising seconds count so the wait is visible even while the model is
    // silent between deltas — the same feedback the loading frame gives, kept
    // through the transition into reasoning so the user never loses the
    // clock. `None` is the tests' stable snapshot (no clock).
    let spinner = match elapsed {
        Some(e) => format!("{MARGIN}{glyph} {label}… {}s", e.as_secs()),
        None => format!("{MARGIN}{glyph} {label}"),
    };
    rows.push(spinner);
    for line in window {
        for piece in wrap_line(line, feed_width) {
            rows.push(format!("{MARGIN}│ {piece}"));
        }
    }
    // The load-bearing rendered-row cap: the line window alone can't bound
    // the on-screen region, because one long line wraps to many rows. Keep
    // the newest REASONING_WINDOW rows below the spinner row.
    let start = 1 + (rows.len() - 1).saturating_sub(REASONING_WINDOW);
    rows.drain(1..start);
    rows
}

/// Which explanatory notice (if any) to show under the spinner row of a
/// loading frame. The choice is the caller's: a streaming-capable backend
/// that has not yet produced reasoning is in a cold start (hooks/MCP/TTFT),
/// not a capability gap, so it must not be labeled non-streaming; a plain
/// backend silent past grace is the case the "does not support streaming"
/// notice was written for.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum LoadingNotice {
    /// Pre-grace, or a backend that has not been classified: just spinner +
    /// elapsed, no notice row.
    None,
    /// A streaming-capable backend (claude `stream-json`, pi `--mode json`)
    /// past grace with no reasoning yet — a cold start, not a capability gap.
    /// Carries the CLI's own program name so the notice labels the actual
    /// backend (`pi`, `claude`, …), never a hardcoded one. Worded to explain
    /// the wait (hooks/MCP/first-token) without claiming the CLI cannot stream.
    ColdStart(String),
    /// A plain backend past grace with no output: assumed non-streaming.
    Silent,
}

/// Build the explanatory notice for the [`LoadingNotice::ColdStart`] case: a
/// streaming-capable CLI that has not yet emitted reasoning. Interpolates the
/// CLI's own `program` name so the wait is attributed to the right backend
/// (`pi`, `claude`, …), not a hardcoded "Claude" — a pi/opencode/custom run
/// must not be mislabeled. Never claims the CLI cannot stream — it can, it is
/// just paying its cold-start cost (SessionStart hooks, MCP handshakes,
/// first-token latency).
fn cold_start_notice(program: &str) -> String {
    format!(
        "{program} is starting up — first reasoning line takes several seconds \
         while hooks and MCP servers initialize"
    )
}

/// The explanatory notice shown under the spinner row once a plain backend
/// has been silent past [`LOADING_GRACE`]. Worded for the CLI-agent case (the
/// common silent-backend source) but accurate for any backend that returns a
/// full answer with no reasoning deltas — it never claims the *model* cannot
/// think, only that aic is not receiving a stream to show.
const SILENT_NOTICE: &str =
    "This CLI agent does not stream its thinking process — aic is waiting for the final answer";

/// Build the visual rows for one loading frame: the spinner+label row on top
/// annotated with `elapsed` seconds, then — depending on `notice` — an
/// explanatory row greedy-wrapped under the shared `│ ` indent. Pure (no
/// I/O) so the layout is unit-testable; [`ReasoningRenderer::paint_loading`]
/// paints exactly what this returns. Mirrors [`reasoning_rows`]'s shape so a
/// loading frame and a reasoning frame hand off cleanly through [`draw_rows`]
/// (same spinner row prefix, same indent for body rows).
fn loading_rows(
    glyph: &str,
    label: &str,
    elapsed: Duration,
    notice: LoadingNotice,
    feed_width: usize,
) -> Vec<String> {
    let secs = elapsed.as_secs();
    let mut rows = Vec::with_capacity(2);
    rows.push(format!("{MARGIN}{glyph} {label}… {secs}s"));
    let text: String = match &notice {
        LoadingNotice::None => return rows,
        LoadingNotice::ColdStart(program) => cold_start_notice(program),
        LoadingNotice::Silent => SILENT_NOTICE.to_string(),
    };
    for piece in wrap_line(&text, feed_width) {
        rows.push(format!("{MARGIN}│ {piece}"));
    }
    rows
}

/// Flicker-free in-place renderer for the streaming reasoning window.
///
/// Replaces the indicatif multi-line spinner, whose redraw is a two-phase
/// "blank every row, then repaint every row" — the blank gap scaled with the
/// window height and the spinner's steady tick forced it ~20×/s, so a 12-line
/// (often 20–36 visual rows) block flickered visibly. This renderer paints
/// with **interleaved clear-then-write**: move to the top of the previous
/// frame, then clear a line and rewrite it immediately before descending to
/// the next. At any instant at most one row is blank, and only until it is
/// rewritten — imperceptible regardless of window height or update rate.
///
/// There is no steady tick: a frame is painted only when reasoning changes,
/// and the glyph advances on each paint so it spins with the stream and
/// freezes when it stalls. [`finish`](Self::finish) erases the frame once
/// thinking ends — the reasoning never lingers on screen or in the scrollback
/// (it was in-place all along) — and restores the cursor to the line where the
/// frame began, so the next line of stderr continues with no gap. All writes
/// are best-effort: a closed stderr just stops drawing, never breaks the
/// commit flow. A no-op off a terminal.
pub(crate) struct ReasoningRenderer {
    term: Term,
    label: &'static str,
    feed_width: usize,
    /// Rendered-row cap for the reasoning window, resolved once by the caller
    /// from the real terminal height ([`reasoning_window_rows`]) so the view
    /// and the renderer share one notion of the window size; a resize
    /// mid-stream only changes wrap widths, never the row budget.
    max_rows: usize,
    glyph: usize,
    prev_height: usize,
    /// `true` once a frame has been painted and not yet finished — guards
    /// [`Drop`] so a mid-stream error still erases the frame.
    active: bool,
    /// `true` while the terminal cursor is hidden for the active stream. Set
    /// when the first frame emits [`HIDE`] and cleared when [`ReasoningRenderer::finish`] emits
    /// [`SHOW`]. Owning the visibility state explicitly — rather than inferring
    /// it from `prev_height == 0` — is what guarantees every [`HIDE`] is paired
    /// with exactly one [`SHOW`], even if a frame were ever painted with no rows.
    /// That zero-row case is currently unreachable — [`reasoning_rows`] always
    /// yields at least the spinner row, so `first_frame` is true exactly once
    /// per stream — but the flag is retained as a contract guard against a
    /// future caller that paints before any row exists.
    cursor_hidden: bool,
}

/// ANSI escapes for the in-place repaint. Hand-written (rather than via
/// `console::Term`'s cursor/clear methods) so the exact byte sequence is
/// assembled by the pure [`frame_bytes`] helper and its anti-flicker
/// interleaving is unit-testable.
const CLR_LINE: &str = "\x1b[2K"; // erase the current line
const UP: &str = "\x1b[1A"; // cursor up one row
const DOWN: &str = "\x1b[1B"; // cursor down one row

/// Hide the terminal cursor for the whole reasoning stream. While the renderer
/// repaints the frame top-to-bottom the hardware caret would otherwise sit, row
/// by row, at the tail of each freshly written line — mid-row over the
/// *previous* frame's longer text on the rows below — which reads as the caret
/// "jumping between characters" during fast streaming. Emitted once on the
/// first frame and held until [`SHOW`] at [`ReasoningRenderer::finish`], so the
/// caret is simply absent while reasoning flows and reappears, parked, once
/// thinking ends — no per-frame flicker, no smearing across the region.
const HIDE: &str = "\x1b[?25l"; // DECRST — hide cursor
const SHOW: &str = "\x1b[?25h"; // DECSET — show cursor

/// Assemble the byte sequence to repaint `rows` over a previous frame
/// `prev_height` rows tall. The repaint is **interleaved clear-then-write**:
/// move to the top, then for each row `\r` + clear + write, descending between
/// rows; a shorter new frame blanks its stale tail and returns the cursor to
/// the last live row. No two rows are ever blanked without the first being
/// rewritten first — the property that kills the flicker indicatif's
/// "blank-all-then-rewrite-all" repaint suffers from.
///
/// Cursor visibility is a stream-spanning concern, not a per-frame one. The
/// first repaint emits [`HIDE`] (when `prev_height == 0`) and the caret stays
/// hidden for the whole stream; only [`clear_frame_bytes`] at
/// [`ReasoningRenderer::finish`] restores it ([`SHOW`]). Mid-stream repaints
/// emit neither — the caret stays hidden, so where it sits during the traversal
/// is irrelevant and can never smear across the repainted rows.
fn frame_bytes(rows: &[String], prev_height: usize) -> String {
    let height = rows.len();
    let mut out = String::new();
    if prev_height == 0 {
        // First frame of the stream: hide the cursor for the whole stream
        // (restored by clear_frame_bytes at finish). The frame paints in place
        // starting at the cursor's current line — no leading `\n`. Every
        // caller arrives with the cursor at column 0 of a fresh line (stderr
        // output always ends in a newline), so an opening `\n` would only
        // reserve a blank line above the block that `finish` could never
        // reclaim, leaving a permanent gap. Clearing the line first (`\r` +
        // [`CLR_LINE`]) means a mid-line cursor is overwritten cleanly rather
        // than smeared.
        out.push_str(HIDE);
    } else {
        for _ in 0..prev_height.saturating_sub(1) {
            out.push_str(UP);
        }
    }
    for (i, row) in rows.iter().enumerate() {
        out.push('\r');
        out.push_str(CLR_LINE);
        out.push_str(row);
        if i + 1 < height {
            out.push_str(DOWN);
        }
    }
    if height < prev_height {
        for _ in height..prev_height {
            out.push_str(DOWN);
            out.push('\r');
            out.push_str(CLR_LINE);
        }
        for _ in height..prev_height {
            out.push_str(UP);
        }
    }
    out
}

/// Assemble the byte sequence to erase a `prev_height`-row frame, clearing
/// each row from the BOTTOM up so the traversal naturally ends on the frame's
/// TOP row — exactly where the first frame began painting (the cursor's line
/// at stream start). So once thinking ends the cursor sits back on its
/// original line, the block is gone, and the next line of stderr overwrites
/// the blank region from the top with no gap trapped above or below.
///
/// Because the frame never opened with a `\n` (see [`frame_bytes`]), there is
/// no reserved blank line to reclaim and no compensation walk: the bottom-up
/// clear is a single coherent traversal that restores the cursor to its start
/// by construction. (Clearing bottom-up is fine here because this is the final
/// erase, never a mid-stream repaint — the anti-flicker interleaved
/// clear-then-write of [`frame_bytes`] doesn't apply to a one-shot erase.)
///
/// This is the stream's end, so it appends [`SHOW`] to restore the cursor when
/// `cursor_hidden` — i.e. when the first frame's [`HIDE`] was emitted. Keying
/// the restore on the renderer's explicit visibility flag (rather than on
/// `prev_height`) is what makes every [`HIDE`] pair with exactly one [`SHOW`]:
/// a zero-row frame that still hid the cursor restores it; a frame that was
/// never painted (`cursor_hidden == false`) owes nothing. No leading [`HIDE`]:
/// the caret is already hidden, so the clear traversal can't smear it.
fn clear_frame_bytes(prev_height: usize, cursor_hidden: bool) -> String {
    let mut out = String::new();
    // Clear the current (bottom) row, ascend, repeat — ending on the top row.
    for i in 0..prev_height {
        out.push('\r');
        out.push_str(CLR_LINE);
        if i + 1 < prev_height {
            out.push_str(UP);
        }
    }
    if cursor_hidden {
        out.push_str(SHOW);
    }
    out
}

impl ReasoningRenderer {
    /// Bind a renderer to stderr with `label` on the spinner row. `max_rows`
    /// is the reasoning window's rendered-row cap (from
    /// [`reasoning_window_rows`]); `feed_width` is resolved once from the
    /// shared terminal geometry ([`terminal_width`]), floored at the progress
    /// surface's [`MIN_PROGRESS_WIDTH`] so the spinner and its label keep room.
    /// A resize mid-stream only changes wrap widths, never correctness.
    pub(crate) fn new(label: &'static str, max_rows: usize) -> Self {
        Self {
            term: Term::stderr(),
            label,
            feed_width: terminal_width().max(MIN_PROGRESS_WIDTH).saturating_sub(6),
            max_rows,
            glyph: 0,
            prev_height: 0,
            active: false,
            cursor_hidden: false,
        }
    }

    /// Paint one frame for the reasoning `window`. Safe to call on every
    /// delta AND on every idle tick — the latter via `Some(elapsed)` keeps the
    /// spinner animating and the elapsed count rising while the model is
    /// silent between deltas, so the reasoning frame never freezes the way a
    /// paint-only-on-delta renderer would during a long TTFT gap. A no-op off
    /// a terminal.
    pub(crate) fn paint(&mut self, window: &[String], elapsed: Option<Duration>) {
        if !self.term.is_term() {
            return;
        }
        let glyph = REASONING_GLYPHS[self.glyph % REASONING_GLYPHS.len()];
        self.glyph = self.glyph.wrapping_add(1);
        let rows = reasoning_rows(glyph, self.label, window, self.feed_width, elapsed, self.max_rows);
        self.draw_rows(&rows);
    }

    /// Paint one loading frame for the silent/cold-start backend state: the
    /// spinner row annotated with `elapsed` seconds, plus — once `notice` is
    /// past [`LOADING_GRACE`]/classified — an explanatory row. Used by
    /// [`analyze_changes`](crate::analyze_changes) between stream start and
    /// the first reasoning delta so a non-streaming or cold-starting backend
    /// is not a silent dead zone: the user always sees motion (the spinning
    /// glyph) and a rising elapsed count. The first delta swaps this frame for
    /// the normal reasoning window via [`paint`](Self::paint); both go through
    /// [`draw_rows`], so the in-place repaint handles the height transition
    /// with no special-casing. A no-op off a terminal.
    pub(crate) fn paint_loading(&mut self, elapsed: Duration, notice: LoadingNotice) {
        if !self.term.is_term() {
            return;
        }
        let glyph = REASONING_GLYPHS[self.glyph % REASONING_GLYPHS.len()];
        self.glyph = self.glyph.wrapping_add(1);
        let rows = loading_rows(glyph, self.label, elapsed, notice, self.feed_width);
        self.draw_rows(&rows);
    }

    /// Repaint `rows` in place via [`frame_bytes`]: one write of the assembled
    /// escape sequence, then a flush. Pure byte assembly lives in
    /// [`frame_bytes`] so the anti-flicker interleaving is unit-testable.
    fn draw_rows(&mut self, rows: &[String]) {
        let first_frame = self.prev_height == 0;
        // Record the cursor-hidden state BEFORE the write: the flag mirrors
        // exactly when frame_bytes emits HIDE (prev_height == 0), so finishing
        // the in-memory tracking ahead of the side effect means a hidden cursor
        // can never be stranded — even if a future insertion between here and
        // the write panicked, Drop's finish would still see cursor_hidden and
        // emit the owed SHOW.
        self.cursor_hidden |= first_frame;
        let bytes = frame_bytes(rows, self.prev_height);
        let _ = self.term.write_str(&bytes);
        let _ = self.term.flush();
        self.prev_height = rows.len();
        self.active = true;
    }

    /// End the reasoning stream: erase the whole frame — spinner row and
    /// reasoning rows — so the block vanishes once thinking is done, and
    /// restore the cursor to the line where the frame began (the cursor's
    /// position at stream start), so the rest of the run's stderr continues
    /// with no blank gap above or below. Idempotent; also the [`Drop`]
    /// backstop for an aborted stream.
    pub(crate) fn finish(&mut self) {
        if !self.active {
            return;
        }
        let bytes = clear_frame_bytes(self.prev_height, self.cursor_hidden);
        let _ = self.term.write_str(&bytes);
        let _ = self.term.flush();
        self.prev_height = 0;
        self.active = false;
        self.cursor_hidden = false;
    }
}

impl Drop for ReasoningRenderer {
    fn drop(&mut self) {
        // Backstop: if the stream aborted before `finish`, still erase the
        // frame so the rest of the run's stderr is clean.
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`ThinkingView::push`] returns the current window: completed lines
    /// (blank ones dropped) in arrival order, oldest-first.
    #[test]
    fn thinking_view_window_shows_completed_lines_and_drops_blanks() {
        let mut v = ThinkingView::new(12);
        let window = v.push("line 1\n\nline 2\n");
        assert_eq!(window, vec!["line 1", "line 2"]);
    }

    /// A partial line with no trailing `\n` is the window's last row while it
    /// builds, then collapses into a completed row when the `\n` arrives.
    #[test]
    fn thinking_view_partial_is_last_window_row_until_newline() {
        let mut v = ThinkingView::new(12);
        assert_eq!(v.push("in progress"), vec!["in progress"]);
        let window = v.push(" done\n");
        assert_eq!(window, vec!["in progress done"]);
    }

    /// One logical line split across several deltas assembles into a single
    /// window row, shown live as it grows.
    #[test]
    fn thinking_view_assembles_split_chunks() {
        let mut v = ThinkingView::new(12);
        assert_eq!(v.push("hel"), vec!["hel"]);
        assert_eq!(v.push("lo"), vec!["hello"]);
        assert_eq!(v.push(" world\n"), vec!["hello world"]);
    }

    /// A delta containing several `\n`-separated lines yields a window with
    /// each one, in order.
    #[test]
    fn thinking_view_many_lines_one_delta() {
        let mut v = ThinkingView::new(12);
        assert_eq!(v.push("a\nb\nc\n"), vec!["a", "b", "c"]);
    }

    /// The window rolls: past the cap, the oldest completed
    /// line is dropped, so the window stays capped and always shows the newest
    /// rows.
    #[test]
    fn thinking_view_rolls_at_capacity() {
        let mut v = ThinkingView::new(12);
        for i in 1..=15 {
            v.push(&format!("line {i}\n"));
        }
        let window = v.push("");
        assert_eq!(window.len(), 12);
        assert_eq!(window.first(), Some(&"line 4".to_string()));
        assert_eq!(window.last(), Some(&"line 15".to_string()));
    }

    /// The in-progress partial line counts against the same budget: with the
    /// window full of completed rows, a partial drops the oldest completed row
    /// so the window never exceeds the cap.
    #[test]
    fn thinking_view_partial_counts_against_budget() {
        let mut v = ThinkingView::new(12);
        for i in 1..=12 {
            v.push(&format!("line {i}\n"));
        }
        let window = v.push("in progress");
        assert_eq!(window.len(), 12);
        // oldest completed row rolled out to make room for the partial
        assert_eq!(window.first(), Some(&"line 2".to_string()));
        assert_eq!(window.last(), Some(&"in progress".to_string()));
    }

    /// [`reasoning_rows`] with `Some(elapsed)` appends a rising seconds
    /// count to the spinner row, so the wait stays visible while the model
    /// is silent between deltas (the steady-tick repaint path). The window
    /// rows are unchanged.
    #[test]
    fn reasoning_rows_spinner_shows_elapsed_when_ticking() {
        let rows = reasoning_rows(
            "⠙",
            "Analyzing",
            &["a line".to_string()],
            80,
            Some(Duration::from_secs(7)),
            12,
        );
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠙ Analyzing… 7s")));
        assert_eq!(rows[1], format!("{MARGIN}│ a line"));
    }

    /// [`reasoning_rows`] always leads with the spinner+label row, even when
    /// the window is empty (the stream just started).
    #[test]
    fn reasoning_rows_leads_with_spinner_for_empty_window() {
        let rows = reasoning_rows("⠋", "Analyzing", &[], 80, None, 12);
        assert_eq!(rows, vec![format!("{MARGIN}⠋ Analyzing")]);
    }

    /// Each retained line becomes one indented row when it fits the budget.
    #[test]
    fn reasoning_rows_indents_each_line() {
        let window = vec!["line 1".to_string(), "line 2".to_string()];
        let rows = reasoning_rows("⠙", "Analyzing", &window, 80, None, 12);
        assert_eq!(
            rows,
            vec![
                format!("{MARGIN}⠙ Analyzing"),
                format!("{MARGIN}│ line 1"),
                format!("{MARGIN}│ line 2"),
            ]
        );
    }

    /// A line longer than the budget wraps to several rows under the same
    /// `│ ` indent — the visual height can exceed the logical-line count.
    #[test]
    fn reasoning_rows_wraps_long_lines_to_multiple_rows() {
        let prefix = format!("{MARGIN}│ ");
        let rows = reasoning_rows(
            "⠹",
            "Analyzing",
            &["the quick brown fox".to_string()],
            10,
            None,
            12,
        );
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
        // every row after the spinner carries the `│ ` indent…
        for row in &rows[1..] {
            assert!(row.starts_with(&prefix), "unindented row: {row:?}");
        }
        // …and the words survive the greedy wrap, rejoinable losslessly.
        let body: String = rows[1..]
            .iter()
            .filter_map(|r| r.strip_prefix(&prefix))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(body, "the quick brown fox");
    }

    /// The load-bearing rendered-row cap: wrap pieces count against
    /// the row budget, so a window whose lines wrap to more rows than
    /// the budget shows only the newest rows — the oldest wrap pieces (and
    /// whole lines) roll out, the spinner row is never dropped.
    #[test]
    fn reasoning_rows_caps_rendered_rows_to_window_budget() {
        let prefix = format!("{MARGIN}│ ");
        // 12 lines × 2 wrap pieces = 24 rendered rows, over the 12-row budget.
        let window: Vec<String> = (1..=12).map(|i| format!("line {i} with words")).collect();
        let rows = reasoning_rows("⠹", "Analyzing", &window, 10, None, 12);
        assert_eq!(rows.len(), 13);
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
        for row in &rows[1..] {
            assert!(row.starts_with(&prefix), "unindented row: {row:?}");
        }
        // The newest rows survived, losslessly rejoinable: lines 7–12 only.
        let body: String = rows[1..]
            .iter()
            .filter_map(|r| r.strip_prefix(&prefix))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(
            body,
            (7..=12)
                .map(|i| format!("line {i} with words"))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    /// A single pathological line that wraps far past the budget still cannot
    /// grow the region: only the newest max_rows rendered rows are
    /// kept, the top row a mid-line cut like a terminal tail window.
    #[test]
    fn reasoning_rows_caps_single_long_line() {
        let prefix = format!("{MARGIN}│ ");
        let long = "word ".repeat(60); // 300 chars → 30 wrap pieces at width 10
        let rows = reasoning_rows("⠹", "Analyzing", &[long], 10, None, 12);
        assert_eq!(rows.len(), 13);
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
        // 12 rows of "word word" = 24 words of the 60 — the newest tail only.
        let words: usize = rows[1..]
            .iter()
            .map(|r| r.strip_prefix(&prefix).unwrap().split_whitespace().count())
            .sum();
        assert_eq!(words, 24);
    }

    /// [`loading_rows`] before the grace deadline: just the spinner row with
    /// the elapsed count, no notice — a slow-to-first-token backend is never
    /// falsely labeled.
    #[test]
    fn loading_rows_pre_grace_is_spinner_plus_elapsed_only() {
        let rows = loading_rows(
            "⠋",
            "Analyzing changes",
            Duration::from_secs(3),
            LoadingNotice::None,
            120,
        );
        assert_eq!(rows, vec![format!("{MARGIN}⠋ Analyzing changes… 3s")]);
    }

    /// [`loading_rows`] past the grace deadline for a plain backend adds the
    /// [`SILENT_NOTICE`] as an indented body row, mirroring a reasoning
    /// frame's shape so the in-place repaint hands off cleanly later.
    #[test]
    fn loading_rows_post_grace_silent_adds_notice() {
        let rows = loading_rows(
            "⠙",
            "Analyzing changes",
            Duration::from_secs(8),
            LoadingNotice::Silent,
            120,
        );
        assert_eq!(
            rows,
            vec![
                format!("{MARGIN}⠙ Analyzing changes… 8s"),
                format!("{MARGIN}│ {SILENT_NOTICE}"),
            ]
        );
    }

    /// [`loading_rows`] past the grace deadline for a streaming-capable
    /// backend adds the cold-start notice built from the carried program name
    /// — a cold start is not a capability gap, so it must not be labeled
    /// non-streaming. The program name interpolates, so `pi`/`claude`/custom
    /// backends are each labeled by their own command, never hardcoded
    /// "Claude".
    #[test]
    fn loading_rows_post_grace_cold_start_adds_notice() {
        let rows = loading_rows(
            "⠹",
            "Analyzing changes",
            Duration::from_secs(7),
            LoadingNotice::ColdStart("claude".to_string()),
            160,
        );
        let expected = cold_start_notice("claude");
        assert_eq!(
            rows,
            vec![
                format!("{MARGIN}⠹ Analyzing changes… 7s"),
                format!("{MARGIN}│ {expected}"),
            ]
        );
    }

    /// The notice wraps under the same `│ ` indent as reasoning rows once it
    /// exceeds the budget, so a narrow terminal cannot overflow the frame.
    #[test]
    fn loading_rows_wraps_notice_to_budget() {
        let rows = loading_rows(
            "⠹",
            "Analyzing changes",
            Duration::from_secs(9),
            LoadingNotice::Silent,
            12,
        );
        assert_eq!(
            rows.first(),
            Some(&format!("{MARGIN}⠹ Analyzing changes… 9s"))
        );
        // every body row is indented and the wrapped pieces rejoin losslessly.
        let prefix = format!("{MARGIN}│ ");
        let body: String = rows[1..]
            .iter()
            .filter_map(|r| r.strip_prefix(&prefix))
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(body, SILENT_NOTICE);
    }

    /// First paint (no previous frame) hides the cursor and clears+writes its
    /// single row in place at the cursor's current line — no leading newline
    /// (which would reserve an unreclaimable blank line above the block) and
    /// no cursor-up preamble.
    #[test]
    fn frame_bytes_first_frame_paints_in_place() {
        let out = frame_bytes(&["only".to_string()], 0);
        assert_eq!(out, format!("{HIDE}\r{CLR_LINE}only"));
        // the first frame never emits a newline: blank lines are unreclaimable.
        assert!(!out.contains('\n'), "first frame must not emit a newline");
    }

    /// The load-bearing anti-flicker property: a same-height repaint clears
    /// each row and rewrites it *immediately* before descending — never the
    /// "blank every row, then rewrite every row" two-phase that flickered. The
    /// full byte sequence is asserted, so any regression to a clear-all-then-
    /// write-all repaint fails here.
    #[test]
    fn frame_bytes_repaint_is_interleaved_clear_then_write() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = frame_bytes(&rows, 3);
        // up to top (2 rows), then per row: CR + clear + write, descending.
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
        // each cleared row is rewritten before the next is touched: no run of
        // two clears without content between them.
        assert!(
            !out.contains(&format!("{CLR_LINE}{CLR_LINE}")),
            "adjacent clears would blank multiple rows at once (flicker)"
        );
    }

    /// A shorter new frame blanks the stale tail rows below it and returns the
    /// cursor to the last live row, so a shrunken window leaves no ghosts.
    #[test]
    fn frame_bytes_shorter_frame_clears_stale_tail() {
        let out = frame_bytes(&["a".to_string()], 3);
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}{DOWN}\r{CLR_LINE}{UP}{UP}");
        assert_eq!(out, expected);
    }

    /// A taller new frame descends into fresh rows below the previous frame —
    /// each new row cleared then written in place as it appears.
    #[test]
    fn frame_bytes_taller_frame_descends_into_new_rows() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let out = frame_bytes(&rows, 1);
        let expected = format!("\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
    }

    /// Erasing a frame clears each row from the bottom up — clear the current
    /// (bottom) row, ascend, repeat — so the traversal naturally ends on the
    /// frame's TOP row (the cursor's line at stream start), with no separate
    /// return walk and no blank gap left above the next line of stderr.
    #[test]
    fn clear_frame_bytes_erases_bottom_up_and_ends_at_top() {
        // 2-row frame: clear the bottom row, ascend, clear the top row. Cursor
        // was hidden → SHOW restores it. Ends on the top row by construction.
        let out = clear_frame_bytes(2, true);
        assert_eq!(out, format!("\r{CLR_LINE}{UP}\r{CLR_LINE}{SHOW}"));
        // 1-row frame: a single clear, no movement.
        assert_eq!(clear_frame_bytes(1, true), format!("\r{CLR_LINE}{SHOW}"));
    }

    /// Regression: the reasoning stream must leave NO trace — neither a blank
    /// gap nor a reserved blank line — between the output that preceded the
    /// block and the output that follows it. The frame paints in place at the
    /// cursor's current line (no opening newline), so its top row is the
    /// cursor's start row; `finish` must restore the cursor to that same row
    /// and the whole stream must emit zero newlines (a `\n` is an unreclaimable
    /// blank line in a terminal, the original cause of the lingering empty
    /// area). The old design opened with a `\n` and parked the cursor on the
    /// cleared region's bottom row, leaving several blank rows
    /// above the next write — the "empty area" after thinking ended.
    #[test]
    fn finish_leaves_no_blank_gap_above_next_output() {
        /// Walk `bytes` from `start_row` and return the final cursor row,
        /// honouring only the moves the renderer emits (`\n`, `\r`, UP, DOWN;
        /// clears and cursor-visibility toggles don't move the row).
        fn cursor_row_after(bytes: &str, start_row: i32) -> i32 {
            let mut row = start_row;
            let mut i = 0;
            while i < bytes.len() {
                if bytes[i..].starts_with('\n') {
                    row += 1;
                    i += 1;
                } else if bytes[i..].starts_with('\r') {
                    i += 1;
                } else if bytes[i..].starts_with(UP) {
                    row -= 1;
                    i += UP.len();
                } else if bytes[i..].starts_with(DOWN) {
                    row += 1;
                    i += DOWN.len();
                } else if bytes[i..].starts_with(CLR_LINE) {
                    i += CLR_LINE.len();
                } else if bytes[i..].starts_with(HIDE) {
                    i += HIDE.len();
                } else if bytes[i..].starts_with(SHOW) {
                    i += SHOW.len();
                } else {
                    i += 1;
                }
            }
            row
        }

        for height in [1usize, 2, 3, 12, 13] {
            let rows: Vec<String> = (0..height).map(|_| "x".to_string()).collect();
            let paint = frame_bytes(&rows, 0);
            // The stream never emits a newline: a `\n` is a permanently blank
            // line the terminal cannot un-scroll, so the whole in-place stream
            // (paint + erase) must be newline-free to leave no trace.
            assert!(
                !paint.contains('\n'),
                "height {height}: first frame must not emit a newline"
            );
            // The frame paints at the cursor's line, so its bottom row is
            // `height - 1` and that's where the cursor sits after painting.
            let after_paint = cursor_row_after(&paint, 0);
            assert_eq!(after_paint, height as i32 - 1, "paint height {height}");
            // finish must restore the cursor to its START row (0), not the
            // bottom — otherwise `height - 1` blank rows linger above the next
            // line of stderr.
            let after_clear = cursor_row_after(&clear_frame_bytes(height, true), after_paint);
            assert_eq!(
                after_clear, 0,
                "height {height}: cursor must return to start row (0), \
                 not the bottom (row {after_paint})"
            );
        }
    }

    /// The cursor is hidden for the whole reasoning stream and restored only
    /// when thinking ends — otherwise the caret would rest, row by row, at the
    /// tail of each freshly written line (mid-row over the previous frame's
    /// longer text) and read as "jumping between characters" during fast
    /// streaming. The first frame hides it; mid-stream repaints leave it hidden
    /// (no per-frame flicker); the final erase is the only thing that restores
    /// it.
    #[test]
    fn cursor_hidden_for_whole_stream_restored_only_at_finish() {
        // The first frame (prev_height 0) hides the cursor and never restores
        // it — the caret is gone for the entire stream.
        let first = frame_bytes(&["only".to_string()], 0);
        assert!(first.starts_with(HIDE), "first frame must hide the cursor");
        assert!(
            !first.contains(SHOW),
            "first frame must not restore mid-stream"
        );

        // A mid-stream repaint touches neither: the caret stays hidden, so its
        // position during the traversal can't smear across the repainted rows.
        let repaint = frame_bytes(&["a".to_string(), "b".to_string()], 2);
        assert!(
            !repaint.contains(HIDE) && !repaint.contains(SHOW),
            "a mid-stream repaint must not touch cursor visibility"
        );

        // The cursor only comes back when the stream ends (clear_frame_bytes),
        // keyed on the renderer's cursor-hidden flag — not on prev_height.
        let erase = clear_frame_bytes(2, true);
        assert!(erase.ends_with(SHOW), "finish must restore the cursor");
        // A frame that hid no cursor owes no SHOW — geometry only (bottom-up
        // clear ending at the top), no restore.
        assert_eq!(
            clear_frame_bytes(2, false),
            format!("\r{CLR_LINE}{UP}\r{CLR_LINE}")
        );
    }

    /// A zero-height frame that was never painted (`cursor_hidden == false`)
    /// is a no-op — keeps [`ReasoningRenderer::finish`] trivially safe when no
    /// frame was drawn.
    #[test]
    fn clear_frame_bytes_noop_for_zero_rows() {
        assert_eq!(clear_frame_bytes(0, false), "");
    }

    /// The `SHOW` owed is decided by `cursor_hidden`, not `prev_height`: a
    /// zero-row frame whose first draw still hid the cursor must restore it,
    /// even though there are no rows to erase. This is exactly the case that
    /// inferring visibility from `prev_height` would leak — hiding with no
    /// painted row, then finishing with `prev_height == 0` → no `SHOW`.
    #[test]
    fn clear_frame_bytes_restores_cursor_for_zero_row_hidden_frame() {
        assert_eq!(clear_frame_bytes(0, true), SHOW);
    }
}
