//! The Run's LLM-call rendering wrapper: paint streamed reasoning on the
//! live frame when one is installed (ADR 0018), or the standalone
//! probe-and-paint path off a terminal. Split from the Run spine so
//! `run/mod.rs` reads as workflow phases; the feed itself lives in
//! `render::reasoning_feed`.

use std::time::Duration;

use crate::core::types::BoxFuture;
use crate::render::cursor;
use crate::render::markdown::LoadingNotice;
use crate::render::markdown::loading_rows;
use crate::render::progress;
use crate::render::reasoning_feed;
use crate::render::timeline::StepIdx;
use crate::render::timeline::driver;
use crate::render::timeline::driver::RunFrame;

/// Run an LLM call behind the live reasoning feed. On a terminal with a Run
/// frame on screen the feed is the frame's [`FrameReasoningSink`] — the
/// window rides the Plan step's live rows, its height budgeted against the
/// whole frame (ADR 0018); otherwise the standalone probe-and-paint
/// [`progress::ReasoningRenderer`] path, as before the timeline.
pub(super) async fn run_with_reasoning_feed<F, T>(
    label: &'static str,
    cold_start: Option<String>,
    make_call: F,
) -> anyhow::Result<T>
where
    F: FnOnce(reasoning_feed::ReasoningTap) -> BoxFuture<anyhow::Result<T>>,
    T: Send,
{
    if let Some(frame) = driver::active_frame().filter(|f| f.is_tty()) {
        let step = frame.active_index();
        let mut sink = FrameReasoningSink {
            frame: frame.clone(),
            step: step.unwrap_or(StepIdx::FIRST),
            label,
            feed_width: frame.theme().width.saturating_sub(6).max(20),
        };
        return reasoning_feed::run(
            &mut sink,
            frame.window_budget(),
            cold_start.as_deref(),
            make_call,
        )
        .await;
    }
    // The DSR cursor-row query does up to ~200 ms of blocking tty I/O (poll
    // + raw-mode byte reads against a deadline). Run it on the blocking pool
    // so it stalls a worker, not the async reactor. A task panic degrades to
    // the no-scroll [`cursor::WindowSizing::fallback`] — decoration must
    // never break the commit.
    let sizing = tokio::task::spawn_blocking(cursor::reasoning_window_rows)
        .await
        .unwrap_or_else(|_| cursor::WindowSizing::fallback());
    let mut sink = progress::reasoning_sink(label, sizing.max_rows, sizing.cursor_row);
    reasoning_feed::run(&mut sink, sizing.max_rows, cold_start.as_deref(), make_call).await
}

/// The reasoning feed as the frame's Plan-step live rows (ADR 0018): the
/// window's rows replace the step's live output on every paint, and the
/// spinner + elapsed ride the step's own label (animated by the frame's
/// ticker), so only the window content repaints as reasoning streams. The
/// loading notice (cold start past the grace window) is the same window's
/// lone row. `finish` erases the window in one frame — the step's `✓`
/// flip takes it from there.
struct FrameReasoningSink {
    frame: RunFrame,
    step: StepIdx,
    label: &'static str,
    feed_width: usize,
}

impl reasoning_feed::ReasoningSink for FrameReasoningSink {
    fn paint(&mut self, window: &[String], in_code_start: bool) {
        let budget = self.frame.window_budget();
        let mut rows = crate::render::markdown::reasoning_rows(
            "⠋",
            self.label,
            window,
            self.feed_width,
            None,
            budget,
            in_code_start,
        );
        // Drop the spinner+elapsed headline row: the step's own label carries
        // both (the ticker animates the glyph, the frame bakes the elapsed).
        if !rows.is_empty() {
            rows.remove(0);
        }
        self.frame.live(self.step, rows);
    }

    fn paint_loading(&mut self, _elapsed: Duration, notice: LoadingNotice) {
        let rows: Vec<String> =
            loading_rows("⠋", self.label, Duration::ZERO, notice, self.feed_width)
                .into_iter()
                .skip(1)
                .collect();
        self.frame.live(self.step, rows);
    }

    fn refresh(&mut self, window: &[String], in_code_start: bool, _elapsed: Duration) {
        self.paint(window, in_code_start);
    }

    fn finish(&mut self) {
        self.frame.clear_live(self.step);
    }
}
