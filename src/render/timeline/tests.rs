use super::*;

/// Byte-exact tests run colors-off (the [`Theme::plain`] default), matching
/// the house style of the `display`/`progress` suites — the styled paths are
/// the same segment lists with escapes applied.
const W: usize = 100;
const THEME: Theme = Theme { colors: false };

/// Pending marker + label are both muted — colors-off, so the byte contract
/// is the row's plain geometry: `  · <label>` (spec §2.3 first frame).
#[test]
fn step_line_pending_is_middle_dot_and_label() {
    let s = Step::pending("Redaction gate");
    assert_eq!(step_line(&s, W, &THEME), "  · Redaction gate");
}

/// Active marker is the first braille glyph; the live elapsed counter joins
/// the label as `· <n>s` integer seconds (spec §2.4).
#[test]
fn step_line_active_carries_spinner_and_elapsed() {
    let mut s = Step::active("Plan batches");
    assert_eq!(step_line(&s, W, &THEME), "  ⠋ Plan batches");
    s.active_elapsed = Some(Duration::from_secs(12));
    assert_eq!(step_line(&s, W, &THEME), "  ⠋ Plan batches · 12s");
}

/// Completed marker is `✓`; the summary replaces the label; a duration ≥ 1 s
/// appends ` · 6.4s`; sub-second durations show nothing (spec §1.4).
#[test]
fn step_line_completed_shows_summary_and_duration_rule() {
    let mut s = Step::pending("Plan batches");
    s.state = StepState::Completed;
    s.summary = Some("Planned 3 batches".into());
    s.duration = Some(Duration::from_millis(6400));
    assert_eq!(step_line(&s, W, &THEME), "  ✓ Planned 3 batches · 6.4s");

    s.duration = Some(Duration::from_millis(900));
    assert_eq!(step_line(&s, W, &THEME), "  ✓ Planned 3 batches");

    s.summary = None;
    s.duration = None;
    assert_eq!(step_line(&s, W, &THEME), "  ✓ Plan batches");
}

/// Failed marker is `✗`; the reason joins the label after ` — ` (spec §2.9).
#[test]
fn step_line_failed_joins_reason_after_em_dash() {
    let mut s = Step::pending("Commit batches");
    s.state = StepState::Failed;
    s.failure = Some(Failure {
        reason: "failed on batch 2 of 3".into(),
        detail: vec![],
    });
    assert_eq!(
        step_line(&s, W, &THEME),
        "  ✗ Commit batches — failed on batch 2 of 3"
    );
}

/// Duration formatting: ≥ 1 s renders one decimal; sub-second is `None`.
#[test]
fn format_duration_one_decimal_only_past_one_second() {
    assert_eq!(
        format_duration(Duration::from_millis(6400)),
        Some("6.4s".into())
    );
    assert_eq!(
        format_duration(Duration::from_secs(41)),
        Some("41.0s".into())
    );
    assert_eq!(format_duration(Duration::from_millis(999)), None);
    assert_eq!(format_duration(Duration::ZERO), None);
}

/// Rows truncate at the width budget, never wrap, and never carry trailing
/// spaces (spec §1.3) — the truncation cuts mid-content with no padding.
#[test]
fn step_line_truncates_at_width_without_trailing_spaces() {
    let s = Step::pending("Redaction gate");
    assert_eq!(step_line(&s, 10, &THEME), "  · Redact");
    // Exact fit: no trailing pad.
    assert_eq!(
        step_line(&s, "  · Redaction gate".len(), &THEME),
        "  · Redaction gate"
    );
    // CJK labels count 2 columns per char (unicode-width).
    let cjk = Step::pending("冲突检测");
    assert_eq!(step_line(&cjk, 10, &THEME), "  · 冲突检"); // 2 + 2 + 3 CJK (6 cols) = 10
}
/// The one layout rule (spec §2.1/§2.3): completed tight, active + live
/// output, one blank separator only when pending steps exist, then pending.
#[test]
fn timeline_frame_composes_completed_active_blank_pending() {
    let mut scan = Step::pending("Scan changes");
    scan.state = StepState::Completed;
    scan.summary = Some("Scanned 14 files".into());

    let mut plan = Step::active("Plan batches");
    plan.live_rows = vec!["    │ ## Splitting decode changes".into()];

    let draft = Step::pending("Draft messages");
    let commit = Step::pending("Commit batches");
    assert_eq!(
        timeline_frame(&[scan, plan, draft, commit], W, &THEME),
        vec![
            "  ✓ Scanned 14 files",
            "  ⠋ Plan batches",
            "│   │ ## Splitting decode changes",
            "│",
            "  · Draft messages",
            "  · Commit batches",
        ]
    );
}

/// No pending steps → no blank separator: the active block ends the frame.
#[test]
fn timeline_frame_omits_separator_without_pending_steps() {
    let mut scan = Step::active("Scan changes");
    scan.live_rows = vec!["    live".into()];
    assert_eq!(
        timeline_frame(&[scan], W, &THEME),
        vec!["  ⠋ Scan changes", "└   live"]
    );
}

/// A failed step ends the frame: `✗` headline, up to 3 detail rows (first
/// error, rest remediation), and every later step is absent — never rendered
/// as pending (spec §2.1/§2.9).
#[test]
fn timeline_frame_failed_step_hides_all_later_steps() {
    let mut scan = Step::pending("Scan changes");
    scan.state = StepState::Completed;
    scan.summary = Some("Scanned 14 files".into());

    let mut gate = Step::pending("Redaction gate");
    gate.state = StepState::Failed;
    gate.failure = Some(Failure {
        reason: "secret-like content detected".into(),
        detail: vec![
            "src/render/config.rs: hunk 2 looks like an API key; use --no-redact to bypass".into(),
            "row 2".into(),
            "row 3".into(),
            "row 4 must be dropped".into(),
        ],
    });
    let plan = Step::pending("Plan batches");
    assert_eq!(
        timeline_frame(&[scan, gate, plan], W, &THEME),
        vec![
            "  ✓ Scanned 14 files",
            "  ✗ Redaction gate — secret-like content detected",
            "│   src/render/config.rs: hunk 2 looks like an API key; use --no-redact to bypass",
            "│   row 2",
            "└   row 3",
        ]
    );
}

/// Durable rows persist under a completed step's summary and keep one blank
/// row after the block before the next step line (spec §1.3/§2.7) — but no
/// trailing blank when the block ends the frame.
#[test]
fn timeline_frame_durable_block_gets_one_blank_before_next_step() {
    let mut commit = Step::pending("Commit batches");
    commit.state = StepState::Completed;
    commit.summary = Some("Committed 1 batch".into());
    commit.durable_rows = vec!["    ✓ a1b2c3d feat: thing".into()];

    let tail = Step::pending("Nothing");
    assert_eq!(
        timeline_frame(&[commit.clone(), tail], W, &THEME),
        vec![
            "  ✓ Committed 1 batch",
            "│   ✓ a1b2c3d feat: thing",
            "│",
            "  · Nothing",
        ]
    );
    assert_eq!(
        timeline_frame(&[commit], W, &THEME),
        vec!["  ✓ Committed 1 batch", "└   ✓ a1b2c3d feat: thing"]
    );
}

/// Sub-rows sit at column 4; the `[i/N]` prefix pads to `[N/N]` width; a done
/// sub-row keeps its duration only past 1 s (spec §2.6/§2.8).
#[test]
fn sub_row_pads_prefix_and_shows_done_duration() {
    assert_eq!(
        sub_row(
            1,
            3,
            &SubBody::InFlight("Generating commit message".into()),
            W,
            &THEME
        ),
        "    [1/3] ⠋ Generating commit message"
    );
    assert_eq!(
        sub_row(
            2,
            3,
            &SubBody::Done(String::new(), Some(Duration::from_millis(6200))),
            W,
            &THEME
        ),
        "    [2/3] ✓ 6.2s"
    );
    assert_eq!(
        sub_row(
            2,
            3,
            &SubBody::Done(String::new(), Some(Duration::from_millis(400))),
            W,
            &THEME
        ),
        "    [2/3] ✓"
    );
    assert_eq!(
        sub_row(
            1,
            3,
            &SubBody::Done(
                "src/render/progress.rs".into(),
                Some(Duration::from_millis(18100))
            ),
            W,
            &THEME
        ),
        "    [1/3] ✓ src/render/progress.rs · 18.1s"
    );
    // Two-digit totals keep single-digit indices aligned.
    assert_eq!(
        sub_row(
            3,
            12,
            &SubBody::InFlight("Retrying docs/adr/0018.md".into()),
            W,
            &THEME
        ),
        "    [3/12]  ⠋ Retrying docs/adr/0018.md"
    );
}

/// A bare menu: `› ` on the selected option, two-space indent on the rest;
/// the returned index points at the cursor row (spec §1.1/§2.5).
#[test]
fn menu_rows_cursor_marks_selected_option() {
    let (rows, cursor) = menu_rows(&["Commit", "Re-generate", "Edit", "Abort"], 0, W, &THEME);
    assert_eq!(cursor, 0);
    assert_eq!(
        rows,
        vec![
            "    › Commit",
            "      Re-generate",
            "      Edit",
            "      Abort",
        ]
    );
    let (rows, cursor) = menu_rows(&["Continue", "Merge two batches…", "Abort"], 1, W, &THEME);
    assert_eq!(cursor, 1);
    assert_eq!(
        rows,
        vec!["      Continue", "    › Merge two batches…", "      Abort"]
    );
}

/// The Batch Review menu: `?` ask headline, `#i` listing rows, then the
/// options menu — cursor index addresses the assembled rows (spec §2.5).
#[test]
fn review_menu_rows_compose_ask_listing_and_menu() {
    let entries = vec![
        "#1 src/render/progress.rs (hunks 1-4) — SameContext".to_string(),
        "#2 src/render/display.rs  (hunks 1-2) — Adjacency".to_string(),
    ];
    let (rows, cursor) = review_menu_rows(
        "batch plan — 2 batches",
        &entries,
        &["Continue", "Abort"],
        0,
        W,
        &THEME,
    );
    assert_eq!(cursor, 3, "ask + 2 entries + first option");
    assert_eq!(
        rows,
        vec![
            "    ? batch plan — 2 batches",
            "      #1 src/render/progress.rs (hunks 1-4) — SameContext",
            "      #2 src/render/display.rs  (hunks 1-2) — Adjacency",
            "    › Continue",
            "      Abort",
        ]
    );
}

/// The one-row y/n ask: `? ` + question, at the sub-row column (spec §2.8).
#[test]
fn ask_row_is_question_marker_and_text() {
    assert_eq!(
        ask_row("resolve now? [Y/n]", W, &THEME),
        "    ? resolve now? [Y/n]"
    );
}

/// The non-TTY ledger (spec §5): pending/active never print; completed
/// prints the same summary string as the styled path; failed prints headline
/// plus detail, capped at 3.
#[test]
fn plain_step_lines_print_only_completed_and_failed() {
    assert!(plain_step_lines(&Step::pending("Draft messages")).is_empty());
    assert!(plain_step_lines(&Step::active("Plan batches")).is_empty());

    let mut done = Step::pending("Plan batches");
    done.state = StepState::Completed;
    done.summary = Some("Planned 3 batches".into());
    done.duration = Some(Duration::from_millis(6400));
    assert_eq!(plain_step_lines(&done), vec!["✓ Planned 3 batches · 6.4s"]);

    let mut failed = Step::pending("Commit batches");
    failed.state = StepState::Failed;
    failed.failure = Some(Failure {
        reason: "aborted on batch 2 of 3".into(),
        detail: vec!["declined".into(), "1 batch committed; re-run aic".into()],
    });
    assert_eq!(
        plain_step_lines(&failed),
        vec![
            "✗ Commit batches — aborted on batch 2 of 3",
            "declined",
            "1 batch committed; re-run aic",
        ]
    );
}

/// Height budget (spec §2.4): min of the window cap and the terminal's spare
/// rows; below 3 rows the window is suppressed (0).
#[test]
fn window_height_budget_caps_and_suppresses_below_three() {
    // Plenty of room: the max_rows cap wins.
    assert_eq!(window_height_budget(12, 40, 10), 12);
    // Terminal tighter than the cap: height − fixed − 2.
    assert_eq!(window_height_budget(12, 20, 10), 8);
    // Exactly 3 survives; 2 suppresses.
    assert_eq!(window_height_budget(12, 20, 15), 3);
    assert_eq!(window_height_budget(12, 20, 16), 0);
    // Degenerate: saturation keeps it 0, never a panic or underflow.
    assert_eq!(window_height_budget(12, 2, 10), 0);
    assert_eq!(window_height_budget(40, 100, 0), 40);
}
