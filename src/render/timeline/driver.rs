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
    Failure, Step, StepIdx, StepState, SubBody, Theme, help_row, menu_rows, plain_step_lines,
    sub_row, timeline_frame, window_height_budget,
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
                if !st.steps.iter().any(|s| s.state == StepState::Active) {
                    break;
                }
                // A menu is open: the Run waits on the user, not on work —
                // freeze the glyph and skip the repaint. Motion would read
                // as progress while nothing progresses, and the static
                // frame is the visual cue that input is expected.
                if st.menu_pause.is_some() {
                    continue;
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

    /// The frame's styling policy + row-width budget.
    pub fn theme(&self) -> Theme {
        self.0.lock().unwrap().theme()
    }

    /// Flip step `i` to active and start its clock.
    pub fn activate(&self, i: StepIdx) {
        let mut st = self.0.lock().unwrap();
        st.starts[i.0] = Some(Instant::now());
        st.menu_total = Duration::ZERO;
        st.menu_pause = None;
        st.steps[i.0].state = StepState::Active;
        st.repaint();
    }

    /// Replace the (active) step's label — the live commit cursor
    /// `⠋ Commit batches · 2/3` (ADR 0018).
    pub fn set_label(&self, i: StepIdx, label: impl Into<String>) {
        let mut st = self.0.lock().unwrap();
        st.steps[i.0].label = label.into();
        st.repaint();
    }

    /// Complete step `i` with `summary`. Its live output (sub-rows, preview,
    /// reasoning window) erases; rows emitted while it was active persist as
    /// its durable block. On a terminal the finished block — `✓` summary
    /// line plus durable rows — then **peels** to permanent scrollback
    /// (chronological collapse, ADR 0018 amendment 3) and the frame repaints
    /// below it holding only the steps still in play, so permanent history
    /// reads in happening order. Off a terminal the same block prints as the
    /// plain ledger instead; on a TTY no ledger line is emitted (the peel
    /// just printed it — a second emit would duplicate the line and push the
    /// cursor out from under the painter's `prev_rows` geometry, desyncing
    /// every later whole-frame repaint).
    pub fn complete(&self, display: &Display, i: StepIdx, summary: impl Into<String>) {
        let lines = {
            let mut st = self.0.lock().unwrap();
            let duration = st.step_elapsed(i);
            // A step with draft sub-rows owns its live rows wholly (the
            // compacted sub-row slots) — they erase; any other live rows
            // (warnings emitted while active) are the durable record.
            // ponytail: sub-row-owned steps drop late live rows (a rare
            // staging warning) at peel; route to durable rows if one ever
            // matters.
            let has_subs = !st.subs[i.0].is_empty();
            st.subs[i.0].clear();
            let step = &mut st.steps[i.0];
            step.state = StepState::Completed;
            step.summary = Some(summary.into());
            step.duration = Some(duration);
            step.active_elapsed = None;
            let live = std::mem::take(&mut step.live_rows);
            if !live.is_empty() && !has_subs {
                step.durable_rows = live;
            }
            if st.paint.is_some() {
                // Chronological collapse (ADR 0018 amendment 3): peel the
                // completed block (✓ line + durable rows, one blank after a
                // durable block — the frame's spacing rule) to permanent
                // scrollback and repaint only the steps still in play, so
                // permanent history reads in happening order and the frame
                // stays active-step-only. The ledger emit below stays
                // non-TTY-only for the same reason it always was: the block
                // just printed is the TTY's copy of it.
                let theme = st.theme();
                let mut block = timeline_frame(std::slice::from_ref(&st.steps[i.0]), &theme);
                if !st.steps[i.0].durable_rows.is_empty() {
                    block.push(String::new());
                }
                st.steps[i.0].peeled = true;
                st.flush_rows(block);
                Vec::new()
            } else {
                plain_step_lines(&st.steps[i.0])
            }
        };
        for line in lines {
            display.emit(&line);
        }
    }

    /// Fail step `i`: `✗` headline + detail rows persist; every later step
    /// is absent from the frame (ADR 0018). Off a terminal the headline and
    /// details append to the ledger; on a terminal the frame shows them in
    /// place (a second emit below the frame would duplicate the block and
    /// desync the repaint geometry — see [`Self::complete`]). Nothing erases
    /// on failure.
    pub fn fail(&self, display: &Display, i: StepIdx, failure: Failure) {
        let (lines, ledger) = {
            let mut st = self.0.lock().unwrap();
            st.steps[i.0].state = StepState::Failed;
            st.steps[i.0].failure = Some(failure);
            st.steps[i.0].live_rows.clear();
            st.steps[i.0].active_elapsed = None;
            st.repaint();
            (
                plain_step_lines(&st.steps[i.0]),
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
        i: StepIdx,
        result: anyhow::Result<T>,
    ) -> anyhow::Result<T> {
        if let Err(e) = &result {
            self.fail(display, i, Failure::from_error("failed", e));
        }
        result
    }

    /// Replace the active step `i`'s live rows wholesale (the reasoning
    /// window's repaint path). No-op off a terminal.
    pub fn live(&self, i: StepIdx, rows: Vec<String>) {
        let mut st = self.0.lock().unwrap();
        st.steps[i.0].live_rows = rows;
        st.repaint();
    }

    /// Erase the active step `i`'s live rows in one frame.
    pub fn clear_live(&self, i: StepIdx) {
        self.live(i, Vec::new());
    }

    pub fn set_sub_row(&self, i: StepIdx, index: usize, body: &SubBody) {
        let mut st = self.0.lock().unwrap();
        let theme = st.theme();
        // The raw terminal width — the budget a live row must fit.
        // `sub_row` clamps its text to it, so no sub-row ever wraps to two
        // physical rows (a wrapped live row would desync the painter's
        // whole-frame repaint on every later tick).
        let width = st
            .paint
            .as_ref()
            .map_or_else(terminal_width, |p| p.raw_width);
        let slots = &mut st.subs[i.0];
        while slots.len() < index {
            slots.push(None);
        }
        let row = sub_row(body, width, &theme);
        slots[index - 1] = Some(row);
        st.steps[i.0].live_rows = slots.iter().flatten().cloned().collect();
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
        let row = note_row(label, &theme);
        st.steps[active.0].live_rows = vec![row];
        st.repaint();
    }

    /// Erase the active step's live-note row (the Re-generate wait ends and
    /// the preview repaints over it, ADR 0018).
    pub fn clear_live_note(&self) {
        let mut st = self.0.lock().unwrap();
        if let Some(a) = st.active_index() {
            st.steps[a.0].live_rows.clear();
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
    /// not a fresh DSR probe — see [`FrameState::flush_rows`].
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
        let sink = RowSink::default();
        let collected = Display::with_cols(sink.clone(), width);
        render(&collected);
        st.flush_rows(sink.lines());
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
    pub fn active_index(&self) -> Option<StepIdx> {
        self.0.lock().unwrap().active_index()
    }

    /// Current live-row count of step `i` — the menu renderer's append base.
    pub fn live_len(&self, i: StepIdx) -> usize {
        self.0.lock().unwrap().steps[i.0].live_rows.len()
    }

    /// Truncate step `i`'s live rows to `len`, then append `rows` — the menu
    /// cursor's in-place redraw.
    pub fn replace_live_tail(&self, i: StepIdx, len: usize, rows: Vec<String>) {
        let mut st = self.0.lock().unwrap();
        st.steps[i.0].live_rows.truncate(len);
        st.steps[i.0].live_rows.extend(rows);
        st.repaint();
    }

    /// Drop steps `i..` from the skeleton: the resolve flow's blocked
    /// Finalize — when any file was rejected or skipped, the timeline ends
    /// at Review with the hand-off rows, and the never-run Finalize step is
    /// absent from the frame entirely (ADR 0018), not rendered as pending.
    pub fn truncate_steps(&self, i: StepIdx) {
        let mut st = self.0.lock().unwrap();
        st.steps.truncate(i.0);
        st.repaint();
    }

    /// Truncate step `i`'s live rows back to `len` (the menu closing).
    pub fn truncate_live(&self, i: StepIdx, len: usize) {
        let mut st = self.0.lock().unwrap();
        st.steps[i.0].live_rows.truncate(len);
        st.repaint();
    }

    /// The reasoning window's height budget for the active step (ADR 0018):
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
            Some(p) => Display::with_cols(
                FrameSink {
                    frame: Self(Arc::clone(&self.0)),
                },
                p.width,
            ),
            None => base.share(),
        }
    }

    #[cfg(test)]
    pub fn step(&self, i: StepIdx) -> Step {
        self.0.lock().unwrap().steps[i.0].clone()
    }
}

impl FrameState {
    fn active_index(&self) -> Option<StepIdx> {
        self.steps
            .iter()
            .position(|s| s.state == StepState::Active)
            .map(StepIdx)
    }

    /// The step a step-scoped sink row belongs to: the active one, else the
    /// most recent completed one.
    fn sink_step(&self) -> StepIdx {
        self.active_index().unwrap_or_else(|| {
            self.steps
                .iter()
                .rposition(|s| s.state == StepState::Completed)
                .map(StepIdx)
                .unwrap_or(StepIdx::FIRST)
        })
    }

    /// That step's rows: live while it is active (erased/promoted at
    /// completion), durable once completed (ADR 0018's caution rows).
    fn sink_rows_mut(&mut self, idx: StepIdx) -> &mut Vec<String> {
        if self.steps[idx.0].state == StepState::Active {
            &mut self.steps[idx.0].live_rows
        } else {
            &mut self.steps[idx.0].durable_rows
        }
    }

    fn theme(&self) -> Theme {
        Theme {
            colors: self.paint.as_ref().is_some_and(|p| p.colors),
            width: self.paint.as_ref().map_or_else(terminal_width, |p| p.width),
        }
    }

    /// The active step's elapsed, excluding time spent at menus.
    fn step_elapsed(&self, i: StepIdx) -> Duration {
        let Some(start) = self.starts[i.0] else {
            return Duration::ZERO;
        };
        let paused = self.menu_total + self.menu_pause.map(|t| t.elapsed()).unwrap_or_default();
        start.elapsed().saturating_sub(paused)
    }

    /// Every fixed row of the frame (all step rows, the separator, durable
    /// rows) — the reasoning window's budget input (ADR 0018).
    fn fixed_rows(&self) -> usize {
        let theme = self.theme();
        let mut steps = self.steps.clone();
        if let Some(a) = steps.iter().position(|s| s.state == StepState::Active) {
            steps[a].live_rows.clear();
        }
        timeline_frame(&steps, &theme).len()
    }

    /// The painter half of [`RunFrame::flush`] (and the chronological-
    /// collapse peel in [`RunFrame::complete`], ADR 0018 amendment 3): one
    /// clear of the live frame, `rows` written as permanent terminal lines
    /// (never repainted again), then the remaining frame repainted below —
    /// the bounded-frame model. `rows` are finished full rows (leading
    /// margin included, already styled); they must be budgeted to the
    /// terminal width by their builders so no row wraps to two physical
    /// rows. The painter's absolute-row tracking survives the print by
    /// arithmetic, not a fresh DSR probe: the clear leaves the cursor on the
    /// frame's top row, `n` written lines each advance one row, and a line
    /// written on the bottom margin scrolls instead — so the cursor lands
    /// on `min(top + n, height)`, exactly the descent [`frame_bytes`]
    /// tracks.
    fn flush_rows(&mut self, rows: Vec<String>) {
        let p = self.paint.as_mut().expect("caller checks paint");
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
        self.repaint();
    }
    fn repaint(&mut self) {
        let theme = match self.paint.as_ref() {
            Some(p) => Theme {
                colors: p.colors,
                width: p.width,
            },
            None => return,
        };
        for i in 0..self.steps.len() {
            if self.steps[i].state == StepState::Active {
                self.steps[i].active_elapsed = Some(self.step_elapsed(StepIdx(i)));
            }
        }
        let mut rows = timeline_frame(&self.steps, &theme);
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
/// prompts) and `help` one muted hints row beneath them, so the frame path
/// keeps the inquire fallback's discoverability.
pub(crate) fn frame_menu(
    ask: Option<&str>,
    options: &[&str],
    selected: usize,
    help: &str,
) -> Option<Option<usize>> {
    let frame = active_frame()?;
    if !frame.is_tty() {
        return None;
    }
    let step = frame.active_index()?;
    frame.menu_open();
    let base = frame.live_len(step);
    let theme = frame.theme();
    let mut ask_rows = Vec::new();
    if let Some(q) = ask {
        ask_rows.push(crate::render::timeline::ask_row(q, &theme));
    }
    // The key hints ride under the options — the same text the inquire
    // fallback shows as its help message, so both paths stay discoverable.
    let help = help_row(help, &theme);
    let mut sel = selected.min(options.len().saturating_sub(1));
    let term = Term::stderr();
    let outcome = loop {
        let (rows, _) = menu_rows(options, sel, &theme);
        let mut all = ask_rows.clone();
        all.extend(rows);
        all.push(help.clone());
        // Paint the menu rows before blocking on a key: without this the
        // ask, the `›` cursor, and the key hints never reach the frame and
        // the menu is invisible — reachable only by blind keys (the
        // regression this call restores). Repaints dedupe when the frame is
        // unchanged; only a selection move changes it.
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

    /// The non-TTY ledger (ADR 0018): only completed and failed steps print,
    /// one plain line each — pending and active steps never do.
    #[tokio::test]
    async fn ledger_prints_only_terminal_steps() {
        let (display, rec) = display();
        let f = frame(&["Scan changes", "Plan batches", "Commit batches"]).await;
        f.activate(StepIdx::FIRST);
        assert!(rec.lines().is_empty(), "an active step must not print");
        f.complete(&display, StepIdx::FIRST, "Scanned 2 files");
        f.complete(&display, StepIdx(2), "Committed 1 batch");
        f.fail(
            &display,
            StepIdx(1),
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

    /// Draft sub-row accounting (ADR 0018): slots are revealed as drafts
    /// start (out-of-order finishes never reorder rows), and ALL sub-rows
    /// erase at step completion — the completed step keeps no live rows.
    #[tokio::test]
    async fn draft_sub_rows_reveal_and_erase_together() {
        let (display, _rec) = display();
        let f = frame(&["Plan batches", "Draft messages"]).await;
        f.activate(StepIdx(1));
        f.set_sub_row(
            StepIdx(1),
            1,
            &SubBody::InFlight("Generating commit message".into()),
        );
        f.set_sub_row(
            StepIdx(1),
            3,
            &SubBody::InFlight("Generating commit message".into()),
        );
        let live = f.step(StepIdx(1)).live_rows;
        assert_eq!(live.len(), 2, "only started drafts are revealed");
        // Out-of-order finishes never reorder rows: the slot that finished
        // flips to ✓ in place, the not-yet-started middle slot stays absent.
        assert!(live[0].contains('⠋'), "rows keep slot order: {live:?}");
        f.set_sub_row(StepIdx(1), 3, &SubBody::Done(String::new(), None));
        assert_eq!(f.step(StepIdx(1)).live_rows.len(), 2);
        assert!(f.step(StepIdx(1)).live_rows[1].contains('✓'));
        f.complete(&display, StepIdx(1), "Drafted 3 messages");
        let step = f.step(StepIdx(1));
        assert!(
            step.live_rows.is_empty(),
            "completion erases every sub-row in one frame"
        );
        assert!(step.durable_rows.is_empty(), "drafts leave nothing durable");
    }

    /// Rows emitted through the frame-capturing sink (the step Display on a
    /// terminal) while a step is active ride its live rows and become its
    /// durable block at completion (ADR 0018's already-landed Commit
    /// entries); after completion they land directly under the summary (the
    /// fallback caution row); `clear_last` pops them exactly.
    #[tokio::test]
    async fn step_display_rows_become_durable_at_completion() {
        let (display, _rec) = display();
        let f = frame(&["Plan batches", "Commit batches"]).await;
        f.complete(&display, StepIdx::FIRST, "Planned 2 batches");
        let step_display = Display::with_cols(FrameSink { frame: f.clone() }, 80);
        // After completion: durable under the completed step.
        step_display.emit("⚠ LLM plan invalid; regrouping");
        assert_eq!(f.step(StepIdx::FIRST).durable_rows.len(), 1);
        // While active: live, then durable at completion.
        f.activate(StepIdx(1));
        step_display.emit("✓ a1b2c3d feat: first");
        step_display.emit("+1 −1  tracked.txt");
        assert_eq!(f.step(StepIdx(1)).live_rows.len(), 2);
        step_display.clear_last(1);
        assert_eq!(
            f.step(StepIdx(1)).live_rows.len(),
            1,
            "clear_last pops live rows"
        );
        step_display.emit("+2 −2  other.txt");
        f.complete(&display, StepIdx(1), "Committed 2 batches");
        assert_eq!(f.step(StepIdx(1)).durable_rows.len(), 2);
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
        f.complete(&display, StepIdx::FIRST, "Scanned 2 files");
        f.flush(&display, |d| {
            d.emit("✓ a1b2c3d feat: first");
            d.emit("+1 −1  tracked.txt");
        });
        assert_eq!(
            rec.lines(),
            vec![
                "  ✓ Scanned 2 files",
                "  ✓ a1b2c3d feat: first",
                "  +1 −1  tracked.txt",
            ]
        );
        assert!(f.step(StepIdx(1)).live_rows.is_empty());
        assert!(f.step(StepIdx(1)).durable_rows.is_empty());
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
        outer.complete(&display().0, StepIdx::FIRST, "Scanned 1 file");
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
    /// `prev_rows` and the screen in lockstep. Under chronological collapse
    /// (ADR 0018 amendment 3) a completed step PEELS instead: its ✓ block
    /// prints exactly once above the entry, the repaint below omits it, and
    /// the flow's last peel is the screen's tail. A regression shows up as
    /// the entry row duplicated, a peeled summary repainting again, or the
    /// tail order flipping.
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
                frame.activate(StepIdx::FIRST);
                frame.live(StepIdx::FIRST, vec!["scan live".into()]);
                frame.complete(&display, StepIdx::FIRST, "Scanned 2 files");
                frame.activate(StepIdx(1));
                frame.live(StepIdx(1), vec!["plan live".into()]);
                frame.flush(&display, |d| d.emit("  PTYFLUSH-ENTRY row"));
                frame.complete(&display, StepIdx(1), "Planned 2 batches");
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
        // Chronological collapse: the Scan ✓ block peeled to scrollback
        // exactly once (never repainted), above the flushed entry; the
        // final Plan peel lands below it — the tail of the screen.
        let summary_at = text.find("Scanned 2 files").expect("peeled block printed");
        let entry_at = text.find("PTYFLUSH-ENTRY").expect("entry row printed");
        assert_eq!(
            text.matches("Scanned 2 files").count(),
            1,
            "a peeled step prints exactly once — the frame never repaints it"
        );
        assert!(
            summary_at < entry_at,
            "the peeled ✓ block lands above the flushed entry"
        );
        let plan_at = text.rfind("Planned 2 batches").expect("final peel printed");
        assert!(
            plan_at > entry_at,
            "the flow's last peel is the tail, below the flushed entry"
        );
    }
}
