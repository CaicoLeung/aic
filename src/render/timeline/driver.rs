//! The Run timeline frame driver (ADR 0018): the stateful half that sits
//! on top of the pure [`timeline`](super) builders.
//!
//! One [`RunFrame`] owns the whole visible stack — the Step rows plus the
//! active step's live output — and repaints it as the bottom-most region via
//! the generalized [`frame_bytes`](crate::render::progress) painter: a 50 ms
//! tick while any step is active, plus an immediate repaint on every state
//! change (ADR 0018). Completed step lines are immutable, so rows that scroll
//! off the top into scrollback freeze permanently — correct, because they
//! never change again. The final frame stays in the scrollback; nothing erases
//! at the end of the Run (or on failure).
//!
//! Off a terminal (piped stderr, tests) there is **no frame**: the driver
//! still tracks step state — sub-row accounting, durations, menu-time
//! exclusion — but its only output is the non-TTY *ledger* (ADR 0018): each
//! step appends its plain `✓` summary (or `✗` headline + details) through the
//! Run's [`Display`] as it completes. Panels (Commit entries, warnings) print
//! via their owners exactly as before, which is what the e2e Buffer harness
//! sees.
//!
//! Step-scoped output routing: [`RunFrame::step_display`] hands out a
//! [`Display`] whose sink is a [`FrameSink`] on a terminal — every line a
//! panel emits (a preview, a warning) is appended to the frame's rows
//! instead of printed, so nothing can interleave below the in-place region
//! — or the Run's own shared sink off a terminal, so non-TTY behavior is
//! byte-identical to the pre-timeline output. Rows emitted while a step is
//! active ride its live output (ephemeral) and become its durable rows when
//! the step completes; rows emitted after completion land directly under
//! that step's summary. Unbounded durable content (Commit entries) must NOT
//! ride the frame — a frame taller than the terminal cannot be repainted —
//! so it lands via [`RunFrame::flush`] as permanent scrollback: erased
//! frame, printed rows, frame repainted below (the bounded-frame model).

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use console::Term;

use crate::render::cursor::{self, WindowSizing};
use crate::render::display::{Display, DisplayWrite};
use crate::render::layout::terminal_width;
use crate::render::progress::{SPINNER_TICK, clear_frame_bytes, frame_bytes};
use crate::render::timeline::note_row;
use crate::render::timeline::{
    Failure, Step, StepState, SubBody, Theme, menu_rows, plain_step_lines, sub_row, timeline_frame,
    window_height_budget,
};

/// The process-global frame slot. The production interactive menus (commit
/// confirmation, Batch Review) render as frame rows and read raw keys, but
/// their seam closures are built before the Run knows its step skeleton —
/// they pick the frame up here at call time instead of threading it through
/// every seam signature (which would break the e2e stub factories).
// ponytail: process-global slot — one run per process; if aic ever runs
// nested runs, thread the frame through RunDeps instead.
static ACTIVE_FRAME: Mutex<Option<RunFrame>> = Mutex::new(None);

/// The frame the production menus render into, if a Run is on screen.
pub(crate) fn active_frame() -> Option<RunFrame> {
    ACTIVE_FRAME.lock().unwrap().clone()
}

/// Install `frame` as the process's active frame, returning a guard that
/// uninstalls it on drop (the Run owns the guard for its whole body). A
/// previously-installed frame is restored on drop, so a nested install
/// leaves the outer frame intact when the inner one ends.
pub(crate) fn install(frame: RunFrame) -> FrameGuard {
    let prev = (*ACTIVE_FRAME.lock().unwrap()).replace(frame);
    FrameGuard(prev)
}

/// Uninstalls the active frame on drop, restoring whatever it replaced.
pub struct FrameGuard(Option<RunFrame>);

impl Drop for FrameGuard {
    fn drop(&mut self) {
        *ACTIVE_FRAME.lock().unwrap() = self.0.take();
    }
}

/// Braille frames for the active step marker (ADR 0018) — the same 10-glyph
/// set the reasoning spinner uses, advanced once per tick.
const GLYPHS: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The whole frame state behind the cheap [`RunFrame`] handle.
struct FrameState {
    /// The step rows (Stage-1 [`Step`] model, one per skeleton entry).
    steps: Vec<Step>,
    /// Per-step start instant (set on activate) for the live elapsed counter
    /// and the completion duration.
    starts: Vec<Option<Instant>>,
    /// Per-step draft sub-row slots (ADR 0018): `subs[step][i-1]` holds
    /// sub-row `i` of `total`, `None` until the draft starts. The active
    /// step's live rows are the compacted non-`None` slots, so sub-rows are
    /// revealed as drafts start and all vanish together at completion.
    subs: Vec<Vec<Option<String>>>,
    /// When a menu is open, the clock stops (ADR 0018: durations cover LLM /
    /// git work only, not wall time spent sitting at a prompt).
    menu_pause: Option<Instant>,
    /// Menu wall time already excluded from the active step's elapsed.
    menu_total: Duration,
    /// Spinner tick counter.
    glyph: usize,
    /// The painter; `None` off a terminal (no frame, ledger only).
    paint: Option<Painter>,
}

/// The terminal-side painter: whole-frame [`frame_bytes`] repaints to stderr.
struct Painter {
    term: Term,
    /// Frame row width (display columns) for the pure builders.
    width: usize,
    /// The terminal's raw column count, uncapped — [`Self::width`] carries
    /// the 100-column prose cap for frame rows, while flushed table rows
    /// ([`RunFrame::flush`]) budget to the real edge (ADR 0018 redesign).
    raw_width: usize,
    /// Terminal height, for the bottom-margin scroll path.
    height: usize,
    /// The DSR-probed absolute cursor row at frame start (`None` → the
    /// legacy no-scroll fallback).
    row: Option<usize>,
    /// Rows the previous paint left on screen.
    prev_rows: Vec<String>,
    /// Whether the cursor is currently hidden (owed a SHOW).
    cursor_hidden: bool,
    /// Cached color capability (colors don't change mid-run).
    colors: bool,
    /// The DSR probe's rendered-row cap for the reasoning window.
    max_window_rows: usize,
}

impl Painter {
    fn prev_height(&self) -> usize {
        self.prev_rows.len()
    }
}

/// A handle to the Run's timeline frame. Cheap to clone; all clones share one
/// [`FrameState`]. Every method is a lock-mutate-(repaint) step, safe to call
/// from futures, the repaint ticker, and the (blocking) menu key loop.
#[derive(Clone)]
pub struct RunFrame(Arc<Mutex<FrameState>>);

impl RunFrame {
    /// Build the frame for a step skeleton (labels in display order). On a
    /// terminal this probes the cursor row (DSR — the same probe the reasoning
    /// window uses) on the blocking pool and starts the 50 ms repaint ticker;
    /// off a terminal the frame is bookkeeping only.
    pub async fn new(labels: Vec<String>) -> Self {
        let term = Term::stderr();
        let paint = if term.is_term() {
            // The DSR query does blocking tty I/O — keep it off the reactor
            // (same contract as `run_with_reasoning_feed`). A panic degrades
            // to the no-scroll fallback; decoration never breaks the commit.
            let sizing = tokio::task::spawn_blocking(cursor::reasoning_window_rows)
                .await
                .unwrap_or_else(|_| WindowSizing::fallback());
            let raw_width = term.size().1 as usize;
            Some(Painter {
                term,
                width: terminal_width(),
                raw_width,
                height: crate::render::layout::terminal_height(),
                row: sizing.cursor_row,
                prev_rows: Vec::new(),
                cursor_hidden: false,
                colors: console::colors_enabled_stderr(),
                max_window_rows: sizing.max_rows,
            })
        } else {
            None
        };
        let n = labels.len();
        let this = Self(Arc::new(Mutex::new(FrameState {
            steps: labels.into_iter().map(Step::pending).collect(),
            starts: vec![None; n],
            subs: vec![Vec::new(); n],
            menu_pause: None,
            menu_total: Duration::ZERO,
            glyph: 0,
            paint,
        })));
        if this.0.lock().unwrap().paint.is_some() {
            this.spawn_ticker();
        }
        this
    }

    fn spawn_ticker(&self) {
        let this = Self(Arc::clone(&self.0));
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(SPINNER_TICK).await;
                let mut st = this.0.lock().unwrap();
                // The ticker lives exactly as long as some step is active —
                // completion or failure of the last active step ends it, so
                // the final frame simply stops repainting and stays.
                if !st.steps.iter().any(|s| s.state == StepState::Active) {
                    break;
                }
                st.glyph = st.glyph.wrapping_add(1);
                st.repaint();
            }
        });
    }

    /// Whether the frame paints (stderr is a terminal). Off a terminal the
    /// driver emits only the plain non-TTY ledger.
    pub fn is_tty(&self) -> bool {
        self.0.lock().unwrap().paint.is_some()
    }

    /// The frame's row width (the pure builders' budget).
    pub fn width(&self) -> usize {
        let st = self.0.lock().unwrap();
        st.paint.as_ref().map_or_else(terminal_width, |p| p.width)
    }

    /// The frame's styling policy.
    pub fn theme(&self) -> Theme {
        Theme {
            colors: self
                .0
                .lock()
                .unwrap()
                .paint
                .as_ref()
                .is_some_and(|p| p.colors),
        }
    }

    /// Flip step `i` to active and start its clock.
    pub fn activate(&self, i: usize) {
        let mut st = self.0.lock().unwrap();
        st.starts[i] = Some(Instant::now());
        st.menu_total = Duration::ZERO;
        st.menu_pause = None;
        st.steps[i].state = StepState::Active;
        st.repaint();
    }

    /// Replace the (active) step's label — the live commit cursor
    /// `⠋ Committing batch 2 of 3` (spec §2.7).
    pub fn set_label(&self, i: usize, label: impl Into<String>) {
        let mut st = self.0.lock().unwrap();
        st.steps[i].label = label.into();
        st.repaint();
    }

    /// Complete step `i` with `summary`: its live output (sub-rows, preview,
    /// reasoning window, already-landed entries) collapses to the `✓` line in
    /// one frame — except rows emitted while it was active, which persist as
    /// its durable block (spec §2.1). The plain ledger line is emitted ONLY
    /// off a terminal: on a TTY the frame already shows the summary in place,
    /// and printing it again below the frame would both duplicate the line
    /// and push the cursor out from under the painter's `prev_rows` geometry,
    /// desyncing every later whole-frame repaint.
    pub fn complete(&self, display: &Display, i: usize, summary: impl Into<String>) {
        let (lines, ledger) = {
            let mut st = self.0.lock().unwrap();
            let duration = st.step_elapsed(i);
            // A step with draft sub-rows owns its live rows wholly (the
            // compacted sub-row slots) — they erase; any other live rows
            // (Commit entries, warnings emitted while active) are the
            // durable record (spec §2.1/§2.7).
            let has_subs = !st.subs[i].is_empty();
            st.subs[i].clear();
            let step = &mut st.steps[i];
            step.state = StepState::Completed;
            step.summary = Some(summary.into());
            step.duration = Some(duration);
            step.active_elapsed = None;
            let live = std::mem::take(&mut step.live_rows);
            if !live.is_empty() && !has_subs {
                step.durable_rows = live;
            }
            st.repaint();
            (
                plain_step_lines(&st.steps[i]),
                st.paint.is_none(), // ledger only off a terminal
            )
        };
        if ledger {
            for line in lines {
                display.emit(&line);
            }
        }
    }

    /// Fail step `i`: `✗` headline + detail rows persist; every later step
    /// is absent from the frame (spec §2.9). Off a terminal the headline and
    /// details append to the ledger; on a terminal the frame shows them in
    /// place (a second emit below the frame would duplicate the block and
    /// desync the repaint geometry — see [`Self::complete`]). Nothing erases
    /// on failure.
    pub fn fail(&self, display: &Display, i: usize, failure: Failure) {
        let (lines, ledger) = {
            let mut st = self.0.lock().unwrap();
            st.steps[i].state = StepState::Failed;
            st.steps[i].failure = Some(failure);
            st.steps[i].live_rows.clear();
            st.steps[i].active_elapsed = None;
            st.repaint();
            (
                plain_step_lines(&st.steps[i]),
                st.paint.is_none(), // ledger only off a terminal
            )
        };
        if ledger {
            for line in lines {
                display.emit(&line);
            }
        }
    }

    /// Paint step `i`'s failed screen when `result` is an `Err`, then pass
    /// the result through for `?`. The generic error exit — reason
    /// `"failed"`, the error chain as the one detail row — shared by every
    /// fallible call between [`Self::activate`] and the step's outcome
    /// handling, so no error can leave the step Active with no failed
    /// screen. Sites with a specific reason (declined, secret-like content,
    /// …) keep their explicit [`Self::fail`] call.
    pub fn fail_on_err<T>(
        &self,
        display: &Display,
        i: usize,
        result: anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if let Err(e) = &result {
            self.fail(
                display,
                i,
                Failure {
                    reason: "failed".into(),
                    detail: vec![format!("{e:#}")],
                },
            );
        }
        result
    }

    /// Replace the active step `i`'s live rows wholesale (the reasoning
    /// window's repaint path). No-op off a terminal.
    pub fn live(&self, i: usize, rows: Vec<String>) {
        let mut st = self.0.lock().unwrap();
        st.steps[i].live_rows = rows;
        st.repaint();
    }

    /// Erase the active step `i`'s live rows in one frame.
    pub fn clear_live(&self, i: usize) {
        self.live(i, Vec::new());
    }

    /// Set draft sub-row `index` (1-based) of `total` for step `i`
    /// (spec §2.6): revealed as drafts start, flips to its done form on
    /// completion. The step's live rows are the compacted slots in index
    /// order, so an out-of-order finish can't reorder the rows.
    pub fn set_sub_row(&self, i: usize, index: usize, total: usize, body: &SubBody) {
        let mut st = self.0.lock().unwrap();
        let width = st.paint_width();
        let theme = st.theme();
        let slots = &mut st.subs[i];
        if slots.len() != total {
            slots.resize_with(total, || None);
        }
        let row = sub_row(index, total, body, width, &theme);
        slots[index - 1] = Some(row);
        st.steps[i].live_rows = slots.iter().flatten().cloned().collect();
        st.repaint();
    }

    /// One transient live row for an in-flight sub-action (the confirmation
    /// menu's `⠋ Re-generating commit message`): replaces the active step's
    /// live rows; the caller repaints the preview over it on completion.
    /// No-op off a terminal (nothing printed there today either).
    pub fn live_note(&self, label: &str) {
        let mut st = self.0.lock().unwrap();
        let Some(active) = st.active_index() else {
            return;
        };
        let theme = st.theme();
        let row = note_row(label, st.paint_width(), &theme);
        st.steps[active].live_rows = vec![row];
        st.repaint();
    }

    /// Erase the active step's live-note row (the Re-generate wait ends and
    /// the preview repaints over it, spec §2.7).
    pub fn clear_live_note(&self) {
        let mut st = self.0.lock().unwrap();
        if let Some(a) = st.active_index() {
            st.steps[a].live_rows.clear();
            st.repaint();
        }
    }

    /// Print rows as **permanent scrollback** below the frame (ADR 0018
    /// bounded-frame model): `render` builds the rows through a collecting
    /// [`Display`], then the frame is erased in one clear, the rows are
    /// written as plain terminal lines (never repainted again), and the
    /// frame repaints below them. This is the only way durable content
    /// (Commit entries) lands on a terminal: rows that ride the frame would
    /// grow it past the terminal height, where whole-frame repaint breaks —
    /// cursor-up clamps at the screen top and every repaint scrolls a full
    /// copy into the scrollback.
    ///
    /// Off a terminal `render` runs against `display` itself, so non-TTY
    /// output is byte-identical to painting through the step display.
    ///
    /// The painter's absolute-row tracking survives the print by arithmetic,
    /// not a fresh DSR probe: the clear leaves the cursor on the frame's top
    /// row, `n` written lines each advance one row, and a line written on
    /// the bottom margin scrolls instead — so the cursor lands on
    /// `min(top + n, height)`, exactly the descent [`frame_bytes`] tracks.
    /// Rows are budgeted to the terminal width by their builders, so no row
    /// ever wraps to two physical rows.
    pub fn flush(&self, display: &Display, render: impl FnOnce(&Display)) {
        let mut st = self.0.lock().unwrap();
        if st.paint.is_none() {
            render(display);
            return;
        }
        let width = st
            .paint
            .as_ref()
            .map_or_else(terminal_width, |p| p.raw_width);
        let rows: Vec<String> = {
            let sink = RowSink::default();
            let collected = Display::framed(sink.clone(), width);
            render(&collected);
            sink.lines()
        };
        {
            let p = st.paint.as_mut().expect("checked above");
            let prev_height = p.prev_height();
            let bytes = clear_frame_bytes(prev_height, p.cursor_hidden);
            let _ = p.term.write_str(&bytes);
            let _ = p.term.flush();
            p.cursor_hidden = false;
            // The clear ends on the frame's top row; remember it before
            // `prev_rows` resets so the post-print cursor row stays exact.
            let top = p
                .row
                .map(|r| r.saturating_sub(prev_height.saturating_sub(1)));
            p.prev_rows.clear();
            for row in &rows {
                let _ = p.term.write_line(row);
            }
            let _ = p.term.flush();
            p.row = top.map(|t| (t + rows.len()).min(p.height));
        }
        st.repaint();
    }

    /// Suspend the frame for a full-screen takeover (the message editor,
    /// ADR 0018): one-frame erase, cursor restored. [`Self::resume`]
    /// repaints the whole frame afterwards.
    pub fn suspend(&self) {
        let mut st = self.0.lock().unwrap();
        let Some(p) = st.paint.as_mut() else {
            return;
        };
        let bytes = clear_frame_bytes(p.prev_height(), p.cursor_hidden);
        let _ = p.term.write_str(&bytes);
        let _ = p.term.flush();
        p.prev_rows.clear();
        p.cursor_hidden = false;
    }

    /// Repaint the whole frame from scratch after a [`Self::suspend`].
    pub fn resume(&self) {
        let mut st = self.0.lock().unwrap();
        st.repaint();
    }

    /// Stop the clock while a menu is open (ADR 0018: durations cover LLM /
    /// git work, not prompt time).
    pub fn menu_open(&self) {
        let mut st = self.0.lock().unwrap();
        if st.menu_pause.is_none() {
            st.menu_pause = Some(Instant::now());
        }
    }

    /// Resume the clock after the menu closes.
    pub fn menu_close(&self) {
        let mut st = self.0.lock().unwrap();
        if let Some(t) = st.menu_pause.take() {
            st.menu_total += t.elapsed();
        }
    }

    /// The active step's index (menus render as its live rows).
    pub fn active_index(&self) -> Option<usize> {
        self.0.lock().unwrap().active_index()
    }

    /// Current live-row count of step `i` — the menu renderer's append base.
    pub fn live_len(&self, i: usize) -> usize {
        self.0.lock().unwrap().steps[i].live_rows.len()
    }

    /// Truncate step `i`'s live rows to `len`, then append `rows` — the menu
    /// cursor's in-place redraw.
    pub fn replace_live_tail(&self, i: usize, len: usize, rows: Vec<String>) {
        let mut st = self.0.lock().unwrap();
        st.steps[i].live_rows.truncate(len);
        st.steps[i].live_rows.extend(rows);
        st.repaint();
    }

    /// Drop steps `i..` from the skeleton: the resolve flow's blocked
    /// Finalize — when any file was rejected or skipped, the timeline ends
    /// at Review with the hand-off rows, and the never-run Finalize step is
    /// absent from the frame entirely (spec §2.8), not rendered as pending.
    pub fn truncate_steps(&self, i: usize) {
        let mut st = self.0.lock().unwrap();
        st.steps.truncate(i);
        st.repaint();
    }

    /// Truncate step `i`'s live rows back to `len` (the menu closing).
    pub fn truncate_live(&self, i: usize, len: usize) {
        let mut st = self.0.lock().unwrap();
        st.steps[i].live_rows.truncate(len);
        st.repaint();
    }

    /// The reasoning window's height budget for the active step (spec §2.4):
    /// `min(DSR max_rows, terminal_height − timeline_fixed_rows − 2)`,
    /// suppressed below [`MIN_WINDOW_ROWS`](crate::render::timeline::MIN_WINDOW_ROWS).
    pub fn window_budget(&self) -> usize {
        let st = self.0.lock().unwrap();
        let Some(p) = st.paint.as_ref() else {
            return 0;
        };
        window_height_budget(p.max_window_rows, p.height, st.fixed_rows())
    }

    /// A [`Display`] whose output lands in the frame instead of on the
    /// terminal: lines emitted while a step is active ride its live rows
    /// (becoming durable at completion); lines emitted after a step completed
    /// land directly under its summary (the deterministic-fallback caution
    /// row). Off a terminal this is the Run's own sink, so non-TTY output is
    /// byte-identical to the pre-timeline behavior.
    pub fn step_display(&self, base: &Display) -> Display {
        let st = self.0.lock().unwrap();
        match st.paint.as_ref() {
            Some(p) => Display::framed(
                FrameSink {
                    frame: Self(Arc::clone(&self.0)),
                },
                p.width,
            ),
            None => base.share(),
        }
    }

    #[cfg(test)]
    pub fn step(&self, i: usize) -> Step {
        self.0.lock().unwrap().steps[i].clone()
    }
}

impl FrameState {
    fn active_index(&self) -> Option<usize> {
        self.steps.iter().position(|s| s.state == StepState::Active)
    }

    /// The step a step-scoped sink row belongs to: the active one, else the
    /// most recent completed one.
    fn sink_step(&self) -> usize {
        self.active_index().unwrap_or_else(|| {
            self.steps
                .iter()
                .rposition(|s| s.state == StepState::Completed)
                .unwrap_or(0)
        })
    }

    /// That step's rows: live while it is active (erased/promoted at
    /// completion), durable once completed (spec §2.7's caution rows).
    fn sink_rows_mut(&mut self, idx: usize) -> &mut Vec<String> {
        if self.steps[idx].state == StepState::Active {
            &mut self.steps[idx].live_rows
        } else {
            &mut self.steps[idx].durable_rows
        }
    }

    fn paint_width(&self) -> usize {
        self.paint.as_ref().map_or_else(terminal_width, |p| p.width)
    }

    fn theme(&self) -> Theme {
        Theme {
            colors: self.paint.as_ref().is_some_and(|p| p.colors),
        }
    }

    /// The active step's elapsed, excluding time spent at menus.
    fn step_elapsed(&self, i: usize) -> Duration {
        let Some(start) = self.starts[i] else {
            return Duration::ZERO;
        };
        let paused = self.menu_total + self.menu_pause.map(|t| t.elapsed()).unwrap_or_default();
        start.elapsed().saturating_sub(paused)
    }

    /// Every fixed row of the frame (all step rows, the separator, durable
    /// rows) — the reasoning window's budget input (spec §2.4).
    fn fixed_rows(&self) -> usize {
        let width = self.paint_width();
        let theme = self.theme();
        let mut steps = self.steps.clone();
        if let Some(a) = steps.iter().position(|s| s.state == StepState::Active) {
            steps[a].live_rows.clear();
        }
        timeline_frame(&steps, width, &theme).len()
    }

    /// Whole-frame repaint: bake the elapsed into active step lines, build
    /// the pure frame, animate the spinner glyph, write the bytes. Identical
    /// frames are skipped (a paused menu tick writes nothing).
    fn repaint(&mut self) {
        let (width, colors) = match self.paint.as_ref() {
            Some(p) => (p.width, p.colors),
            None => return,
        };
        for i in 0..self.steps.len() {
            if self.steps[i].state == StepState::Active {
                self.steps[i].active_elapsed = Some(self.step_elapsed(i));
            }
        }
        let theme = Theme { colors };
        let mut rows = timeline_frame(&self.steps, width, &theme);
        let glyph = GLYPHS[self.glyph % GLYPHS.len()];
        for row in &mut rows {
            if row.contains(GLYPHS[0]) {
                *row = row.replacen(GLYPHS[0], glyph, 1);
            }
        }
        let Some(p) = self.paint.as_mut() else {
            return;
        };
        if rows == p.prev_rows {
            return;
        }
        let first = p.prev_height() == 0;
        p.cursor_hidden |= first;
        let height = p.height;
        let (bytes, end_row) = frame_bytes(&rows, p.prev_height(), p.row, height);
        let _ = p.term.write_str(&bytes);
        let _ = p.term.flush();
        p.row = end_row;
        p.prev_rows = rows;
    }
}

/// A collecting [`DisplayWrite`] for [`RunFrame::flush`]: a [`Display`]
/// rendered into it yields the finished rows — styled per stderr's color
/// capability, full rows exactly as a terminal would receive them — without
/// touching the terminal. Cheap clone shares the buffer.
#[derive(Default, Clone)]
struct RowSink(Arc<Mutex<Vec<String>>>);

impl RowSink {
    fn lines(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl DisplayWrite for RowSink {
    fn write_line(&self, line: &str) {
        self.0.lock().unwrap().push(line.to_string());
    }
    fn clear_last(&self, n: usize) {
        let mut buf = self.0.lock().unwrap();
        let keep = buf.len().saturating_sub(n);
        buf.truncate(keep);
    }
    fn colors_enabled(&self) -> bool {
        console::colors_enabled_stderr()
    }
}

/// The frame-capturing [`DisplayWrite`] (see [`RunFrame::step_display`]).
struct FrameSink {
    frame: RunFrame,
}

impl DisplayWrite for FrameSink {
    fn write_line(&self, line: &str) {
        let mut st = (self.frame.0).lock().unwrap();
        // Rows ride the active step's live output (ephemeral, promoted to
        // durable at completion — see `complete`); rows emitted between
        // steps (the fallback caution row after Plan completes) land
        // directly under the most recent completed step's summary.
        let idx = st.sink_step();
        st.sink_rows_mut(idx).push(line.to_string());
        st.repaint();
    }

    fn clear_last(&self, n: usize) {
        if n == 0 {
            return;
        }
        let mut st = (self.frame.0).lock().unwrap();
        let idx = st.sink_step();
        let rows = st.sink_rows_mut(idx);
        let keep = rows.len().saturating_sub(n);
        rows.truncate(keep);
        st.repaint();
    }

    fn colors_enabled(&self) -> bool {
        console::colors_enabled_stderr()
    }
}

/// The menu key loop shared by the production frame menus (ADR 0007's
/// zero-residue contract, now by construction: closing the menu is just a
/// shorter frame). Renders `options` with the `›` cursor as live rows under
/// the active step (below whatever preview already sits there), reads raw
/// keys, and returns the chosen index — the inner `None` on Esc / Ctrl-C /
/// closed stdin. The outer `None` means "no frame is painting" (no active
/// Run, or non-TTY stderr): the caller falls back to the inquire row. The
/// optional `ask` renders one `?` row above the options (the merge-pick
/// prompts); the frame menus otherwise sit under their own self-describing
/// content.
pub(crate) fn frame_menu(
    ask: Option<&str>,
    options: &[&str],
    selected: usize,
) -> Option<Option<usize>> {
    let frame = active_frame()?;
    if !frame.is_tty() {
        return None;
    }
    let step = frame.active_index()?;
    frame.menu_open();
    let base = frame.live_len(step);
    let width = frame.width();
    let theme = frame.theme();
    let mut ask_rows = Vec::new();
    if let Some(q) = ask {
        ask_rows.push(crate::render::timeline::ask_row(q, width, &theme));
    }
    let mut sel = selected.min(options.len().saturating_sub(1));
    let term = Term::stderr();
    let outcome = loop {
        let (rows, _) = menu_rows(options, sel, width, &theme);
        let mut all = ask_rows.clone();
        all.extend(rows);
        frame.replace_live_tail(step, base, all);
        match term.read_key() {
            Ok(console::Key::ArrowUp) if !options.is_empty() => {
                sel = (sel + options.len() - 1) % options.len();
            }
            Ok(console::Key::ArrowDown) if !options.is_empty() => {
                sel = (sel + 1) % options.len();
            }
            Ok(console::Key::Enter) => break Some(sel),
            // Esc / Ctrl-C / a read failure (closed stdin) all cancel.
            Ok(console::Key::Escape) | Err(_) => break None,
            Ok(console::Key::Char('\u{3}')) => break None,
            Ok(_) => {}
        }
    };
    frame.truncate_live(step, base);
    frame.menu_close();
    Some(outcome)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A recording sink: captures lines, pops on clear_last — the same
    /// contract the e2e BufferWrite harness pins. (`cargo test` runs with
    /// stderr piped, so every frame built here is the non-TTY ledger mode —
    /// exactly the path these tests pin.)
    #[derive(Default, Clone)]
    struct Rec(Arc<Mutex<Vec<String>>>);

    impl Rec {
        fn lines(&self) -> Vec<String> {
            self.0.lock().unwrap().clone()
        }
    }

    impl DisplayWrite for Rec {
        fn write_line(&self, line: &str) {
            self.0.lock().unwrap().push(line.to_string());
        }
        fn clear_last(&self, n: usize) {
            let mut buf = self.0.lock().unwrap();
            let keep = buf.len().saturating_sub(n);
            buf.truncate(keep);
        }
    }

    fn display() -> (Display, Rec) {
        let rec = Rec::default();
        (Display::with(rec.clone()), rec)
    }

    async fn frame(labels: &[&str]) -> RunFrame {
        RunFrame::new(labels.iter().map(|s| s.to_string()).collect()).await
    }

    /// The non-TTY ledger (spec §5): only completed and failed steps print,
    /// one plain line each — pending and active steps never do.
    #[tokio::test]
    async fn ledger_prints_only_terminal_steps() {
        let (display, rec) = display();
        let f = frame(&["Scan changes", "Plan batches", "Commit batches"]).await;
        f.activate(0);
        assert!(rec.lines().is_empty(), "an active step must not print");
        f.complete(&display, 0, "Scanned 2 files");
        f.complete(&display, 2, "Committed 1 batch");
        f.fail(
            &display,
            1,
            Failure {
                reason: "secret-like content detected".into(),
                detail: vec!["src/config.rs: hunk 2 looks like an API key".into()],
            },
        );
        assert_eq!(
            rec.lines(),
            vec![
                "  ✓ Scanned 2 files",
                "  ✓ Committed 1 batch",
                "  ✗ Plan batches — secret-like content detected",
                "  src/config.rs: hunk 2 looks like an API key",
            ]
        );
    }

    /// Draft sub-row accounting (spec §2.6): slots are revealed as drafts
    /// start (out-of-order finishes never reorder rows), and ALL sub-rows
    /// erase at step completion — the completed step keeps no live rows.
    #[tokio::test]
    async fn draft_sub_rows_reveal_and_erase_together() {
        let (display, _rec) = display();
        let f = frame(&["Plan batches", "Draft messages"]).await;
        f.activate(1);
        f.set_sub_row(
            1,
            1,
            3,
            &SubBody::InFlight("Generating commit message".into()),
        );
        f.set_sub_row(
            1,
            3,
            3,
            &SubBody::InFlight("Generating commit message".into()),
        );
        let live = f.step(1).live_rows;
        assert_eq!(live.len(), 2, "only started drafts are revealed");
        assert!(live[0].contains("[1/3]"), "rows keep index order: {live:?}");
        assert!(live[1].contains("[3/3]"), "rows keep index order: {live:?}");
        // A finish flips its own row only; the other stays in flight.
        f.set_sub_row(1, 3, 3, &SubBody::Done(String::new(), None));
        assert_eq!(f.step(1).live_rows.len(), 2);
        assert!(f.step(1).live_rows[1].contains('✓'));
        f.complete(&display, 1, "Drafted 3 messages");
        let step = f.step(1);
        assert!(
            step.live_rows.is_empty(),
            "completion erases every sub-row in one frame"
        );
        assert!(step.durable_rows.is_empty(), "drafts leave nothing durable");
    }

    /// Rows emitted through the frame-capturing sink (the step Display on a
    /// terminal) while a step is active ride its live rows and become its
    /// durable block at completion (spec §2.7's already-landed Commit
    /// entries); after completion they land directly under the summary (the
    /// fallback caution row); `clear_last` pops them exactly.
    #[tokio::test]
    async fn step_display_rows_become_durable_at_completion() {
        let (display, _rec) = display();
        let f = frame(&["Plan batches", "Commit batches"]).await;
        f.complete(&display, 0, "Planned 2 batches");
        let step_display = Display::framed(FrameSink { frame: f.clone() }, 80);
        // After completion: durable under the completed step.
        step_display.emit("⚠ LLM plan invalid; regrouping");
        assert_eq!(f.step(0).durable_rows.len(), 1);
        // While active: live, then durable at completion.
        f.activate(1);
        step_display.emit("[1/2] ✓ a1b2c3d feat: first");
        step_display.emit("+1 −1  tracked.txt");
        assert_eq!(f.step(1).live_rows.len(), 2);
        step_display.clear_last(1);
        assert_eq!(f.step(1).live_rows.len(), 1, "clear_last pops live rows");
        step_display.emit("+2 −2  other.txt");
        f.complete(&display, 1, "Committed 2 batches");
        assert_eq!(f.step(1).durable_rows.len(), 2);
    }

    /// `flush` off a terminal (the only path unit tests can drive — no
    /// painter exists) renders straight through the passed display — which
    /// must be the Run's own sink, never a frame-captured one (the render
    /// runs under the frame lock; a FrameSink sink would re-enter it).
    /// Production is safe by construction: `step_display` hands out the
    /// FrameSink sink only on a terminal, where `flush` ignores the passed
    /// display. The flushed rows are byte-identical to painting them
    /// directly, and nothing is retained in any step's live/durable rows.
    /// The TTY geometry (erase → print below → repaint) is pinned by the
    /// PTY render-verification harness, not here.
    #[tokio::test]
    async fn flush_off_terminal_paints_through_the_display_only() {
        let (display, rec) = display();
        let f = frame(&["Scan changes", "Commit batches"]).await;
        f.complete(&display, 0, "Scanned 2 files");
        f.flush(&display, |d| {
            d.emit("[1/2] ✓ a1b2c3d feat: first");
            d.emit("+1 −1  tracked.txt");
        });
        assert_eq!(
            rec.lines(),
            vec![
                "  ✓ Scanned 2 files",
                "  [1/2] ✓ a1b2c3d feat: first",
                "  +1 −1  tracked.txt",
            ]
        );
        assert!(f.step(1).live_rows.is_empty());
        assert!(f.step(1).durable_rows.is_empty());
    }

    /// The install guard restores the previous frame on drop, so a nested
    /// install leaves the outer frame intact when the inner one ends.
    /// (Only `is_some` is asserted: the process-global slot is shared with
    /// the e2e runs executing in parallel in this test binary.)
    #[tokio::test]
    async fn install_guard_restores_the_previous_frame() {
        let outer = frame(&["Outer run"]).await;
        let guard_outer = install(outer.clone());
        assert!(active_frame().is_some());
        let guard_inner = install(frame(&["Inner run"]).await);
        assert!(active_frame().is_some());
        drop(guard_inner);
        assert!(active_frame().is_some());
        drop(guard_outer);
        outer.complete(&display().0, 0, "Scanned 1 file");
    }
}

/// End-to-end [`RunFrame::flush` geometry over a real PTY — the one piece of
/// the timeline the pure builder tests cannot see (they run off a terminal,
/// where `flush` degrades to a plain emit). A forked child paints a frame
/// onto the pty as its stdio and flushes one entry row through it; the
/// parent replays DSR replies, captures the master side, and asserts the
/// erase → entry → repaint sequence.
#[cfg(all(unix, test))]
mod pty_tests {
    use super::*;
    use crate::render::cursor::pty_tests::open_pty;

    /// `flush` on a terminal must (1) erase the painted frame in one
    /// sequence, (2) print the flushed entry rows exactly once, and (3)
    /// repaint the frame below them — the cursor arithmetic that keeps
    /// `prev_rows` and the screen in lockstep. A regression in the erase or
    /// repaint order shows up as the entry row duplicated or the step
    /// summaries never reappearing after it.
    ///
    /// Fork discipline: unlike `cursor::pty_tests` (libc-only children),
    /// this child allocates — it builds a single-thread tokio runtime and
    /// paints real rows. // ponytail: fork-with-allocation can deadlock on
    /// a foreign malloc lock held at the fork instant; if that ever bites,
    /// move the child into a dedicated spawned test binary.
    #[test]
    fn flush_erases_prints_entry_and_repaints_on_a_tty() {
        let (master, slave) = open_pty();
        // 30×100 geometry: room for the frame below the row-12 DSR reply.
        let ws = libc::winsize {
            ws_row: 30,
            ws_col: 100,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        assert_eq!(unsafe { libc::ioctl(slave, libc::TIOCSWINSZ, &ws) }, 0);

        let pid = unsafe { libc::fork() };
        assert!(pid >= 0, "fork failed");
        if pid == 0 {
            unsafe {
                libc::dup2(slave, 0);
                libc::dup2(slave, 1);
                libc::dup2(slave, 2);
                if slave > 2 {
                    libc::close(slave);
                }
                libc::close(master);
            }
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("child runtime");
            runtime.block_on(async {
                let frame = RunFrame::new(vec!["Scan".into(), "Plan".into()]).await;
                let display = Display::new();
                frame.activate(0);
                frame.live(0, vec!["scan live".into()]);
                frame.complete(&display, 0, "Scanned 2 files");
                frame.activate(1);
                frame.live(1, vec!["plan live".into()]);
                frame.flush(&display, |d| d.emit("  PTYFLUSH-ENTRY row"));
                frame.complete(&display, 1, "Planned 2 batches");
            });
            let msg = b"\nPTYFLUSH-DONE\n";
            unsafe {
                libc::write(2, msg.as_ptr().cast(), msg.len());
                libc::_exit(0);
            }
        }

        // Parent: fake terminal — answer every DSR query with row 12 (the
        // probe's canonical path; row ≤ height so it is kept), keep reading
        // the master until the child exits, then drain what is left.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut capture: Vec<u8> = Vec::new();
        let mut buf = [0u8; 4096];
        let mut replied = 0usize;
        let mut exited = false;
        while !exited {
            let mut status = 0;
            let r = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            assert!(r >= 0, "waitpid failed");
            let mut pfd = libc::pollfd {
                fd: master,
                events: libc::POLLIN,
                revents: 0,
            };
            while unsafe { libc::poll(&mut pfd, 1, 0) } > 0 {
                let n = unsafe { libc::read(master, buf.as_mut_ptr().cast(), buf.len()) };
                if n <= 0 {
                    break;
                }
                capture.extend_from_slice(&buf[..n as usize]);
            }
            // Answer each DSR query exactly once, promptly (a reply after
            // the probe's raw window closes echoes as garbage).
            let queries = capture.windows(4).filter(|w| *w == b"\x1b[6n").count();
            while replied < queries {
                let reply = b"\x1b[12;1R";
                unsafe { libc::write(master, reply.as_ptr().cast(), reply.len()) };
                replied += 1;
            }
            if r == pid {
                assert!(
                    libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
                    "child drive failed: {status}"
                );
                exited = true;
            } else {
                assert!(
                    std::time::Instant::now() < deadline,
                    "child did not exit in 10s — flush deadlocked?"
                );
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
        }
        unsafe {
            libc::close(master);
            libc::close(slave);
        }

        let text = String::from_utf8_lossy(&capture);
        assert!(
            text.contains("PTYFLUSH-DONE"),
            "child never finished its drive"
        );
        assert_eq!(
            text.matches("PTYFLUSH-ENTRY").count(),
            1,
            "flushed entry row must print exactly once, not duplicate"
        );
        assert!(
            text.contains("\x1b[2K"),
            "flush must erase the painted frame (CLR_LINE bytes)"
        );
        let first_summary = text
            .find("Scanned 2 files")
            .expect("frame painted pre-flush");
        let entry_at = text.find("PTYFLUSH-ENTRY").expect("entry row printed");
        let last_summary = text
            .rfind("Scanned 2 files")
            .expect("frame repainted post-flush");
        assert!(
            first_summary < entry_at,
            "frame must be painted before the flush"
        );
        assert!(
            last_summary > entry_at,
            "frame must repaint below the flushed entry row"
        );
    }
}
