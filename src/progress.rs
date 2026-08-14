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
//! The Markdown painter itself — line → styled rows — lives in
//! [`crate::markdown`]; the renderer here frames what it returns.
//! The cursor-row probe that sizes the reasoning window to the rows below the
//! prompt lives in [`crate::cursor`]; the renderer here takes its result as
//! plain row numbers.

use console::Term;
use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use crate::layout::{MARGIN, terminal_height, terminal_width};
use crate::markdown::{LoadingNotice, classify_line, loading_rows, reasoning_rows};

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
/// While reasoning is actively streaming, idle-tick repaints are suppressed:
/// the flowing text is itself the motion, so a tick repaint would only
/// re-flash the stable rows (the original "chunky" cause). Once the model
/// goes silent for longer than this, ticks resume repainting so the spinner
/// keeps animating and the elapsed count keeps rising — the stall feedback.
/// Roughly three tick periods: short enough that a stall is noticed promptly,
/// long enough that a normal inter-delta gap (TTFT between tokens) never trips
/// it.
// ponytail: tuned by eye; raise if streaming still flashes, lower if the
// spinner feels sluggish to restart after a stall.
pub(crate) const ACTIVE_THRESHOLD: Duration = Duration::from_millis(150);

/// Per-row stagger of the dissolution at [`ReasoningRenderer::finish`]: each
/// row is erased one tick apart so the block visibly *dissolves* row-by-row
/// (oldest last, bottom-up) rather than blinking out wholesale. Total dissolve
/// ≈ `rows × DISSOLVE_STEP`, bounded because the row cap is bounded.
// ponytail: visual tuning — raise for a slower, more legible dissolve, lower
// if it drags the end of a fast commit.
const DISSOLVE_STEP: Duration = Duration::from_millis(70);

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

/// Run N concurrent futures, each behind its own bar on one shared
/// [`indicatif::MultiProgress`], labeled `[i/N] {label}`. Unlike N standalone
/// [`with_spinner`] calls — whose independent [`indicatif::ProgressBar`]s
/// collide on a single terminal line (only one clears, the rest leave residue)
/// — a `MultiProgress` gives each bar its own row and clears it on completion,
/// so concurrent drafts render cleanly. Futures are polled `concurrency` at a
/// time via order-preserving `buffered`, keeping call order (and the test
/// messengers' per-call counters) deterministic. The outer `Result` is a setup
/// failure (a malformed style); each inner `Result` is one future's outcome.
pub(crate) async fn with_indexed_spinners<T>(
    label: &str,
    concurrency: usize,
    futs: impl IntoIterator<Item = crate::types::BoxFuture<anyhow::Result<T>>>,
) -> anyhow::Result<Vec<anyhow::Result<T>>>
where
    T: Send + 'static,
{
    use futures::stream::{self, StreamExt};
    let futs: Vec<crate::types::BoxFuture<anyhow::Result<T>>> = futs.into_iter().collect();
    let count = futs.len();
    let mp = indicatif::MultiProgress::new();
    let style = spinner_style()?;
    let tracked: Vec<crate::types::BoxFuture<anyhow::Result<T>>> = futs
        .into_iter()
        .enumerate()
        .map(|(i, fut)| -> crate::types::BoxFuture<anyhow::Result<T>> {
            let bar = mp.add(indicatif::ProgressBar::new_spinner());
            bar.set_style(style.clone());
            bar.set_message(format!("[{}/{}] {label}", i + 1, count));
            bar.enable_steady_tick(SPINNER_TICK);
            Box::pin(async move {
                let r = fut.await;
                bar.disable_steady_tick();
                bar.finish_and_clear();
                r
            })
        })
        .collect();
    Ok(stream::iter(tracked).buffered(concurrency).collect().await)
}

/// A rolling window over the model's streamed reasoning, sized to the caller's
/// `cap` — the rendered-row budget from [`crate::cursor::reasoning_window_rows`], reused as
/// the line-storage bound. Each line renders to at least one row, so at most
/// `cap` lines are ever visible at once; that makes `cap` an exact upper
/// bound on retained lines, not an approximation. The post-wrap rendered-row
/// cap (one line can wrap to several rows) is applied separately in
/// [`reasoning_rows`]. The caller renders [`push`](Self::push)'s returned
/// window into the spinner's in-place multi-line message, so the block redraws
/// in place — never printed as permanent lines that linger in the scrollback —
/// and is erased once thinking ends ([`ReasoningRenderer::finish`]), leaving
/// the terminal clean for the rest of the run. The cap keeps the block bounded
/// while it streams, so no unbounded region ever accumulates.
///
/// `cap` is sized by the caller from [`crate::cursor::reasoning_window_rows`], so a larger
/// terminal retains more reasoning instead of aging it out at a fixed rate.
/// Completed lines (terminated by `\n`) roll into the window; the in-progress
/// partial line (no trailing `\n` yet) is shown as the window's last line while
/// it builds and counts against the same budget. Blank lines are dropped to
/// keep the feed information-dense.
///
/// The view also tracks the markdown fence state of the **whole stream**, not
/// just the visible window: each stored line remembers whether it started
/// inside a fenced code block. [`push`](Self::push) reports the state entering
/// the window it returns, so [`reasoning_rows`] classifies the visible lines
/// exactly as a full-stream scan would — a code block whose opener scrolled
/// out of the window still colours its content, and a closer seen after the
/// opener left the window still closes instead of flipping the state open
/// again (which would render following prose as code).
pub(crate) struct ThinkingView {
    /// Completed lines, each with the fence state that was current when the
    /// line STARTED (before its own classification ran).
    lines: VecDeque<(String, bool)>,
    cur: String,
    cap: usize,
    /// Running fence state after the last completed line — the state entering
    /// the next line, and the state entering the in-progress partial.
    in_code: bool,
}

impl ThinkingView {
    pub(crate) fn new(cap: usize) -> Self {
        Self {
            lines: VecDeque::with_capacity(cap),
            cur: String::new(),
            cap,
            in_code: false,
        }
    }

    /// Ingest a reasoning delta (may be a partial line, many lines, or empty)
    /// and return the current window — the newest completed lines plus the
    /// in-progress partial line, oldest-first, capped to `cap` lines — along
    /// with the fence state entering the window's first line (see the type
    /// doc). A delta that ends mid-line leaves the partial buffered and shown
    /// as the window's last line until the next `\n`.
    pub(crate) fn push(&mut self, delta: &str) -> (Vec<String>, bool) {
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
        (self.window(), self.window_start_in_code())
    }

    /// Append a completed line: classify it against the running fence state,
    /// store the entering state with the line, then bound *storage* to `cap`
    /// — not the visible window. This stops a long chain-of-thought from
    /// retaining every completed line forever; the visible cap lives in
    /// [`window`](Self::window), which also accounts for the in-progress
    /// partial row.
    fn push_line(&mut self, line: String) {
        let entering = self.in_code;
        let (_, _, next) = classify_line(&line, entering);
        self.in_code = next;
        self.lines.push_back((line, entering));
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
        let mut rows: Vec<String> = self.lines.iter().map(|(l, _)| l.clone()).collect();
        if !self.cur.trim().is_empty() {
            rows.push(self.cur.clone());
        }
        let start = rows.len().saturating_sub(self.cap);
        rows[start..].to_vec()
    }

    /// The fence state entering the current window's first line: the state
    /// stored with that line (captured when it was pushed, i.e. after all
    /// lines before it — including ones that have since rolled out). A window
    /// that is only the in-progress partial enters at the running state.
    fn window_start_in_code(&self) -> bool {
        let rows = self.lines.len() + usize::from(!self.cur.trim().is_empty());
        let start = rows.saturating_sub(self.cap);
        match self.lines.get(start) {
            Some((_, entering)) => *entering,
            None => self.in_code,
        }
    }
}

/// Braille frames for the analysis spinner. Advanced once per reasoning
/// delta (see [`ReasoningRenderer`]) so the glyph spins while reasoning flows
/// and freezes when it stalls — incidental animation that needs no background
/// ticker, since a steady tick is exactly what forced indicatif's flickering
/// full-block repaints.
const REASONING_GLYPHS: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

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
    /// from the real terminal height ([`crate::cursor::reasoning_window_rows`]) so the view
    /// and the renderer share one notion of the window size; a resize
    /// mid-stream only changes wrap widths, never the row budget.
    max_rows: usize,
    /// Terminal height in rows (same 0→24 fallback as
    /// [`crate::layout::terminal_height`]) — the bottom margin past which a
    /// descent scrolls instead of clamping.
    height: usize,
    /// Absolute 1-based row of the cursor at the start of the next paint —
    /// the frame's bottom row after the previous paint, or the DSR-reported
    /// row before the first. `None` when the cursor could not be queried:
    /// frames then never scroll (legacy behavior).
    row: Option<usize>,
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
    /// The last frame's rows, for incremental repaint. On a typical delta only
    /// the in-progress partial line (the bottom row) grows; everything above is
    /// byte-identical, so redrawing just that one row — instead of clearing and
    /// rewriting the whole block — is what makes streaming read as fluid growth
    /// rather than a flashing block. Structural changes (a line completing and
    /// the window rolling, a row expiring, a height change) fall back to a full
    /// [`frame_bytes`] repaint.
    prev_rows: Vec<String>,
    /// Frozen spinner elapsed-time shown while streaming is active. The spinner
    /// must NOT change between deltas (or the bottom-row-only diff would see a
    /// changed top row and fall back to a full repaint, reintroducing the
    /// flash), so it advances only on a stall tick via [`Self::refresh`].
    shown_elapsed: Duration,
    /// Wall-clock of the last content delta. While `Instant::now() - this` <
    /// [`ACTIVE_THRESHOLD`], [`Self::refresh`] is a no-op — the stream is the
    /// motion, a tick repaint would only flash stable rows. `None` until the
    /// first paint.
    last_delta: Option<std::time::Instant>,
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
/// `start_row` is the absolute 1-based terminal row of the cursor when the
/// paint begins (`None` when the DSR query failed — the legacy no-scroll
/// behavior, since the bottom margin is then unknowable), and `height` the
/// terminal's row count. Descending **past** the bottom margin is the one
/// move the terminal won't make for us: cursor-down clamps at the margin
/// instead of scrolling, which is exactly the collapse that made tall frames
/// overwrite the bottom row. So a descent from the last row emits `\n`
/// instead — a scroll — shifting the screen up one row and giving the frame
/// a fresh bottom row. The scroll shifts the frame's own top row up with
/// everything else, so the erase contract still holds: [`clear_frame_bytes`]
/// walks up `prev_height - 1` rows from the frame's bottom and lands on the
/// frame's top, which is wherever the prompt (and the frame) drifted to.
/// Returns the assembled bytes plus the cursor's absolute row afterwards
/// (`None` when the start was unknown), which the renderer feeds back as the
/// next paint's `start_row`.
///
/// Cursor visibility is a stream-spanning concern, not a per-frame one. The
/// first repaint emits [`HIDE`] (when `prev_height == 0`) and the caret stays
/// hidden for the whole stream; only [`clear_frame_bytes`] at
/// [`ReasoningRenderer::finish`] restores it ([`SHOW`]). Mid-stream repaints
/// emit neither — the caret stays hidden, so where it sits during the traversal
/// is irrelevant and can never smear across the repainted rows.
fn frame_bytes(
    rows: &[String],
    prev_height: usize,
    start_row: Option<usize>,
    height: usize,
) -> (String, Option<usize>) {
    let height = height.max(1);
    // The frame's top row: where the cursor is now, walked up by the repaint
    // preamble. `prev_height == 0` has no preamble — the frame starts at the
    // cursor's own row.
    let top = start_row.map(|r| r.min(height).saturating_sub(prev_height.saturating_sub(1)));
    let frame_height = rows.len();
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
    // Absolute row of the cursor as we descend; tracked only when known.
    let mut row = top;
    for (i, r) in rows.iter().enumerate() {
        out.push('\r');
        out.push_str(CLR_LINE);
        out.push_str(r);
        if i + 1 < frame_height {
            match row {
                Some(cur) if cur >= height => {
                    // At the bottom margin, cursor-down would clamp (the
                    // collapse) — scroll the screen up instead so the frame
                    // gains a fresh bottom row. The cursor stays on the last
                    // row, everything above it shifted up one.
                    out.push('\n');
                }
                _ => {
                    out.push_str(DOWN);
                }
            }
            if let Some(cur) = row.as_mut()
                && *cur < height
            {
                *cur += 1;
            }
        }
    }
    if frame_height < prev_height {
        // Stale tail below the shorter frame: blank each row and walk back.
        // The walk ends at or above the frame's own bottom (the old frame
        // never painted past the new one's bottom unless both hit the bottom
        // margin, in which case there is no tail), so it never scrolls.
        for _ in frame_height..prev_height {
            out.push_str(DOWN);
            out.push('\r');
            out.push_str(CLR_LINE);
        }
        for _ in frame_height..prev_height {
            out.push_str(UP);
        }
    }
    let end_row = top.map(|t| (t + frame_height - 1).min(height));
    (out, end_row)
}

/// Assemble the byte sequence to erase a `prev_height`-row frame, clearing
/// each row from the BOTTOM up so the traversal naturally ends on the frame's
/// TOP row — exactly where the first frame began painting (the cursor's line
/// at stream start, which scrolled frames carry upward with them, so it is
/// still the prompt's row when the stream ends). So once thinking ends the
/// cursor sits back on that line, the block is gone, and the next line of
/// stderr overwrites the blank region from the top with no gap trapped above
/// or below.
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

/// Whether `new` differs from `prev` only in its last row — same length, same
/// every row above. This is the common streaming case: one token grew the
/// in-progress partial line (the bottom row) and nothing else moved. When true,
/// [`ReasoningRenderer::draw_rows`] rewrites just that bottom row in place
/// instead of clearing and rewriting the whole block, which is what makes the
/// feed read as fluid character growth rather than a flashing block. A roll
/// (a line completed and the window shifted), a row count change, or any change
/// above the bottom row all return `false` → full [`frame_bytes`] repaint.
fn incremental_bottom(new: &[String], prev: &[String]) -> bool {
    !new.is_empty()
        && new.len() == prev.len()
        && prev[new.len() - 1] != new[new.len() - 1]
        && prev[..new.len() - 1] == new[..new.len() - 1]
}
impl ReasoningRenderer {
    /// Bind a renderer to stderr with `label` on the spinner row. `max_rows`
    /// is the reasoning window's rendered-row cap and `cursor_row` the
    /// DSR-reported cursor row (`None` if the query failed — scrolling is
    /// then disabled), both from [`crate::cursor::reasoning_window_rows`]. `feed_width` is
    /// resolved once from the shared terminal geometry ([`terminal_width`]),
    /// floored at the progress surface's [`MIN_PROGRESS_WIDTH`] so the
    /// spinner and its label keep room. A resize mid-stream only changes
    /// wrap widths, never correctness.
    pub(crate) fn new(label: &'static str, max_rows: usize, cursor_row: Option<usize>) -> Self {
        Self {
            term: Term::stderr(),
            label,
            feed_width: terminal_width().max(MIN_PROGRESS_WIDTH).saturating_sub(6),
            max_rows,
            height: terminal_height(),
            row: cursor_row,
            glyph: 0,
            prev_height: 0,
            active: false,
            cursor_hidden: false,
            prev_rows: Vec::new(),
            shown_elapsed: Duration::ZERO,
            last_delta: None,
        }
    }

    /// Paint one frame for a content delta. The spinner is **frozen** here —
    /// neither the glyph nor its elapsed count advances — so between tokens
    /// only the in-progress line grows, and [`draw_rows`] can rewrite just that
    /// one bottom row. That single-row update (vs. clearing and rewriting the
    /// whole block every token) is the difference between fluid character
    /// growth and the flashing "block-by-block" feed. The spinner animates
    /// instead on stall ticks via [`Self::refresh`]. A no-op off a terminal.
    pub(crate) fn paint(&mut self, window: &[String], in_code_start: bool) {
        if !self.term.is_term() {
            return;
        }
        let glyph = REASONING_GLYPHS[self.glyph % REASONING_GLYPHS.len()];
        let rows = reasoning_rows(
            glyph,
            self.label,
            window,
            self.feed_width,
            Some(self.shown_elapsed),
            self.max_rows,
            in_code_start,
        );
        self.draw_rows(&rows);
        self.last_delta = Some(std::time::Instant::now());
    }

    /// Idle-tick repaint. While streaming is active (`last_delta` within
    /// [`ACTIVE_THRESHOLD`]) this is a no-op: the flowing text is itself the
    /// motion, and a repaint would only re-flash the stable rows above it — the
    /// original chunky symptom. Once the model falls silent past the threshold,
    /// the spinner advances and the elapsed count rises so the wait stays
    /// visible across a long TTFT gap without the feed ever freezing. A no-op
    /// off a terminal.
    pub(crate) fn refresh(&mut self, window: &[String], in_code_start: bool, elapsed: Duration) {
        if !self.term.is_term() {
            return;
        }
        if let Some(t) = self.last_delta
            && t.elapsed() < ACTIVE_THRESHOLD
        {
            return;
        }
        self.glyph = self.glyph.wrapping_add(1);
        self.shown_elapsed = elapsed;
        let glyph = REASONING_GLYPHS[self.glyph % REASONING_GLYPHS.len()];
        let rows = reasoning_rows(
            glyph,
            self.label,
            window,
            self.feed_width,
            Some(elapsed),
            self.max_rows,
            in_code_start,
        );
        self.draw_rows(&rows);
    }

    /// Paint one loading frame for the silent/cold-start backend state: the
    /// spinner row annotated with `elapsed` seconds, plus — once `notice` is
    /// past [`LOADING_GRACE`]/classified — an explanatory row. Used by the
    /// [`reasoning_feed`](crate::reasoning_feed) driver between stream start
    /// and the first reasoning delta so a non-streaming or cold-starting
    /// backend is not a silent dead zone: the user always sees motion (the spinning
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

    /// Repaint `rows` in place. The common streaming case — only the bottom
    /// row grew — rewrites that one row where the cursor already sits, skipping
    /// the full clear+rewrite entirely. Structural changes (first frame, a line
    /// completing so the window rolls, a row expiring, a height change) fall
    /// back to [`frame_bytes`]'s anti-flicker full repaint. Pure byte assembly
    /// lives in [`frame_bytes`] / [`incremental_bottom`] so it stays unit-testable.
    fn draw_rows(&mut self, rows: &[String]) {
        let first_frame = self.prev_rows.is_empty();
        // Identical frame (a whitespace-only delta ThinkingView dropped, or a
        // stall tick whose window hadn't moved): writing it would only flash.
        if !first_frame && rows == self.prev_rows.as_slice() {
            return;
        }
        // Record cursor-hidden BEFORE the write: the flag mirrors exactly when
        // frame_bytes emits HIDE (first frame), so tracking it ahead of the
        // side effect means a hidden cursor can never be stranded — even a
        // panic between here and the write leaves Drop seeing the owed SHOW.
        self.cursor_hidden |= first_frame;
        if !first_frame && incremental_bottom(rows, &self.prev_rows) {
            // The cursor already rests on the bottom row (every paint ends
            // there), so a bare CR + clear + rewrite of just that row is the
            // entire update — no ascent, no descent, stable rows untouched.
            let last = rows.last().expect("non-empty by incremental_bottom");
            let mut buf = String::from("\r");
            buf.push_str(CLR_LINE);
            buf.push_str(last);
            let _ = self.term.write_str(&buf);
            let _ = self.term.flush();
        } else {
            let (bytes, end_row) = frame_bytes(rows, self.prev_height, self.row, self.height);
            let _ = self.term.write_str(&bytes);
            let _ = self.term.flush();
            self.row = end_row;
        }
        self.prev_rows.clear();
        self.prev_rows.extend_from_slice(rows);
        self.prev_height = rows.len();
        self.active = true;
    }

    /// End the reasoning stream by **dissolving** the frame one row at a time,
    /// each erased [`DISSOLVE_STEP`] apart, so the block visibly retreats
    /// row-by-row instead of blinking out wholesale — the "linger then exit
    /// one-by-one" close. The erase runs bottom-up (matching the terminal's
    /// scroll direction) and, like the old one-shot clear, ends on the frame's
    /// top row — exactly where the first frame began — so the cursor is
    /// restored to the prompt's line and the next stderr output continues with
    /// no gap above or below. Idempotent.
    ///
    /// The staggered waits are decoration at the *end* of analysis (the stream
    /// is already complete), replacing the old fixed read-tail hold-then-erase:
    /// the dissolve itself is the readability window. An *aborted* stream takes
    /// the fast path in [`Drop`] instead, never blocking on decoration.
    pub(crate) fn finish(&mut self) {
        if !self.active {
            return;
        }
        let n = self.prev_height;
        for i in 0..n {
            // Erase the lowest remaining row, flush so it shows, then ascend to
            // the next — ending on the top row after n-1 ascents.
            let mut buf = String::from("\r");
            buf.push_str(CLR_LINE);
            let _ = self.term.write_str(&buf);
            let _ = self.term.flush();
            if i + 1 < n {
                let _ = self.term.write_str(UP);
                let _ = self.term.flush();
                std::thread::sleep(DISSOLVE_STEP);
            }
        }
        if self.cursor_hidden {
            let _ = self.term.write_str(SHOW);
            let _ = self.term.flush();
        }
        self.prev_rows.clear();
        self.prev_height = 0;
        self.active = false;
        self.cursor_hidden = false;
    }

    /// Fast wholesale erase — the [`Drop`] backstop for a stream that aborted
    /// before [`finish`](Self::finish). Same byte sequence as the legacy
    /// one-shot clear (bottom-up, ending on the top row, [`SHOW`] if the cursor
    /// was hidden) but with no staggered waits, so an error or panic path never
    /// hangs on decoration.
    fn erase_frame(&mut self) {
        let bytes = clear_frame_bytes(self.prev_height, self.cursor_hidden);
        let _ = self.term.write_str(&bytes);
        let _ = self.term.flush();
        self.prev_rows.clear();
        self.prev_height = 0;
        self.active = false;
        self.cursor_hidden = false;
    }
}

/// Production [`reasoning_feed::ReasoningSink`]. Each method forwards to the
/// inherent method of the same name in `impl ReasoningRenderer` above — Rust's
/// method resolution prefers inherent over trait, so a `self.paint` body here
/// is *not* a recursive call. The trait seam exists so the driver loop can run
/// against a recording fake in tests; this is the only production sink.
impl crate::reasoning_feed::ReasoningSink for ReasoningRenderer {
    fn paint(&mut self, window: &[String], in_code_start: bool) {
        self.paint(window, in_code_start)
    }
    fn paint_loading(&mut self, elapsed: Duration, notice: LoadingNotice) {
        self.paint_loading(elapsed, notice)
    }
    fn refresh(&mut self, window: &[String], in_code_start: bool, elapsed: Duration) {
        self.refresh(window, in_code_start, elapsed)
    }
    fn finish(&mut self) {
        self.finish()
    }
}

impl Drop for ReasoningRenderer {
    fn drop(&mut self) {
        // Backstop: if the stream aborted before finish(), erase the frame fast
        // (no staggered dissolve) so the rest of the run's stderr is clean.
        if self.active {
            self.erase_frame();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::markdown::LineKind;

    /// [`ThinkingView::push`] returns the current window: completed lines
    /// (blank ones dropped) in arrival order, oldest-first.
    #[test]
    fn thinking_view_window_shows_completed_lines_and_drops_blanks() {
        let mut v = ThinkingView::new(12);
        let (window, in_code) = v.push("line 1\n\nline 2\n");
        assert_eq!(window, vec!["line 1", "line 2"]);
        assert!(!in_code); // plain prose: no fence state
    }

    /// A partial line with no trailing `\n` is the window's last row while it
    /// builds, then collapses into a completed row when the `\n` arrives.
    #[test]
    fn thinking_view_partial_is_last_window_row_until_newline() {
        let mut v = ThinkingView::new(12);
        assert_eq!(v.push("in progress").0, vec!["in progress"]);
        let (window, _) = v.push(" done\n");
        assert_eq!(window, vec!["in progress done"]);
    }

    /// One logical line split across several deltas assembles into a single
    /// window row, shown live as it grows.
    #[test]
    fn thinking_view_assembles_split_chunks() {
        let mut v = ThinkingView::new(12);
        assert_eq!(v.push("hel").0, vec!["hel"]);
        assert_eq!(v.push("lo").0, vec!["hello"]);
        assert_eq!(v.push(" world\n").0, vec!["hello world"]);
    }

    /// A delta containing several `\n`-separated lines yields a window with
    /// each one, in order.
    #[test]
    fn thinking_view_many_lines_one_delta() {
        let mut v = ThinkingView::new(12);
        assert_eq!(v.push("a\nb\nc\n").0, vec!["a", "b", "c"]);
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
        let (window, _) = v.push("");
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
        let (window, _) = v.push("in progress");
        assert_eq!(window.len(), 12);
        // oldest completed row rolled out to make room for the partial
        assert_eq!(window.first(), Some(&"line 2".to_string()));
        assert_eq!(window.last(), Some(&"in progress".to_string()));
    }

    /// First paint (no previous frame) hides the cursor and clears+writes its
    /// single row in place at the cursor's current line — no leading newline
    /// (which would reserve an unreclaimable blank line above the block) and
    /// no cursor-up preamble. With the cursor unknown, the paint never
    /// scrolls (the legacy behavior).
    #[test]
    fn frame_bytes_first_frame_paints_in_place() {
        let (out, end_row) = frame_bytes(&["only".to_string()], 0, None, 24);
        assert_eq!(out, format!("{HIDE}\r{CLR_LINE}only"));
        // the first frame never emits a newline: blank lines are unreclaimable.
        assert!(!out.contains('\n'), "first frame must not emit a newline");
        // unknown start row → unknown end row, and no scroll bytes.
        assert_eq!(end_row, None);
    }

    /// The load-bearing anti-flicker property: a same-height repaint clears
    /// each row and rewrites it *immediately* before descending — never the
    /// "blank every row, then rewrite every row" two-phase that flickered. The
    /// full byte sequence is asserted, so any regression to a clear-all-then-
    /// write-all repaint fails here.
    #[test]
    fn frame_bytes_repaint_is_interleaved_clear_then_write() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (out, end_row) = frame_bytes(&rows, 3, None, 24);
        // up to top (2 rows), then per row: CR + clear + write, descending.
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
        assert_eq!(end_row, None);
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
        let (out, end_row) = frame_bytes(&["a".to_string()], 3, None, 24);
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}{DOWN}\r{CLR_LINE}{UP}{UP}");
        assert_eq!(out, expected);
        assert_eq!(end_row, None);
    }

    /// A taller new frame descends into fresh rows below the previous frame —
    /// each new row cleared then written in place as it appears.
    #[test]
    fn frame_bytes_taller_frame_descends_into_new_rows() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (out, end_row) = frame_bytes(&rows, 1, None, 24);
        let expected = format!("\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
        assert_eq!(end_row, None);
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

    /// The incremental-repaint guard: only a lone change to the bottom row
    /// (the in-progress line growing by one token) qualifies for the cheap
    /// single-row rewrite. A roll (window shifted), a height change, a change
    /// above the bottom, or identical frames all fall back to a full repaint —
    /// the cases that would corrupt the screen if redrawn as a single row.
    #[test]
    fn incremental_bottom_only_when_lone_bottom_row_grew() {
        let row = |s: &str| s.to_string();
        // Bottom row grew, everything above identical → incremental.
        assert!(incremental_bottom(
            &[row("a"), row("b"), row("grow")],
            &[row("a"), row("b"), row("gr")]
        ));
        // Identical → not incremental (a no-op write, handled before this fn,
        // but the predicate must not claim a phantom bottom change either).
        assert!(!incremental_bottom(
            &[row("a"), row("b")],
            &[row("a"), row("b")]
        ));
        // A row ABOVE the bottom changed (e.g. the spinner unfroze) → full
        // repaint, never a lone bottom rewrite.
        assert!(!incremental_bottom(
            &[row("A"), row("b"), row("c")],
            &[row("a"), row("b"), row("c")]
        ));
        // Height changed (a line completed, window rolled/grew) → full repaint.
        assert!(!incremental_bottom(&[row("a"), row("b")], &[row("a")]));
        assert!(!incremental_bottom(&[row("a")], &[row("a"), row("b")]));
        // Empty is never incremental.
        assert!(!incremental_bottom(&[], &[row("a")]));
        assert!(!incremental_bottom(&[row("a")], &[]));
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
            // Cursor unknown (`None`): the legacy no-scroll geometry — a
            // mid-screen stream that never touches the bottom margin.
            let (paint, _) = frame_bytes(&rows, 0, None, 24);
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
        let (first, _) = frame_bytes(&["only".to_string()], 0, None, 24);
        assert!(first.starts_with(HIDE), "first frame must hide the cursor");
        assert!(
            !first.contains(SHOW),
            "first frame must not restore mid-stream"
        );

        // A mid-stream repaint touches neither: the caret stays hidden, so its
        // position during the traversal can't smear across the repainted rows.
        let (repaint, _) = frame_bytes(&["a".to_string(), "b".to_string()], 2, None, 24);
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

    /// The load-bearing scroll: descending **past** the bottom margin is the
    /// one move the terminal refuses (cursor-down clamps — the collapse that
    /// made tall frames overwrite the bottom row), so the renderer emits `\n`
    /// — a scroll that shifts the screen up and grants the frame a fresh
    /// bottom row. A first frame at the very bottom of the screen (the usual
    /// shell-prompt position) thus paints every row, scrolling once per row
    /// past the first, instead of collapsing.
    #[test]
    fn frame_bytes_scrolls_when_descending_past_bottom_margin() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (out, end_row) = frame_bytes(&rows, 0, Some(24), 24);
        let expected = format!("{HIDE}\r{CLR_LINE}a\n\r{CLR_LINE}b\n\r{CLR_LINE}c");
        assert_eq!(out, expected);
        // The frame's bottom row is the screen's last row; the cursor stays
        // there for the next paint (the top drifted up with the scrolls).
        assert_eq!(end_row, Some(24));
        assert_eq!(
            out.matches('\n').count(),
            2,
            "one scroll per descent past the margin"
        );
    }

    /// A frame that fits in the rows below the cursor never scrolls — the
    /// screen stays put and the cursor ends on the frame's bottom row.
    #[test]
    fn frame_bytes_fits_without_scroll_when_room_below() {
        let rows = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        let (out, end_row) = frame_bytes(&rows, 0, Some(10), 24);
        let expected = format!("{HIDE}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c");
        assert_eq!(out, expected);
        assert_eq!(end_row, Some(12));
    }

    /// A frame already pinned to the bottom margin grows by scrolling exactly
    /// once for the new row — the repaint preamble still reaches the (drifted)
    /// frame top, and the cursor reports the bottom row for the next paint.
    #[test]
    fn frame_bytes_growth_from_bottom_scrolls_once() {
        let rows = vec![
            "a".to_string(),
            "b".to_string(),
            "c".to_string(),
            "d".to_string(),
        ];
        let (out, end_row) = frame_bytes(&rows, 3, Some(24), 24);
        let expected =
            format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}b{DOWN}\r{CLR_LINE}c\n\r{CLR_LINE}d");
        assert_eq!(out, expected);
        assert_eq!(end_row, Some(24));
    }

    /// A frame that shrinks after the screen scrolled clears its stale tail
    /// and reports the new (higher) bottom row, so the next paint's preamble
    /// lands on the correct — drifted — frame top.
    #[test]
    fn frame_bytes_shrink_after_scroll_clears_tail_and_reports_row() {
        let (out, end_row) = frame_bytes(&["a".to_string()], 3, Some(24), 24);
        let expected = format!("{UP}{UP}\r{CLR_LINE}a{DOWN}\r{CLR_LINE}{DOWN}\r{CLR_LINE}{UP}{UP}");
        assert_eq!(out, expected);
        assert_eq!(end_row, Some(22));
    }

    /// The erase contract holds after scrolling: the frame's top row drifts
    /// up with the scrolls (it is wherever the prompt line drifted to), and
    /// [`clear_frame_bytes`]'s bottom-up walk — from the frame's bottom row,
    /// `prev_height - 1` ascents — lands exactly on that drifted top. The
    /// next line of stderr therefore continues at the prompt, with the whole
    /// reasoning region erased above it: no gap, nothing lingering.
    #[test]
    fn scrolled_stream_erase_ends_at_drifted_frame_top() {
        let height = 24usize;
        let rows: Vec<String> = (0..13).map(|_| "x".to_string()).collect();
        let (paint, end) = frame_bytes(&rows, 0, Some(height), height);
        assert_eq!(end, Some(24));
        // 12 scrolls shift everything — including the prompt and the frame
        // top — up 12 rows; the frame still fits entirely on screen.
        assert_eq!(paint.matches('\n').count(), 12);
        // Erase from the frame's bottom row (24): 13 clears, 12 ascents →
        // the drifted top row — exactly `height − (rows − 1)`.
        let drifted_top = height - (rows.len() - 1);
        assert_eq!(drifted_top, 12);
        let erase = clear_frame_bytes(13, true);
        // Bottom-up walk: clear current row, ascend, repeat, ending on the
        // drifted top; SHOW restores the caret.
        let expected = format!("\r{CLR_LINE}{UP}").repeat(12) + &format!("\r{CLR_LINE}{SHOW}");
        assert_eq!(erase, expected);
        // The next line of stderr writes at the drifted top — the prompt's
        // new row — with the whole reasoning region erased above it.
        assert_eq!(end.unwrap() - (13 - 1), drifted_top);
    }

    /// The fence state survives window rolls: [`ThinkingView`] tracks it over
    /// the whole stream and reports the state entering the window's first
    /// line, so a code block whose opener scrolled out of the window keeps
    /// its colour (and, crucially, its closer still closes).
    #[test]
    fn thinking_view_fence_state_persists_across_window_rolls() {
        let mut v = ThinkingView::new(3);
        // Stream: opener, 4 code lines, closer, prose — cap 3 keeps only the
        // tail, so the opener is long gone by the time the closer arrives.
        v.push("```rust\n");
        v.push("code 1\ncode 2\ncode 3\ncode 4\n");
        // Window is [code 2, code 3, code 4] — all entered inside the block.
        let (window, in_code) = v.push("");
        assert_eq!(window, vec!["code 2", "code 3", "code 4"]);
        assert!(
            in_code,
            "window starts inside the block, opener already rolled out"
        );
        // The closer arrives; the window now starts even deeper in the block
        // and the closer must still be classified as a closer.
        let (window, in_code) = v.push("```\n");
        assert_eq!(window, vec!["code 3", "code 4", "```"]);
        assert!(in_code);
        // Prose after the closer: the window now starts even deeper in the
        // block — `code 4` entered inside it — so its entering state is still
        // `true`. The closer is IN the window, so a scan from the true state
        // closes the block and the prose classifies as Normal. This is the
        // regression: a per-frame scan starting at `false` would classify the
        // closer as an OPENER (the state resets made it look unopened) and
        // colour the following prose as code.
        let (window, in_code) = v.push("plain prose\n");
        assert_eq!(window, vec!["code 4", "```", "plain prose"]);
        assert!(
            in_code,
            "window starts mid-block (code 4 entered inside it)"
        );
        // Full-stream equivalence: scanning the window from its true entering
        // state yields exactly what a scan of the whole stream would.
        let mut s = in_code;
        let kinds: Vec<LineKind> = window
            .iter()
            .map(|line| {
                let (kind, _, next) = classify_line(line, s);
                s = next;
                kind
            })
            .collect();
        assert_eq!(
            kinds,
            vec![LineKind::Code, LineKind::CodeFence, LineKind::Normal],
            "the closer must close and the prose must not be code"
        );
    }

    /// A partial line being typed inside a code block is classified as code
    /// even before its `\n` arrives: the window's scan reaches it with the
    /// running state (opened by the in-window fence).
    #[test]
    fn thinking_view_partial_enters_at_running_fence_state() {
        let mut v = ThinkingView::new(12);
        v.push("```\n");
        let (window, in_code) = v.push("let x = ");
        assert_eq!(window, vec!["```", "let x = "]);
        // The window starts at the fence line (entering state false); the
        // scan then opens the block, so the partial — the window's last line
        // — classifies as code.
        assert!(!in_code);
        let mut s = in_code;
        let kinds: Vec<LineKind> = window
            .iter()
            .map(|line| {
                let (kind, _, next) = classify_line(line, s);
                s = next;
                kind
            })
            .collect();
        assert_eq!(kinds, vec![LineKind::CodeFence, LineKind::Code]);
    }
}
