use super::*;
use crate::git::FileStats;

/// Byte-exact tests run colors-off, matching the house style of the
/// `display`/`progress` suites — the styled paths are the same segment
/// lists with escapes applied.
const W: usize = 100;

/// The shared colors-off theme at the suite's default width budget.
const THEME: Theme = Theme {
    colors: false,
    width: W,
};

/// A colors-off theme at a specific width budget (the truncation tests).
fn theme(w: usize) -> Theme {
    Theme {
        colors: false,
        width: w,
    }
}

/// Pending marker + label are both muted — colors-off, so the byte contract
/// is the row's plain geometry: `  · <label>` (ADR 0018 first frame).
#[test]
fn step_line_pending_is_middle_dot_and_label() {
    let s = Step::pending("Redaction gate");
    assert_eq!(step_line(&s, &THEME), "  · Redaction gate");
}

/// Active marker is the first braille glyph; the live elapsed counter joins
/// the label as `· <n>s` integer seconds (ADR 0018).
#[test]
fn step_line_active_carries_spinner_and_elapsed() {
    let mut s = Step::active("Plan batches");
    assert_eq!(step_line(&s, &THEME), "  ⠋ Plan batches");
    s.active_elapsed = Some(Duration::from_secs(12));
    assert_eq!(step_line(&s, &THEME), "  ⠋ Plan batches · 12s");
}

/// Completed marker is `✓`; the summary replaces the label; a duration ≥ 1 s
/// appends ` · 6.4s`; sub-second durations show nothing (ADR 0018).
#[test]
fn step_line_completed_shows_summary_and_duration_rule() {
    let mut s = Step::completed("Plan batches", "Planned 3 batches");
    s.duration = Some(Duration::from_millis(6400));
    assert_eq!(step_line(&s, &THEME), "  ✓ Planned 3 batches · 6.4s");

    s.duration = Some(Duration::from_millis(900));
    assert_eq!(step_line(&s, &THEME), "  ✓ Planned 3 batches");

    s.summary = None;
    s.duration = None;
    assert_eq!(step_line(&s, &THEME), "  ✓ Plan batches");
}

/// Failed marker is `✗`; the reason joins the label after ` — ` (ADR 0018).
#[test]
fn step_line_failed_joins_reason_after_em_dash() {
    let s = Step::failed("Commit batches", "failed on batch 2 of 3", Vec::new());
    assert_eq!(
        step_line(&s, &THEME),
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
/// spaces (ADR 0018) — the truncation cuts mid-content with no padding.
#[test]
fn step_line_truncates_at_width_without_trailing_spaces() {
    let s = Step::pending("Redaction gate");
    assert_eq!(step_line(&s, &theme(10)), "  · Redact");
    // Exact fit: no trailing pad.
    assert_eq!(
        step_line(&s, &theme("  · Redaction gate".len())),
        "  · Redaction gate"
    );
    // CJK labels count 2 columns per char (unicode-width).
    let cjk = Step::pending("冲突检测");
    assert_eq!(step_line(&cjk, &theme(10)), "  · 冲突检"); // 2 + 2 + 3 CJK (6 cols) = 10
}

/// The one layout rule (ADR 0018): completed tight, active + live
/// output, one blank separator only when pending steps exist, then pending.
#[test]
fn timeline_frame_composes_completed_active_blank_pending() {
    let scan = Step::completed("Scan changes", "Scanned 14 files");

    let mut plan = Step::active("Plan batches");
    plan.live_rows = vec!["    │ ## Splitting decode changes".into()];

    let draft = Step::pending("Draft messages");
    let commit = Step::pending("Commit batches");
    assert_eq!(
        timeline_frame(&[scan, plan, draft, commit], &THEME),
        vec![
            "  ✓ Scanned 14 files",
            "  ⠋ Plan batches",
            "    │ ## Splitting decode changes",
            "",
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
        timeline_frame(&[scan], &THEME),
        vec!["  ⠋ Scan changes", "    live"]
    );
}

/// A failed step ends the frame: `✗` headline, up to 3 detail rows (first
/// error, rest remediation), and every later step is absent — never rendered
/// as pending (ADR 0018).
#[test]
fn timeline_frame_failed_step_hides_all_later_steps() {
    let scan = Step::completed("Scan changes", "Scanned 14 files");

    let gate = Step::failed(
        "Redaction gate",
        "secret-like content detected",
        vec![
            "src/render/config.rs: hunk 2 looks like an API key; use --no-redact to bypass".into(),
            "row 2".into(),
            "row 3".into(),
            "row 4 must be dropped".into(),
        ],
    );
    let plan = Step::pending("Plan batches");
    assert_eq!(
        timeline_frame(&[scan, gate, plan], &THEME),
        vec![
            "  ✓ Scanned 14 files",
            "  ✗ Redaction gate — secret-like content detected",
            "    src/render/config.rs: hunk 2 looks like an API key; use --no-redact to bypass",
            "    row 2",
            "    row 3",
        ]
    );
}

/// Durable rows persist under a completed step's summary and keep one blank
/// row after the block before the next step line (ADR 0018) — but no
/// trailing blank when the block ends the frame.
#[test]
fn timeline_frame_durable_block_gets_one_blank_before_next_step() {
    let mut commit = Step::completed("Commit batches", "Committed 1 batch");
    commit.durable_rows = vec!["    ✓ a1b2c3d feat: thing".into()];

    let tail = Step::pending("Nothing");
    assert_eq!(
        timeline_frame(&[commit.clone(), tail], &THEME),
        vec![
            "  ✓ Committed 1 batch",
            "    ✓ a1b2c3d feat: thing",
            "",
            "  · Nothing",
        ]
    );
    assert_eq!(
        timeline_frame(&[commit], &THEME),
        vec!["  ✓ Committed 1 batch", "    ✓ a1b2c3d feat: thing"]
    );
}

/// Sub-rows sit at column 4 with no `[i/N]` numbering — every row of the
/// step shares one shape; a done sub-row keeps its duration only past 1 s
/// (ADR 0018). Width 80 fits all of these unchanged.
#[test]
fn sub_row_shows_done_duration() {
    assert_eq!(
        sub_row(
            &SubBody::InFlight("Generating commit message".into()),
            80,
            &THEME
        ),
        "    ⠋ Generating commit message"
    );
    assert_eq!(
        sub_row(
            &SubBody::Done(String::new(), Some(Duration::from_millis(6200))),
            80,
            &THEME
        ),
        "    ✓ 6.2s"
    );
    assert_eq!(
        sub_row(
            &SubBody::Done(String::new(), Some(Duration::from_millis(400))),
            80,
            &THEME
        ),
        "    ✓"
    );
    assert_eq!(
        sub_row(
            &SubBody::Done(
                "src/render/progress.rs".into(),
                Some(Duration::from_millis(18100))
            ),
            80,
            &THEME
        ),
        "    ✓ src/render/progress.rs · 18.1s"
    );
    assert_eq!(
        sub_row(
            &SubBody::InFlight("Retrying docs/adr/0018.md".into()),
            80,
            &THEME
        ),
        "    ⠋ Retrying docs/adr/0018.md"
    );
}

/// A landed commit's echo shares the durable entry's grammar — hash then
/// subject — so the live row and the flushed row read identically
/// (colors-off: the styled test theme emits no ANSI).
#[test]
fn sub_row_landed_echoes_hash_and_styled_subject() {
    assert_eq!(
        sub_row(
            &SubBody::Landed {
                hash: "a1b2c3d".into(),
                subject: "fix(auth): correct token check".into()
            },
            80,
            &THEME
        ),
        "    ✓ a1b2c3d fix(auth): correct token check"
    );
    // Colon-less messages fall back to the full text, like the entry row.
    assert_eq!(
        sub_row(
            &SubBody::Landed {
                hash: "a1b2c3d".into(),
                subject: "plain message".into()
            },
            80,
            &THEME
        ),
        "    ✓ a1b2c3d plain message"
    );
}

/// The landed echo clamps like every other sub-row: the subject is LLM
/// output with no builder-side budget (prompt-capped at 72 chars, nothing
/// validates that), and an unclamped row would wrap and desync the
/// painter's whole-frame repaint on every later tick.
#[test]
fn sub_row_clamps_long_landed_subject_to_the_terminal_width() {
    let subject = format!("feat: {}", "x".repeat(70));
    let row = sub_row(
        &SubBody::Landed {
            hash: "a1b2c3d".into(),
            subject,
        },
        40,
        &THEME,
    );
    // Budget 26 (40 − inset 6 − hash 7 − joiner 1): `feat: ` + 19 x's +
    // `…` — the whole row is exactly 40 columns.
    assert_eq!(row, format!("    ✓ a1b2c3d feat: {}…", "x".repeat(19)));
}

/// The active row's live elapsed joins only from 1 s — mirroring the
/// completed state's rule — so a fast step never flashes `· 0s`.
#[test]
fn step_line_active_hides_sub_second_elapsed() {
    let s = Step::active("Plan batches");
    let s = Step {
        active_elapsed: Some(Duration::from_millis(400)),
        ..s
    };
    assert_eq!(step_line(&s, &THEME), "  ⠋ Plan batches");
}

/// A live sub-row clamps its text to the terminal width (`…` marking the
/// cut, duration suffix kept inside the budget): sub-row text is the one
/// frame content with no builder-side budget — commit subjects are
/// prompt-capped at 72 chars but nothing validates that — and a row that
/// wraps to two physical rows desyncs the painter's whole-frame repaint
/// on every later tick.
#[test]
fn sub_row_clamps_long_done_text_to_the_terminal_width() {
    let subject = "f".repeat(60);
    let row = sub_row(
        &SubBody::Done(
            format!("abc1234 {subject}"),
            Some(Duration::from_millis(6200)),
        ),
        40,
        &THEME,
    );
    // Budget 27 (40 − inset 6 − ` · 6.2s` 7): `abc1234 ` + 18 f's + `…`;
    // the whole row is exactly 40 columns.
    assert_eq!(row, format!("    ✓ abc1234 {}… · 6.2s", "f".repeat(18)));
}

/// CJK labels count two columns per char — the clamp keeps whole chars
/// and never splits a fullwidth glyph across the budget.
#[test]
fn sub_row_clamps_cjk_labels_by_display_width() {
    let label = "重".repeat(30);
    let row = sub_row(&SubBody::InFlight(label), 20, &THEME);
    // Budget 14 (20 − 6): six fullwidth chars (12) + `…` = 13 columns.
    assert_eq!(row, format!("    ⠋ {}…", "重".repeat(6)));
}

/// A bare menu: `› ` on the selected option, two-space indent on the rest;
/// the returned index points at the cursor row (ADR 0018).
#[test]
fn menu_rows_cursor_marks_selected_option() {
    let (rows, cursor) = menu_rows(&["Commit", "Re-generate", "Edit", "Abort"], 0, &THEME);
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
    let (rows, cursor) = menu_rows(&["Continue", "Merge two batches…", "Abort"], 1, &THEME);
    assert_eq!(cursor, 1);
    assert_eq!(
        rows,
        vec!["      Continue", "    › Merge two batches…", "      Abort"]
    );
}

/// Menu options and ask rows ride the live frame, so they clamp to the
/// theme's width budget — a batch label built from deep repo paths can
/// exceed the terminal, and a wrapped live row desyncs the painter's
/// whole-frame repaint on every later tick.
#[test]
fn menu_rows_and_ask_row_clamp_to_the_frame_width() {
    let t = theme(40);
    let (rows, cursor) = menu_rows(&["Abort", &"o".repeat(100)], 1, &t);
    assert_eq!(cursor, 1);
    // Budget 34 (40 − inset 6): the unselected row indents, the selected
    // one carries the `› ` cursor; both rows are exactly 40 columns.
    assert_eq!(rows[0], "      Abort");
    assert_eq!(rows[1], format!("    › {}…", "o".repeat(33)));
    let ask = ask_row(&"q".repeat(100), &t);
    assert_eq!(ask, format!("    ? {}…", "q".repeat(33)));
}

/// The Batch Review menu: `?` ask headline, `#i` listing rows, then the
/// options menu — cursor index addresses the assembled rows (ADR 0018).
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

/// The one-row y/n ask: `? ` + question, at the sub-row column (ADR 0018).
#[test]
fn ask_row_is_question_marker_and_text() {
    assert_eq!(
        ask_row("resolve now? [Y/n]", &THEME),
        "    ? resolve now? [Y/n]"
    );
}

/// The frame menu's help row: muted key hints at the sub-row column, so the
/// frame path shows the same text the inquire fallback carries as its help
/// message.
#[test]
fn help_row_is_muted_hints_at_sub_indent() {
    assert_eq!(
        help_row("↑/↓ select · Enter confirm · Esc or Ctrl-C aborts", &THEME),
        "    ↑/↓ select · Enter confirm · Esc or Ctrl-C aborts"
    );
}

/// The non-TTY ledger (ADR 0018): pending/active never print; completed
/// prints the same summary string as the styled path; failed prints headline
/// plus detail, capped at 3.
#[test]
fn plain_step_lines_print_only_completed_and_failed() {
    assert!(plain_step_lines(&Step::pending("Draft messages")).is_empty());
    assert!(plain_step_lines(&Step::active("Plan batches")).is_empty());

    let mut done = Step::completed("Plan batches", "Planned 3 batches");
    done.duration = Some(Duration::from_millis(6400));
    assert_eq!(plain_step_lines(&done), vec!["✓ Planned 3 batches · 6.4s"]);

    let failed = Step::failed(
        "Commit batches",
        "aborted on batch 2 of 3",
        vec!["declined".into(), "1 batch committed; re-run aic".into()],
    );
    assert_eq!(
        plain_step_lines(&failed),
        vec![
            "✗ Commit batches — aborted on batch 2 of 3",
            "declined",
            "1 batch committed; re-run aic",
        ]
    );
}

/// Height budget (ADR 0018): min of the window cap and the terminal's spare
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

/// `compact_count` keeps exact numbers below a thousand and collapses
/// above it: one decimal while the whole part is a single digit, no
/// decimal once it isn't, `k`→`m` at the next magnitude.
#[test]
fn compact_count_collapses_only_above_a_thousand() {
    assert_eq!(compact_count(0), "0");
    assert_eq!(compact_count(999), "999");
    assert_eq!(compact_count(1_000), "1k");
    assert_eq!(compact_count(2_100), "2.1k");
    assert_eq!(compact_count(9_999), "10k");
    assert_eq!(compact_count(15_000), "15k");
    assert_eq!(compact_count(999_949), "999k");
    assert_eq!(compact_count(999_950), "1m");
    assert_eq!(compact_count(999_999), "1m");
}

/// `delta_suffix` sums per-file stats into one ` · +A −D` segment and
/// stays present at zero (binary-only scans still show the churn pair).
#[test]
fn delta_suffix_sums_and_survives_zero() {
    let stats = |a: usize, d: usize| FileStats {
        path: "x.rs".into(),
        added: a,
        deleted: d,
        new: false,
        removed: false,
        binary: false,
        old_size: 0,
        new_size: 0,
    };
    assert_eq!(delta_suffix(&[stats(3, 1), stats(5, 0)]), " · +8 −1");
    assert_eq!(delta_suffix(&[]), " · +0 −0");
}

/// `menu_frame_rows` is the single assembly `frame_menu` paints per key
/// read (ADR 0018): the original menu bug built ask/options/help rows
/// every loop but never handed them to the painter — every interactive
/// menu rendered zero rows on a TTY. e2e's non-TTY buffer cannot see
/// frame painting, so this pins the painted row set itself: the ask row
/// leads, the `›` cursor marks the selection, the help row closes, and
/// every row clamps to the frame width with `…` (the no-wrap contract).
#[test]
fn menu_frame_rows_compose_ask_options_and_help_in_order() {
    let rows = menu_frame_rows(
        Some("stage this hunk?"),
        &["Commit", "Abort"],
        0,
        "↑/↓ select · Enter confirm · Esc aborts",
        &THEME,
    );
    assert_eq!(rows.len(), 4, "{rows:?}");
    assert_eq!(rows[0], "    ? stage this hunk?");
    assert_eq!(rows[1], "    › Commit");
    assert_eq!(rows[2], "      Abort");
    assert_eq!(rows[3], "    ↑/↓ select · Enter confirm · Esc aborts");
}

/// Without an ask the assembly is options + help only — the merge-pick
/// prompts pass an ask, the confirm menu does not.
#[test]
fn menu_frame_rows_without_ask_skip_the_ask_row() {
    let rows = menu_frame_rows(None, &["Commit", "Abort"], 1, "Esc aborts", &THEME);
    assert_eq!(rows.len(), 3, "{rows:?}");
    assert!(rows[0].contains("Commit") && !rows[0].contains('›'));
    assert_eq!(rows[1], "    › Abort");
}

/// Every row of the assembly honors the no-wrap budget: at width 20 each
/// row (ask, option, help) is at most 20 columns and the cut carries `…`.
#[test]
fn menu_frame_rows_clamp_every_row_to_the_frame_width() {
    let long: String = "x".repeat(80);
    let t = theme(20);
    let rows = menu_frame_rows(Some(&long), &[&long], 0, &long, &t);
    for row in &rows {
        assert!(display_width(row) <= 20, "{row:?}");
        assert!(row.ends_with('…'), "the cut must be marked: {row:?}");
    }
}
