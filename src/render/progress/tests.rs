use super::*;
use crate::render::markdown::LineKind;

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
