//! The Run's reasoning-feed display driver.
//!
//! Owns the *when-to-paint* policy that sits between a streaming LLM call and a
//! [`progress::ReasoningRenderer`] sink: a rolling reasoning window redraws in
//! place as the model thinks and is erased when thinking ends, so nothing
//! lingers in the scrollback. When the backend emits no reasoning deltas at all
//! (a CLI agent in single-shot print mode, or an API cold start), a loading
//! frame keeps the screen alive on a [`progress::SPINNER_TICK`] cadence —
//! spinner + elapsed counter — and, past [`progress::LOADING_GRACE`], an
//! explanatory notice.
//!
//! This module is the *policy* (which frame, on what event); the sink
//! ([`ReasoningSink`], concretely [`progress::ReasoningRenderer`] in
//! production) owns the *rendering* (what bytes to emit). The trait seam lets
//! the loop be driven against a recording fake in tests, while the byte-level
//! row/frame assembly stays unit-tested in `progress`.
//!
//! **Why a channel.** The streaming future owns the [`progress::ThinkingView`]
//! inside its `on_reasoning` callback; reasoning windows are forwarded over an
//! unbounded channel to the repaint loop rather than rendered inline, so the
//! future (writer) and the loop (renderer) never borrow-conflict.
//!
//! **Why biased completion.** The completion arm is polled first: a fast
//! backend that returns before any repaint tick paints nothing at all.

use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::progress::{
    LOADING_GRACE, SPINNER_TICK, LoadingNotice, ThinkingView,
};
use crate::BoxFuture;

/// What the driver paints to: one frame per content delta or loading tick, and
/// a final dissolve on success. The production impl is
/// [`progress::ReasoningRenderer`]; tests inject a recording fake so the loop's
/// frame-policy decisions are assertable without a terminal.
pub(crate) trait ReasoningSink: Send {
    /// Paint one frame for a content delta (spinner frozen, only the in-progress
    /// line grows).
    fn paint(&mut self, window: &[String], in_code_start: bool);
    /// Paint one loading frame for the silent/cold-start state.
    fn paint_loading(&mut self, elapsed: Duration, notice: LoadingNotice);
    /// Idle-tick repaint: advance the spinner and elapsed once the model goes
    /// silent past the active threshold.
    fn refresh(&mut self, window: &[String], in_code_start: bool, elapsed: Duration);
    /// Dissolve the frame row-by-row and restore the cursor. Called once on the
    /// success path; the prod sink's `Drop` is a fast-erase backstop for an
    /// aborted stream.
    fn finish(&mut self);
}

/// The `on_reasoning` callback handed to a streaming LLM call: the driver wires
/// it to its internal [`ThinkingView`] + channel, so each delta flows into the
/// repaint loop without the caller knowing about either.
pub(crate) type ReasoningTap = Box<dyn FnMut(&str) + Send>;

/// Which loading notice (if any) to show, given the time since stream start and
/// whether the backend is a streaming-capable cold start. A pure extraction of
/// the decision the loop used to inline — unit-tested without a clock.
///
/// Past [`LOADING_GRACE`] a streaming-capable backend gets a cold-start notice
/// (it *is* streaming, just not reasoning yet); a plain backend gets the
/// non-streaming notice. Under the grace, no notice at all.
pub(crate) fn loading_notice(elapsed: Duration, cold_start: Option<&str>) -> LoadingNotice {
    if elapsed >= LOADING_GRACE {
        match cold_start {
            Some(program) => LoadingNotice::ColdStart(program.to_string()),
            None => LoadingNotice::Silent,
        }
    } else {
        LoadingNotice::None
    }
}

/// Drive `make_call` behind the reasoning feed, painting frames into `sink`.
/// `make_call` receives the [`ReasoningTap`] and forwards it into its streaming
/// generator; the tap pushes reasoning windows into the driver's channel, and
/// this loop paints them — loading frames before the first delta, content
/// frames on each delta, and a final [`ReasoningSink::finish`].
///
/// `max_rows` is the rendered-row budget from the caller's terminal geometry
/// probe (reused as the [`ThinkingView`] line cap); `cold_start` labels the
/// past-grace notice for a streaming-capable backend. Both are inert inputs —
/// the driver does no config or terminal I/O of its own.
pub(crate) async fn run<S, F, T>(
    sink: &mut S,
    max_rows: usize,
    cold_start: Option<&str>,
    make_call: F,
) -> anyhow::Result<T>
where
    S: ReasoningSink,
    F: FnOnce(ReasoningTap) -> BoxFuture<anyhow::Result<T>>,
{
    let (tx, mut rx) = mpsc::unbounded_channel::<(Vec<String>, bool)>();
    let mut view = ThinkingView::new(max_rows);
    let tap: ReasoningTap = Box::new(move |delta: &str| {
        let (window, in_code_start) = view.push(delta);
        // Send only fails if the receiver was dropped — which happens after the
        // stream completes, when pending windows no longer matter.
        let _ = tx.send((window, in_code_start));
    });

    let fut = make_call(tap);
    tokio::pin!(fut);

    let start = Instant::now();
    let mut got_output = false;
    let mut last_window: Vec<String> = Vec::new();
    let mut last_in_code_start = false;
    // The interval's first tick fires immediately — wanted, so the first frame
    // reflects a real (even 0 s) elapsed, giving sub-second feedback before any
    // LLM round-trip.
    let mut ticker = tokio::time::interval(SPINNER_TICK);

    let result = loop {
        tokio::select! {
            biased;
            // Completion wins over everything: a fast backend paints nothing.
            res = &mut fut => {
                break res;
            }
            // A reasoning delta hands the rolling window to the sink and latches
            // `got_output` so no later tick repaints a loading frame over the
            // feed.
            window = rx.recv() => {
                if let Some((window, in_code_start)) = window {
                    got_output = true;
                    last_window = window.clone();
                    last_in_code_start = in_code_start;
                    sink.paint(&window, in_code_start);
                }
            }
            // Steady tick: before the first delta, the loading frame keeps the
            // screen alive; after it, repaint the retained window so the spinner
            // keeps animating while the model is silent between deltas.
            _ = ticker.tick() => {
                let elapsed = start.elapsed();
                if !got_output {
                    sink.paint_loading(elapsed, loading_notice(elapsed, cold_start));
                } else {
                    sink.refresh(&last_window, last_in_code_start, elapsed);
                }
            }
        }
    };

    sink.finish();
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// Records every sink call as a short string, so the loop's frame-policy
    /// decisions are assertable without a terminal.
    #[derive(Default, Clone)]
    struct FakeSink(Arc<Mutex<Vec<String>>>);

    impl ReasoningSink for FakeSink {
        fn paint(&mut self, window: &[String], _: bool) {
            self.0.lock().push(format!("paint:{}", window.len()));
        }
        fn paint_loading(&mut self, elapsed: Duration, notice: LoadingNotice) {
            self.0.lock().push(format!("loading:{elapsed:?}/{notice:?}"));
        }
        fn refresh(&mut self, _: &[String], _: bool, elapsed: Duration) {
            self.0.lock().push(format!("refresh:{elapsed:?}"));
        }
        fn finish(&mut self) {
            self.0.lock().push("finish".into());
        }
    }

    fn events(sink: &FakeSink) -> Vec<String> {
        sink.0.lock().clone()
    }

    #[test]
    fn loading_notice_none_under_grace() {
        assert_eq!(
            loading_notice(Duration::from_secs(1), None),
            LoadingNotice::None
        );
        assert_eq!(
            loading_notice(Duration::from_secs(1), Some("claude")),
            LoadingNotice::None
        );
    }

    #[test]
    fn loading_notice_silent_past_grace_without_cold_start() {
        assert_eq!(
            loading_notice(Duration::from_secs(6), None),
            LoadingNotice::Silent
        );
    }

    #[test]
    fn loading_notice_cold_start_past_grace() {
        assert_eq!(
            loading_notice(Duration::from_secs(6), Some("claude")),
            LoadingNotice::ColdStart("claude".to_string())
        );
    }

    #[tokio::test]
    async fn run_paints_on_delta_then_finishes() {
        let sink = FakeSink::default();
        let record = sink.0.clone();
        let mut sink = sink;

        // The tap fires once, then yields so the repaint loop processes the
        // channel before completion.
        let res: anyhow::Result<i32> = run(&mut sink, 10, None, |mut tap| {
            Box::pin(async move {
                tap("thinking about the diff\n");
                tokio::task::yield_now().await;
                Ok(7)
            })
        })
        .await;

        assert_eq!(res.unwrap(), 7);
        let e = events(&FakeSink(record));
        assert!(e.contains(&"paint:1".to_string()), "missing paint: {e:?}");
        assert_eq!(e.last().unwrap(), "finish");
    }

    #[tokio::test]
    async fn run_finishes_even_when_backend_is_silent() {
        let sink = FakeSink::default();
        let record = sink.0.clone();
        let mut sink = sink;

        // No reasoning delta at all — a non-streaming backend that returns whole.
        let res: anyhow::Result<i32> = run(&mut sink, 10, None, |_tap| {
            Box::pin(async move { Ok(3) })
        })
        .await;

        assert_eq!(res.unwrap(), 3);
        let e = events(&FakeSink(record));
        assert_eq!(e.last().unwrap(), "finish");
        assert!(!e.iter().any(|s| s.starts_with("paint")), "no paint for silent backend: {e:?}");
    }
}
