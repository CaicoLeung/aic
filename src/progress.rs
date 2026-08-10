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

use console::{Color, Style, Term};
use std::collections::VecDeque;
use std::future::Future;
use std::time::Duration;

use crate::layout::{MARGIN, terminal_height, terminal_width, wrap_line};

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

/// Hard ceiling on the reasoning window. A very tall terminal could otherwise
/// paint dozens of rows, pushing the in-place region toward the bottom edge
/// where further growth would scroll into the scrollback (breaking the discard
/// model — reasoning must never linger). Well past any comfortable reading
/// window, so real terminals are essentially never clamped here in practice.
const MAX_REASONING_ROWS: usize = 40;

/// How many reasoning rows stay visible at once — a *rendered-row* cap, now
/// sized to the space **below the cursor** instead of a fixed 12.
///
/// The old fixed 12 was the root cause of "content scrolls past too fast to
/// read": at 12 rows a line is visible for only ~12 emission-intervals before
/// it rolls off the top of the window. Enlarging the window makes each line
/// linger proportionally longer — the window *is* the "invisible area" content
/// was scrolling out of, so growing it is the direct fix.
///
/// But the window is bounded by a hard geometric constraint: the in-place
/// renderer paints **downward from the cursor**, and cursor-down moves clamp
/// at the bottom margin instead of scrolling. A region taller than the rows
/// left below the cursor therefore **collapses** — several frame rows
/// overwrite the same bottom physical row, so the reasoning is invisible, and
/// the erase at [`ReasoningRenderer::finish`] cannot reclaim what was never
/// cleanly painted. Sizing to the *full* terminal height (`h - 3`) assumed the
/// cursor starts in the top three rows; in reality it sits at the shell prompt
/// — mid-screen or lower — so a full-height window collapsed in the common
/// case. The window therefore queries the cursor's actual row (DSR) and sizes
/// to the rows genuinely available below it — capped at [`MAX_REASONING_ROWS`].
///
/// Two regimes, decided by how much room the cursor actually has below it:
///
/// * **Fit** — the cursor is at least a couple of rows off the bottom
///   (`height - cursor_row >= 2`): the frame (spinner + up to `max_rows`
///   reasoning rows) fits in the rows from the cursor's row to the bottom
///   (`height - cursor_row + 1`), so `max_rows = height - cursor_row` and the
///   screen never moves.
/// * **Scroll** — the cursor sits in the bottom two rows: there is no room to
///   fit a readable window, so the renderer grows the frame past the bottom
///   margin by scrolling the screen (a `\n` at the bottom margin, which the
///   cursor-down clamp would otherwise swallow). The cap is the pre-dynamic
///   fixed budget, clamped so the frame still fits on screen — no reasoning
///   row ever lands in the scrollback. This is the common case: the shell
///   prompt (and therefore the frame's start row) usually sits on the last
///   line of a full screen, and the old fixed-12 window simply collapsed
///   there.
///
/// When the cursor cannot be queried, [`REASONING_FALLBACK_ROWS`] (the
/// pre-dynamic fixed cap) is used and the renderer never scrolls — the
/// user-visible baseline before the sizing regression.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct WindowSizing {
    /// Rendered-row cap for the reasoning window.
    pub(crate) max_rows: usize,
    /// The cursor's 1-based row as reported by the DSR query (`None` when the
    /// query failed) — lets the renderer scroll the frame at the bottom
    /// margin. `None` disables scrolling (legacy behavior).
    pub(crate) cursor_row: Option<usize>,
}

pub(crate) fn reasoning_window_rows() -> WindowSizing {
    let h = terminal_height();
    let cursor_row = query_cursor_row().filter(|r| *r <= h);
    WindowSizing {
        max_rows: reasoning_rows_for(h, cursor_row),
        cursor_row,
    }
}

/// The window cap for a `height`-row terminal with the cursor on 1-based row
/// `cursor_row` (`None` = unknown). See [`reasoning_window_rows`] for the two
/// regimes: with the cursor known and at least two rows of room below it, the
/// frame fits without scrolling (`max_rows = height - cursor_row`); a cursor
/// in the bottom two rows has no room to fit a readable window, so the
/// renderer scrolls instead and the pre-dynamic budget applies (clamped so
/// the frame never exceeds the screen). With the cursor unknown, fall back to
/// the fixed cap that predates the dynamic window — the user-visible baseline
/// before the sizing regression (12 rows was the original [`ThinkingView`]
/// budget).
fn reasoning_rows_for(height: usize, cursor_row: Option<usize>) -> usize {
    match cursor_row.filter(|r| *r <= height) {
        Some(r) => {
            let space = height - r; // rows below the cursor's own row
            if space >= 2 {
                space.min(MAX_REASONING_ROWS)
            } else {
                REASONING_FALLBACK_ROWS.min(height.saturating_sub(1))
            }
        }
        None => REASONING_FALLBACK_ROWS,
    }
}

/// Fallback window cap when the cursor row cannot be queried (stdin or stderr
/// not a terminal, unresponsive terminal, parse failure): the pre-dynamic
/// fixed budget. Matching the old behavior exactly means a failed query is
/// never *worse* than the experience before the dynamic-window feature.
const REASONING_FALLBACK_ROWS: usize = 12;

/// Query the cursor's current 1-based row via DSR (`ESC[6n`), consuming the
/// `ESC[<row>;<col>R` response byte-by-byte from stdin so nothing leaks into
/// later stdin reads (prompts). Returns `None` on any failure: stdin or stderr
/// not a terminal, no response within 200 ms, or an unparseable reply. The
/// terminal is restored to its prior mode on every path.
#[cfg(unix)]
fn query_cursor_row() -> Option<usize> {
    use std::os::fd::AsRawFd;

    let in_fd = std::io::stdin().as_raw_fd();
    if unsafe { libc::isatty(in_fd) } != 1 {
        return None;
    }
    if !Term::stderr().is_term() {
        return None;
    }
    query_cursor_row_core(in_fd)
}

/// The cursor-row query itself, against an already-validated tty `fd` (the
/// caller checks `isatty` and that stderr is a terminal). Split from
/// [`query_cursor_row`] so the full I/O path — pre-check, raw mode, byte
/// loop, late-reply drain — is testable over a PTY without touching the test
/// process's real stdin/stderr.
///
/// Input-safety contract, in order:
///
/// 1. **Never consume input that was pending before the query.** If anything
///    is already buffered (the user started typing, or a previous read leaked
///    bytes), the query is skipped entirely — those bytes are the user's, not
///    ours to eat.
/// 2. **Bail the moment a byte can't continue a DSR reply.** Raw mode lasts
///    only as long as the reply is actually arriving; a keypress (or Ctrl-C)
///    landing mid-window ends the query immediately and restores the
///    terminal, so the raw window is a few milliseconds, not the full
///    deadline — at most one keystroke can be lost, and only in the
///    millisecond race between the pre-check and the reply.
/// 3. **Drain a late reply.** A reply that straggles past the deadline is
///    consumed from stdin (still in raw mode) so it can never surface as
///    garbage in a later prompt read. The drain only commits once a reply
///    actually shows up — bytes are never read speculatively past the first
///    one, which must be ESC to be a reply at all.
#[cfg(unix)]
fn query_cursor_row_core(fd: std::os::fd::RawFd) -> Option<usize> {
    use std::time::Instant;

    // 1. Skip if input is already pending: consumed bytes are lost, so a
    //    quiet queue is a precondition, not an optimization.
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
        return None;
    }
    // The query goes to the same tty the reply comes back on: for a terminal
    // stdin (the only path that reaches here) that is the user's screen.
    let query = b"\x1b[6n";
    if unsafe { libc::write(fd, query.as_ptr().cast(), query.len()) } != query.len() as isize {
        return None;
    }

    // Raw input for the reply: canonical mode would buffer it until a
    // newline the response never carries. Output processing (OPOST) is kept
    // so stderr output stays translated during the brief raw window.
    let restore = unsafe {
        let mut old: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut old) != 0 {
            return None;
        }
        let mut raw = old;
        libc::cfmakeraw(&mut raw);
        raw.c_oflag = old.c_oflag;
        if libc::tcsetattr(fd, libc::TCSANOW, &raw) != 0 {
            return None;
        }
        old
    };
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut resp: Vec<u8> = Vec::with_capacity(16);
    let result = loop {
        if Instant::now() >= deadline {
            break None;
        }
        let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
        if ready <= 0 {
            continue;
        }
        let mut byte = 0u8;
        let n = unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) };
        if n != 1 {
            break None;
        }
        // 2. A byte that can't be part of `ESC[<row>;<col>R` is not the reply
        //    (user input, or garbage): bail immediately instead of waiting
        //    out the deadline in raw mode.
        if !dsr_byte_ok(&resp, byte) {
            break None;
        }
        resp.push(byte);
        if byte == b'R' {
            break parse_dsr(&resp);
        }
        if resp.len() > 32 {
            break None;
        }
    };
    // 3. Late-reply drain, still in raw mode. A reply that straggles past the
    //    deadline would otherwise sit in the kernel queue and surface as
    //    garbage in a later prompt read (canonical mode holds it until a
    //    newline that never comes — the worst kind of leak), so a short grace
    //    waits for its first byte. A partial reply (`resp` holds a real
    //    ESC-led prefix) drains straight to its closing `R`; a query that
    //    never saw its first byte only consumes a byte once one actually
    //    arrives, and only when it is ESC — a non-ESC byte ends the drain
    //    immediately (that one keystroke, typed in the millisecond race
    //    between pre-check and reply, is lost — the alternative, an undrained
    //    reply corrupting the next prompt read, is worse).
    if result.is_none() {
        let drain_deadline = Instant::now() + DRAIN_GRACE;
        if resp.is_empty() {
            while Instant::now() < drain_deadline {
                if unsafe { libc::poll(&mut pfd, 1, 10) } > 0 {
                    let mut first = 0u8;
                    if unsafe { libc::read(fd, (&mut first as *mut u8).cast(), 1) } == 1
                        && first == b'\x1b'
                    {
                        drain_dsr_reply(fd);
                    }
                    break;
                }
            }
        } else {
            drain_dsr_reply(fd);
        }
    }
    unsafe {
        libc::tcsetattr(fd, libc::TCSANOW, &restore);
    }
    result
}

/// How long a reply that missed the query deadline is still waited for, so a
/// slow terminal's answer cannot leak into later stdin reads. Covers reply
/// latency up to 200 ms past the deadline (i.e. ~400 ms end to end) — well
/// past any link RTT that would make the reply genuinely late.
const DRAIN_GRACE: Duration = Duration::from_millis(200);

/// Whether `b` can continue a DSR reply already accumulated in `resp`
/// (`ESC[<row>;<col>R`): the first byte must be ESC, the second `[`, then
/// digits / `;` until the closing `R` (the loop stops there, so `R` only
/// passes while empty-accumulated checks for the leading bytes apply).
fn dsr_byte_ok(resp: &[u8], b: u8) -> bool {
    match resp.len() {
        0 => b == b'\x1b',
        1 => b == b'[',
        _ => b.is_ascii_digit() || b == b';' || b == b'R',
    }
}

/// Consume the remainder of a DSR reply from `fd` (still in raw mode) so it
/// cannot leak into later stdin reads: reads until the closing `R`, a reply-
/// shaped byte budget, or a short grace — whichever first. Only called once
/// the reply is known to be in flight (an ESC-led prefix was seen).
#[cfg(unix)]
fn drain_dsr_reply(fd: std::os::fd::RawFd) {
    use std::time::Instant;

    let deadline = Instant::now() + Duration::from_millis(200);
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let mut n = 0usize;
    while Instant::now() < deadline && n < 32 {
        if unsafe { libc::poll(&mut pfd, 1, 10) } <= 0 {
            break;
        }
        let mut byte = 0u8;
        if unsafe { libc::read(fd, (&mut byte as *mut u8).cast(), 1) } != 1 {
            break;
        }
        n += 1;
        if byte == b'R' {
            break;
        }
    }
}

#[cfg(not(unix))]
fn query_cursor_row() -> Option<usize> {
    None
}

/// Parse a DSR response `ESC[<row>;<col>R` into the 1-based row. Pure (no
/// I/O) so the parse is unit-testable without a terminal.
fn parse_dsr(resp: &[u8]) -> Option<usize> {
    let s = std::str::from_utf8(resp).ok()?;
    let body = s.strip_prefix("\x1b[")?.strip_suffix('R')?;
    let row = body.split(';').next()?.parse::<usize>().ok()?;
    (row > 0).then_some(row)
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

/// Markdown render kind for one reasoning line. Streaming markdown can't be
/// fully parsed (the input is partial), so classification is line-local with a
/// running fence state — robust where it matters (headings, fenced code
/// blocks). The state is carried across the whole stream by
/// [`ThinkingView`], so a code block whose opener has already scrolled out of
/// the window still colours its content correctly, and a closer after a long
/// block still closes it. Inline `**bold**` / `` `code` `` are deliberately
/// not parsed: they can split across wrap boundaries, and the explicit ask was
/// line-level "bold titles" and "code blocks", both of which are wrap-safe.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum LineKind {
    /// Plain reasoning prose — no styling.
    Normal,
    /// An ATX heading (`#`..`######` + space) — the "bold titles". Rendered
    /// bold, with the leading `#` markers stripped.
    Heading,
    /// A line inside a fenced code block — the "code blocks". Rendered in a
    /// distinct colour so code reads as code, like the coding-agent UIs aic
    /// mirrors.
    Code,
    /// A ``` / ~~~ fence line itself — rendered dim so it reads as a delimiter
    /// rather than content.
    CodeFence,
}

/// Whether `s` is an ATX heading opener: one to six `#`, then end-of-line or
/// a space (CommonMark — `#` alone is a heading). The space requirement (or
/// end of line) keeps `#include`, `#!/bin/bash`, and `#tag` out of the
/// heading kind — only `# Title`-shaped lines qualify.
fn is_atx_heading(s: &str) -> bool {
    let hashes = s.bytes().take_while(|&b| b == b'#').count();
    matches!(hashes, 1..=6) && matches!(s.as_bytes().get(hashes), None | Some(&b' '))
}

/// Classify one reasoning line for rendering, given whether the running scan
/// is already inside a fenced code block. Returns the line's kind, the text to
/// wrap (headings have their `#` markers stripped), and the fence state to
/// carry into the next line. Pure (no I/O) so classification is unit-testable
/// independently of the ANSI styling.
fn classify_line(line: &str, in_code: bool) -> (LineKind, String, bool) {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        (LineKind::CodeFence, trimmed.to_string(), !in_code)
    } else if in_code {
        (LineKind::Code, line.to_string(), in_code)
    } else if is_atx_heading(trimmed) {
        let body = trimmed.trim_start_matches('#').trim_start();
        (LineKind::Heading, body.to_string(), in_code)
    } else {
        (LineKind::Normal, line.to_string(), in_code)
    }
}

/// Apply the kind's ANSI style to an already-wrapped piece. `Normal` is a
/// no-op (plain text); the others go through `console`, which strips the ANSI
/// on a non-TTY so piped output stays clean. Styling is applied *after* wrap
/// so the width math in [`crate::layout::wrap_line`] never counts escape bytes.
fn style_kind(piece: &str, kind: LineKind) -> String {
    let style = match kind {
        LineKind::Normal => return piece.to_string(),
        LineKind::Heading => Style::new().bold(),
        LineKind::Code => Style::new().fg(Color::Cyan),
        LineKind::CodeFence => Style::new().dim(),
    };
    style.apply_to(piece).to_string()
}

/// Build the visual rows for one reasoning frame: the spinner+label row on
/// top, then each retained reasoning line greedy-wrapped under the shared
/// `│ ` indent and styled by its markdown kind (bold headings, coloured code
/// blocks). Pure (no I/O) so the layout is unit-testable; the renderer paints
/// exactly what this returns.
///
/// `feed_width` is the per-piece wrap budget. `max_rows` is the rendered-row
/// cap (from [`reasoning_window_rows`]); the newest `max_rows` rows below the
/// spinner are kept and the oldest drop out — a long line that wraps to
/// several rows still cannot grow the region, so the top row may start
/// mid-line, like a terminal tail window. An empty `window` yields just the
/// spinner row. `in_code_start` is the markdown fence state entering the
/// window's first line, as tracked by [`ThinkingView`] over the whole stream
/// — never assumed `false` here, or a code block whose opener scrolled out of
/// the window would classify its closer as an opener and colour the following
/// prose as code.
fn reasoning_rows(
    glyph: &str,
    label: &str,
    window: &[String],
    feed_width: usize,
    elapsed: Option<Duration>,
    max_rows: usize,
    in_code_start: bool,
) -> Vec<String> {
    let mut rows = Vec::with_capacity(window.len() + 1);
    // Spinner row: when `elapsed` is `Some` (the live feed is ticking), append
    // a rising seconds count so the wait is visible even while the model is
    // silent between deltas — the same feedback the loading frame gives, kept
    // through the transition into reasoning so the user never loses the clock.
    // `None` is the tests' stable snapshot (no clock).
    let spinner = match elapsed {
        Some(e) => format!("{MARGIN}{glyph} {label}… {}s", e.as_secs()),
        None => format!("{MARGIN}{glyph} {label}"),
    };
    rows.push(spinner);
    // Running fence state, carried from the stream (see the doc) and advanced
    // line-by-line across the window.
    let mut in_code = in_code_start;
    for line in window {
        let (kind, display, next) = classify_line(line, in_code);
        in_code = next;
        for piece in wrap_line(&display, feed_width) {
            rows.push(format!("{MARGIN}│ {}", style_kind(&piece, kind)));
        }
    }
    // The load-bearing rendered-row cap: the line window alone can't bound the
    // on-screen region, because one long line wraps to many rows. Keep the
    // newest `max_rows` rows below the spinner row.
    let start = 1 + (rows.len() - 1).saturating_sub(max_rows);
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

impl ReasoningRenderer {
    /// Bind a renderer to stderr with `label` on the spinner row. `max_rows`
    /// is the reasoning window's rendered-row cap and `cursor_row` the
    /// DSR-reported cursor row (`None` if the query failed — scrolling is
    /// then disabled), both from [`reasoning_window_rows`]. `feed_width` is
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
        }
    }

    /// Paint one frame for the reasoning `window`. Safe to call on every
    /// delta AND on every idle tick — the latter via `Some(elapsed)` keeps the
    /// spinner animating and the elapsed count rising while the model is
    /// silent between deltas, so the reasoning frame never freezes the way a
    /// paint-only-on-delta renderer would during a long TTFT gap. A no-op off
    /// a terminal. `in_code_start` is the markdown fence state entering the
    /// window's first line, as tracked by [`ThinkingView`].
    pub(crate) fn paint(
        &mut self,
        window: &[String],
        in_code_start: bool,
        elapsed: Option<Duration>,
    ) {
        if !self.term.is_term() {
            return;
        }
        let glyph = REASONING_GLYPHS[self.glyph % REASONING_GLYPHS.len()];
        self.glyph = self.glyph.wrapping_add(1);
        let rows = reasoning_rows(
            glyph,
            self.label,
            window,
            self.feed_width,
            elapsed,
            self.max_rows,
            in_code_start,
        );
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
        let (bytes, end_row) = frame_bytes(rows, self.prev_height, self.row, self.height);
        let _ = self.term.write_str(&bytes);
        let _ = self.term.flush();
        self.prev_height = rows.len();
        // The cursor now rests on the frame's bottom row; that absolute row is
        // the next paint's start (frames scroll at the bottom margin, so the
        // top drifts and cannot be derived from the row count alone).
        self.row = end_row;
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

    #[test]
    fn parse_dsr_extracts_1based_row() {
        assert_eq!(parse_dsr(b"\x1b[12;1R"), Some(12));
        assert_eq!(parse_dsr(b"\x1b[1;80R"), Some(1));
        assert_eq!(parse_dsr(b"\x1b[40;1R"), Some(40));
    }

    #[test]
    fn parse_dsr_rejects_garbage() {
        assert_eq!(parse_dsr(b"nope"), None);
        assert_eq!(parse_dsr(b"\x1b[R"), None);
        assert_eq!(parse_dsr(b"\x1b[0;1R"), None); // row 0 is never valid
        assert_eq!(parse_dsr(b""), None);
    }

    #[test]
    fn window_sizes_to_space_below_cursor() {
        // 24-row terminal, cursor on row 21 (near the bottom): the frame
        // (spinner + window) must fit rows 21..24 — a 3-row window, never a
        // collapsing full-height one.
        assert_eq!(reasoning_rows_for(24, Some(21)), 3);
        // Cursor at the very top: the full height is available, capped at MAX.
        assert_eq!(reasoning_rows_for(24, Some(1)), 23);
        // Mid-screen cursor on a 40-row terminal: exact-fit case.
        assert_eq!(reasoning_rows_for(40, Some(3)), 37);
        // Cursor in the bottom two rows has no room to fit a readable window:
        // the renderer scrolls instead, and the pre-dynamic budget applies
        // (clamped so the frame still fits on screen — 12 < 40).
        assert_eq!(reasoning_rows_for(40, Some(40)), 12);
        assert_eq!(reasoning_rows_for(40, Some(39)), 12);
        // Two rows of room below the cursor: exact fit again (no scrolling).
        assert_eq!(reasoning_rows_for(40, Some(38)), 2);
        // A tiny terminal clamps the scroll budget so the frame never exceeds
        // the screen: 1 spinner + 4 rows on a 5-row terminal.
        assert_eq!(reasoning_rows_for(5, Some(5)), 4);
        // Unknown cursor: the pre-dynamic fixed cap (never worse than before
        // the dynamic-window feature).
        assert_eq!(reasoning_rows_for(24, None), REASONING_FALLBACK_ROWS);
        // A bogus cursor row past the terminal height falls back too.
        assert_eq!(reasoning_rows_for(24, Some(99)), REASONING_FALLBACK_ROWS);
    }

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
            false,
        );
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠙ Analyzing… 7s")));
        assert_eq!(rows[1], format!("{MARGIN}│ a line"));
    }

    /// [`reasoning_rows`] always leads with the spinner+label row, even when
    /// the window is empty (the stream just started).
    #[test]
    fn reasoning_rows_leads_with_spinner_for_empty_window() {
        let rows = reasoning_rows("⠋", "Analyzing", &[], 80, None, 12, false);
        assert_eq!(rows, vec![format!("{MARGIN}⠋ Analyzing")]);
    }

    /// Each retained line becomes one indented row when it fits the budget.
    #[test]
    fn reasoning_rows_indents_each_line() {
        let window = vec!["line 1".to_string(), "line 2".to_string()];
        let rows = reasoning_rows("⠙", "Analyzing", &window, 80, None, 12, false);
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
            false,
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
        let rows = reasoning_rows("⠹", "Analyzing", &window, 10, None, 12, false);
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
        let rows = reasoning_rows("⠹", "Analyzing", &[long], 10, None, 12, false);
        assert_eq!(rows.len(), 13);
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
        // 12 rows of "word word" = 24 words of the 60 — the newest tail only.
        let words: usize = rows[1..]
            .iter()
            .map(|r| r.strip_prefix(&prefix).unwrap().split_whitespace().count())
            .sum();
        assert_eq!(words, 24);
    }

    /// Plain prose classifies as Normal, leaving the text untouched.
    #[test]
    fn classify_plain_prose_is_normal() {
        let (kind, text, _) = classify_line("just thinking out loud", false);
        assert_eq!(kind, LineKind::Normal);
        assert_eq!(text, "just thinking out loud");
    }

    /// An ATX heading (`#` + space, or bare hashes at end of line) classifies
    /// as Heading with the markers stripped — the "bold titles". The space or
    /// end-of-line requirement keeps `#include`, shebangs, and `#tag` out of
    /// the heading kind.
    #[test]
    fn classify_atx_heading_strips_markers() {
        let (kind, text, _) = classify_line("## Plan", false);
        assert_eq!(kind, LineKind::Heading);
        assert_eq!(text, "Plan");
        // CommonMark: `#` alone at end of line is still a heading.
        assert_eq!(classify_line("#", false).0, LineKind::Heading);
        assert_eq!(classify_line("######", false).0, LineKind::Heading);
        // No space and content follows: not a heading.
        assert_eq!(
            classify_line("#include <stdio.h>", false).0,
            LineKind::Normal
        );
        assert_eq!(classify_line("#!/bin/bash", false).0, LineKind::Normal);
        assert_eq!(classify_line("#tag", false).0, LineKind::Normal);
        // Seven hashes: past the ATX limit, not a heading.
        assert_eq!(classify_line("####### deep", false).0, LineKind::Normal);
    }

    /// A fenced code block toggles Code state across its lines: the fence line
    /// is CodeFence, content lines are Code, and state closes on the matching
    /// fence. Both ``` and ~~~ open/close.
    #[test]
    fn classify_fence_toggles_code_state() {
        let (k0, _, s0) = classify_line("```rust", false);
        assert_eq!(k0, LineKind::CodeFence);
        assert!(s0);
        let (k1, _, s1) = classify_line("let x = 1;", s0);
        assert_eq!(k1, LineKind::Code);
        assert!(s1);
        let (k2, _, s2) = classify_line("```", s1);
        assert_eq!(k2, LineKind::CodeFence);
        assert!(!s2);
        // Back to Normal after the closer; tilde fences open too.
        assert_eq!(classify_line("done", s2).0, LineKind::Normal);
        assert!(classify_line("~~~", false).2);
    }

    /// An unclosed fence (stream ended mid-block) leaves subsequent lines as
    /// Code — matching "render as code" for a half-written block.
    #[test]
    fn classify_unclosed_fence_keeps_code() {
        let (_, _, in_code) = classify_line("```python", false);
        assert_eq!(classify_line("def f():", in_code).0, LineKind::Code);
    }

    /// [`reasoning_rows`] classifies markdown lines and still honours the
    /// rendered-row cap + spinner row. Asserts only length and the spinner
    /// (ANSI is environment-dependent on the test process); kind correctness
    /// lives in the classify_* tests above.
    #[test]
    fn reasoning_rows_handles_markdown_lines_and_caps() {
        let window = vec![
            "# Heading".to_string(),
            "```rs".to_string(),
            "let x = 2;".to_string(),
            "```".to_string(),
        ];
        let rows = reasoning_rows("⠹", "Analyzing", &window, 80, None, 2, false);
        assert_eq!(rows.len(), 3); // spinner + newest 2 rows
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
    }

    /// The styling path is live: a bold style actually emits ANSI when forced
    /// (in the real run `paint`'s `is_term` guard is what gates emission; this
    /// forces it so the test process — not a TTY — still observes the escape,
    /// guarding against style_kind ever silently becoming a no-op).
    #[test]
    fn styling_path_emits_ansi_when_forced() {
        let s = Style::new().bold().force_styling(true);
        let styled = s.apply_to("hi").to_string();
        assert!(
            styled.contains("\x1b["),
            "no ANSI escape emitted: {styled:?}"
        );
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

    /// [`reasoning_rows`] honours the stream's fence state entering the
    /// window: mid-block content stays Code and a closer closes — the
    /// classification a full-stream scan would give, regardless of how much
    /// of the block scrolled out of the window.
    #[test]
    fn reasoning_rows_classifies_with_running_fence_state() {
        let prefix = format!("{MARGIN}│ ");
        // Window whose opener is gone: [code, closer, prose] with entering
        // state `true`. The closer must close; prose must be Normal.
        let window = vec![
            "let x = 1;".to_string(),
            "```".to_string(),
            "done".to_string(),
        ];
        let rows = reasoning_rows("⠹", "Analyzing", &window, 80, None, 12, true);
        assert_eq!(rows.first(), Some(&format!("{MARGIN}⠹ Analyzing")));
        let body: Vec<&str> = rows[1..]
            .iter()
            .map(|r| r.strip_prefix(&prefix).unwrap())
            .collect();
        assert_eq!(body, vec!["let x = 1;", "```", "done"]);
        // And a window that starts mid-block with `false` (no opener at all,
        // e.g. the stream genuinely began outside a block) classifies the
        // first fence as an opener.
        let rows = reasoning_rows("⠹", "Analyzing", &window, 80, None, 12, false);
        assert_eq!(rows.len(), 4, "spinner + 3 rows survive the cap");
    }
}

/// Full-I/O tests for [`query_cursor_row_core`] over a real PTY: a forked
/// child plays the terminal emulator on the pty master (reads the DSR query,
/// answers per script), while the parent runs the query against the slave.
/// This is the committed version of the PR's "deterministic PTY harness"
/// claim — the input-safety contract (never consume pending input, never
/// leak a late reply) is exercised end to end, not just the pure parse.
#[cfg(all(unix, test))]
mod pty_tests {
    use super::*;
    use std::os::fd::RawFd;
    use std::time::{Duration, Instant};

    /// What the fake terminal does on the pty master.
    #[derive(Clone, Copy)]
    enum Script {
        /// Wait for the query, then reply immediately.
        Reply(&'static [u8]),
        /// Wait for the query, sleep `ms`, then reply (a late reply).
        ReplyLate(&'static [u8], u32),
        /// Wait for the query, then stay silent (no reply ever).
        Silent,
        /// Write bytes immediately without waiting for any query.
        PreWrite(&'static [u8]),
    }

    fn open_pty() -> (RawFd, RawFd) {
        let mut master = 0;
        let mut slave = 0;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty failed");
        (master, slave)
    }

    /// Child-side terminal emulator. Runs after `fork`, so it must not touch
    /// the allocator: fixed stack buffer, libc calls only.
    fn terminal_script(master: RawFd, script: Script) -> i32 {
        if let Script::PreWrite(bytes) = script {
            let n = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
            return if n == bytes.len() as isize { 0 } else { 1 };
        }
        // Wait for the DSR query (`ESC[6n`) to arrive on the master.
        let query = b"\x1b[6n";
        let mut buf = [0u8; 64];
        let mut len = 0usize;
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut pfd = libc::pollfd {
            fd: master,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            if Instant::now() >= deadline {
                return 2; // query never arrived
            }
            let ready = unsafe { libc::poll(&mut pfd, 1, 50) };
            if ready <= 0 {
                continue;
            }
            let n = unsafe { libc::read(master, buf[len..].as_mut_ptr().cast(), buf.len() - len) };
            if n <= 0 {
                return 3;
            }
            len += n as usize;
            if len >= query.len() && buf[..len].windows(query.len()).any(|w| w == query) {
                break;
            }
            if len == buf.len() {
                return 4; // buffer full without ever seeing the query
            }
        }
        match script {
            Script::Silent => 0,
            Script::ReplyLate(bytes, ms) => {
                unsafe { libc::usleep(ms * 1000) };
                let n = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
                if n == bytes.len() as isize { 0 } else { 5 }
            }
            Script::Reply(bytes) => {
                let n = unsafe { libc::write(master, bytes.as_ptr().cast(), bytes.len()) };
                if n == bytes.len() as isize { 0 } else { 5 }
            }
            Script::PreWrite(_) => 0,
        }
    }

    /// Put `fd` in raw (non-canonical) mode, like the query does — without
    /// this, a pty slave buffers input until a newline (canonical mode), so a
    /// single byte like the PreWrite script's `x` would never make poll
    /// report it readable.
    fn make_raw(fd: RawFd) {
        unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            assert_eq!(libc::tcgetattr(fd, &mut t), 0, "tcgetattr failed");
            libc::cfmakeraw(&mut t);
            assert_eq!(
                libc::tcsetattr(fd, libc::TCSANOW, &t),
                0,
                "tcsetattr failed"
            );
        }
    }

    /// Fork a fake terminal running `script`; returns `(master, slave, pid)`
    /// — the parent keeps the master open for the test's lifetime so the
    /// slave never sees EOF/HUP (a closed master would fake "input pending"
    /// on every later poll). Both fds are closed by the test; `pid` is
    /// reaped with [`reap`].
    fn spawn_terminal(script: Script) -> (RawFd, RawFd, i32) {
        let (master, slave) = open_pty();
        make_raw(slave);
        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe { libc::close(slave) };
            let code = terminal_script(master, script);
            unsafe { libc::_exit(code) };
        }
        (master, slave, pid)
    }

    fn reap(pid: i32) -> i32 {
        let mut status = 0;
        unsafe { libc::waitpid(pid, &mut status, 0) };
        status
    }

    fn poll_ready(fd: RawFd, timeout_ms: u32) -> bool {
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
        unsafe { libc::poll(&mut pfd, 1, timeout_ms as libc::c_int) > 0 }
    }

    /// The full DSR round trip over a real PTY: the fake terminal sees the
    /// query and answers `ESC[21;1R`; the query reports row 21.
    #[test]
    fn dsr_query_roundtrip_over_pty() {
        let (master, slave, pid) = spawn_terminal(Script::Reply(b"\x1b[21;1R"));
        let row = query_cursor_row_core(slave);
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
        assert_eq!(row, Some(21));
    }

    /// A garbage reply (not ESC-led) bails the byte loop on its first byte —
    /// the query fails fast instead of waiting out the 200 ms deadline.
    #[test]
    fn dsr_rejects_garbage_reply() {
        let (master, slave, pid) = spawn_terminal(Script::Reply(b"hello"));
        let row = query_cursor_row_core(slave);
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
        assert_eq!(row, None);
    }

    /// No reply at all: the query times out and returns `None` (the fallback
    /// path) instead of hanging.
    #[test]
    fn dsr_times_out_without_reply() {
        let (master, slave, pid) = spawn_terminal(Script::Silent);
        let row = query_cursor_row_core(slave);
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
        assert_eq!(row, None);
    }

    /// A reply that arrives after the 200 ms deadline (but inside the drain
    /// grace) is consumed, so it can never sit in the input queue and surface
    /// as garbage in a later prompt read.
    #[test]
    fn dsr_drains_late_reply_so_nothing_leaks() {
        let (master, slave, pid) = spawn_terminal(Script::ReplyLate(b"\x1b[40;1R", 300));
        let row = query_cursor_row_core(slave);
        assert_eq!(row, None, "reply past the deadline must not be reported");
        // Give the drain grace time to finish, then the queue must be empty.
        std::thread::sleep(Duration::from_millis(500));
        assert!(
            !poll_ready(slave, 0),
            "late reply leaked into the input queue — it would corrupt a later prompt read"
        );
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
    }

    /// Input already pending when the query starts is never consumed: the
    /// pre-check skips the query entirely and the byte survives for the next
    /// read.
    #[test]
    fn dsr_skips_query_when_input_pending() {
        let (master, slave, pid) = spawn_terminal(Script::PreWrite(b"x"));
        assert!(poll_ready(slave, 2000), "child never wrote its input");
        let row = query_cursor_row_core(slave);
        assert_eq!(row, None, "a busy stdin must not be queried");
        // The byte is still pending — the pre-check did not consume it.
        assert!(poll_ready(slave, 2000), "pending input was swallowed");
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
        assert_eq!(reap(pid), 0);
    }
}
