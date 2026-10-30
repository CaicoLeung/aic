//! Batch Review (issue #157): the opt-in gate that interrupts the Run
//! between batch-plan validation and Batch drafting. Owns the gate itself
//! ([`BatchReview`]), the menu seam, the production terminal menu, and the
//! review loop that merges Batches until the user continues or aborts. The
//! menu shows each Batch's files, hunk ranges, and reason; a merge folds two
//! Batches into one and re-validates the plan as an exact hunk partition
//! before the menu re-renders; an undo restores the pre-merge plan (the
//! go-back the commit-confirmation menu pattern calls for). Abort ends the
//! Run before anything is staged or drafted — the cheapest exit in the
//! whole workflow.
//!
//! `review_plan` borrows the workflow's [`Display`] rather than owning it:
//! Batch Review is a phase of the commit Run, not a standalone service (same
//! shape as `confirm_draft`). See CONTEXT.md "Batch Review".

use std::collections::HashMap;

use crate::llm::generator::{self, BatchPlanBatch, BatchPlanOutput};
use crate::render::display::Display;
use crate::render::palette;
use crate::render::timeline::driver::RunFrame;
use crate::render::timeline::{Failure, StepIdx, plural};
use crate::workflow::input::{select_in_frame, truncate};
use anyhow::Context;
use console::Style;

/// One decision from the Batch Review menu (issue #157). Continue proceeds
/// to drafting with the plan as shown; Abort ends the Run with nothing
/// staged and nothing committed; Merge folds Batches `a` and `b` (0-based)
/// into one and returns to the menu with the plan re-rendered; UndoMerge
/// restores the plan as it stood before the last merge — without it, a
/// mistaken merge could only be undone by aborting the whole Run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewChoice {
    Continue,
    Abort,
    Merge(usize, usize),
    UndoMerge,
}

/// Erased Batch Review menu: given the current Batches and whether a merge
/// is undoable, returns the user's decision. Boxed for the same reason as
/// the confirm-menu seam — production wires it to a terminal menu, tests
/// inject a scripted decision sequence.
pub(crate) type ReviewMenu = Box<dyn Fn(&[BatchPlanBatch], bool) -> anyhow::Result<ReviewChoice>>;

/// Opt-in Batch Review (issue #157): the gate plus the menu seam it needs.
/// [`BatchReview::Disabled`] is the default — no menu, the plan drafts
/// byte-for-byte as before the option existed. Modeled as an enum so the
/// disabled variant carries no dead closures; tests build
/// [`BatchReview::Interactive`] directly with scripted seams, production
/// uses [`BatchReview::interactive`].
pub(crate) enum BatchReview {
    /// Review off — the validated plan goes straight to drafting.
    Disabled,
    /// Review on, wired to the production menu.
    Interactive {
        /// Current Batches → user decision (Continue / Abort / Merge).
        menu: ReviewMenu,
    },
}

impl BatchReview {
    /// Production Batch Review: the terminal [`review_menu`]. The production
    /// constructor stays private to this module; tests inject their own
    /// seams by building [`BatchReview::Interactive`] directly.
    pub(crate) fn interactive() -> Self {
        Self::Interactive {
            menu: Box::new(review_menu),
        }
    }
}

// ----------------------------------------------------------------------
// The review loop
// ----------------------------------------------------------------------

/// Run the Batch Review loop (issue #157): paint the plan, let the user
/// merge Batches — and undo merges — until satisfied, and return the
/// re-validated plan for drafting. A pass-through only when the gate is
/// off; otherwise the menu lists any plan it is given — a single-Batch
/// plan simply offers Continue / Abort (nothing is mergeable, but the plan
/// is still shown and abortable before any drafting is paid for).
///
/// The listing and menu are the Review step's live output (ADR 0018): on a
/// terminal they render as frame rows under the active step and every
/// merge/undo re-renders the same frame in place; off a terminal the
/// listing prints and erases exactly as before the timeline. Continue
/// completes the step (`✓ Plan approved · N batches`); Abort is the failed
/// screen (`✗ Review plan — aborted by user` plus the nothing-staged hint).
///
/// Each merge re-runs `validate_batch_plan`: merging two members of a valid
/// partition is always still a valid partition, so the check is defensive
/// only — but it guards the future plan-model extensions that will edit
/// plans in richer ways than a merge. Undo pops the last pre-merge
/// snapshot, which was valid when it was pushed, so no re-validation is
/// needed. Abort bails with the same clean wording as the staged
/// single-commit path's decline: nothing has been staged or drafted, so
/// there is nothing to unwind.
pub(crate) fn review_plan(
    mut plan: BatchPlanOutput,
    snapshot: &crate::workflow::run::WorkdirSnapshot,
    display: &Display,
    review: &BatchReview,
    frame: &RunFrame,
    step: StepIdx,
) -> anyhow::Result<BatchPlanOutput> {
    let BatchReview::Interactive { menu } = review else {
        return Ok(plan);
    };
    let mut history: Vec<BatchPlanOutput> = Vec::new();
    let mut merges = 0usize;
    let step_display = frame.step_display(display);

    loop {
        let rows = paint_plan(&step_display, &plan, snapshot.hunk_counts());
        match menu(&plan.batches, !history.is_empty())? {
            ReviewChoice::Continue => {
                step_display.clear_last(rows);
                let n = plan.batches.len();
                let summary = if merges > 0 {
                    format!("Plan approved · {} · {merges} merged", plural(n, "batch"))
                } else {
                    format!("Plan approved · {}", plural(n, "batch"))
                };
                frame.complete(display, step, summary);
                return Ok(plan);
            }
            ReviewChoice::Abort => {
                step_display.clear_last(rows);
                frame.fail(
                    display,
                    step,
                    Failure {
                        reason: "aborted by user".into(),
                        detail: vec!["nothing staged, nothing committed".into()],
                    },
                );
                anyhow::bail!("aborted — no commit made");
            }
            ReviewChoice::Merge(a, b) => {
                step_display.clear_last(rows);
                let before = plan.clone();
                merge_batches(&mut plan, a, b)?;
                generator::validate_batch_plan(&plan, snapshot.hunk_counts())
                    .context("merged batch plan failed validation")?;
                history.push(before);
                merges += 1;
            }
            ReviewChoice::UndoMerge => {
                step_display.clear_last(rows);
                plan = history
                    .pop()
                    .ok_or_else(|| anyhow::anyhow!("no merge to undo"))?;
                merges = merges.saturating_sub(1);
            }
        }
    }
}

/// Fold Batches `a` and `b` (0-based) of `plan` into one Batch at the
/// earlier index: same-file changes coalesce (their hunk lists concatenate —
/// a valid plan never overlaps, so the union stays disjoint), other changes
/// append, and the reasons join with `" · "` (a `None` reason contributes
/// nothing). Errors on a self-merge or an out-of-range index — both
/// unreachable through the production menu, which offers only distinct
/// in-range Batches; the guard keeps scripted seams honest.
fn merge_batches(plan: &mut BatchPlanOutput, a: usize, b: usize) -> anyhow::Result<()> {
    if a == b {
        anyhow::bail!("cannot merge batch {} with itself", a + 1);
    }
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    if hi >= plan.batches.len() {
        anyhow::bail!(
            "batch index {} out of range (plan has {} batches)",
            hi + 1,
            plan.batches.len()
        );
    }
    let removed = plan.batches.remove(hi);
    let target = &mut plan.batches[lo];
    for change in removed.changes {
        match target.changes.iter_mut().find(|c| c.file == change.file) {
            Some(existing) => existing.hunks.extend(change.hunks),
            None => target.changes.push(change),
        }
    }
    target.reason = match (target.reason.take(), removed.reason) {
        (Some(x), Some(y)) => Some(format!("{x} · {y}")),
        (kept, None) | (None, kept) => kept,
    };
    Ok(())
}

// ----------------------------------------------------------------------
// Listing + labels
// ----------------------------------------------------------------------

/// Paint the plan listing — a pending header, one row per Batch — and
/// return how many rows it occupies (header + Batches + trailing blank) so
/// the loop can erase it with [`Display::clear_last`] when the menu resolves
/// or re-renders. Every row is truncated to the terminal's text width
/// ([`Display::text_width`] — the same resolution the commit preview wraps
/// to) so each Batch stays
/// on exactly one terminal row and the row count stays exact — a wrapped
/// row would make `clear_last` under-erase.
fn paint_plan(
    display: &Display,
    plan: &BatchPlanOutput,
    file_hunk_counts: &[(String, usize)],
) -> usize {
    let pending = palette::pending();
    // Row budget for the summary text: `text_width` already nets out both
    // margins, so only the `? ` marker's 2 columns come off the top.
    let width = display.text_width().saturating_sub(2);
    let pending_row = |text: &str, style: Style| {
        display.emit(&format!(
            "{} {}",
            display.styled("?", pending.clone()),
            display.styled(&truncate(text, width), style)
        ));
    };
    pending_row(
        &format!("batch plan — {} batches", plan.batches.len()),
        palette::pending(),
    );
    for (i, batch) in plan.batches.iter().enumerate() {
        pending_row(&batch_summary(i, batch, file_hunk_counts), palette::muted());
    }
    display.emit_blank();
    plan.batches.len() + 2
}

/// One Batch's listing row: `#N file:1-3, other (all) — reason`, hunk
/// indices folded into ranges. Empty `hunks` means every hunk of the file —
/// expanded to the full range via `file_hunk_counts` when known (the plan
/// view and the validator then agree on what "all" is), else shown as
/// `(all)`. Zero-hunk files (binary/mode/rename) always show `(all)`.
fn batch_summary(i: usize, batch: &BatchPlanBatch, file_hunk_counts: &[(String, usize)]) -> String {
    let counts: HashMap<&str, usize> = file_hunk_counts
        .iter()
        .map(|(p, n)| (p.as_str(), *n))
        .collect();
    let files = batch
        .changes
        .iter()
        .map(
            |c| match (c.hunks.is_empty(), counts.get(c.file.as_str())) {
                (false, _) => format!("{}:{}", c.file, fold_ranges(&c.hunks)),
                (true, Some(&n)) if n > 0 => {
                    format!("{}:{}", c.file, fold_ranges(&(1..=n).collect::<Vec<_>>()))
                }
                _ => format!("{} (all)", c.file),
            },
        )
        .collect::<Vec<_>>()
        .join(", ");
    let reason = batch
        .reason
        .as_deref()
        .map(|r| format!(" — {r}"))
        .unwrap_or_default();
    format!("#{} {files}{reason}", i + 1)
}

/// One Batch's menu-pick label: `#N files · reason` (no hunk ranges — the
/// listing above the menu already shows them; the label needs to stay short
/// enough for a Select row).
fn batch_label(i: usize, batch: &BatchPlanBatch) -> String {
    let files = batch
        .changes
        .iter()
        .map(|c| c.file.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    let reason = batch
        .reason
        .as_deref()
        .map(|r| format!(" · {}", truncate(r, 40)))
        .unwrap_or_default();
    format!("#{} {}{}", i + 1, truncate(&files, 48), reason)
}

/// Fold hunk indices into `1-3, 5` ranges. A validated plan's indices are
/// unique and in range but not necessarily ordered; sorting here keeps the
/// display stable regardless of the plan's ordering.
fn fold_ranges(hunks: &[usize]) -> String {
    let mut sorted = hunks.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut out = String::new();
    let mut i = 0;
    while i < sorted.len() {
        let start = sorted[i];
        let mut end = start;
        while i + 1 < sorted.len() && sorted[i + 1] == end + 1 {
            i += 1;
            end = sorted[i];
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        if start == end {
            out.push_str(&start.to_string());
        } else {
            out.push_str(&format!("{start}-{end}"));
        }
        i += 1;
    }
    out
}

// ----------------------------------------------------------------------
// Production menu
// ----------------------------------------------------------------------

/// The four menu rows in display order, each bound to its [`ReviewAction`]
/// in one place — the same single-table shape as the confirm menu's
/// `CONFIRM_ACTIONS`, so reordering rows can never silently desync a label
/// from its action. Continue leads (cursor 0) so Enter on an unattended
/// menu proceeds rather than merges; Abort trails like every other menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReviewAction {
    Continue,
    Merge,
    UndoMerge,
    Abort,
}

const REVIEW_ACTIONS: [(&str, ReviewAction); 4] = [
    ("Continue", ReviewAction::Continue),
    ("Merge two Batches…", ReviewAction::Merge),
    ("Undo last merge", ReviewAction::UndoMerge),
    ("Abort", ReviewAction::Abort),
];

/// The actions offered right now: Merge needs two or more Batches (merging
/// down to one must not strand the menu — a "Merge" row over a single Batch
/// would trap the second pick in its distinct-batch re-ask loop forever);
/// Undo last merge needs a merge to undo. Everything else is always offered.
fn available_actions(len: usize, can_undo: bool) -> Vec<(&'static str, ReviewAction)> {
    REVIEW_ACTIONS
        .iter()
        .filter(|(_, action)| match action {
            ReviewAction::Merge => len >= 2,
            ReviewAction::UndoMerge => can_undo,
            _ => true,
        })
        .copied()
        .collect()
}

/// Production Batch Review menu (issue #157): the offered actions as
/// timeline frame rows under the plan listing (ADR 0018) — `›` cursor, raw
/// keys; Merge then walks two Batch picks. A cancel at the action menu
/// (Esc / Ctrl-C / closed stdin) aborts — nothing has been staged, and the
/// menu's only go-back is the undo row (same philosophy as the confirm
/// menu). A cancel at a merge pick instead backs out to the action menu:
/// the picks are a sub-dialog of the menu, so cancelling there means "not
/// this action", not "end the Run" — and nothing is staged yet, so backing
/// out is always safe.
fn review_menu(batches: &[BatchPlanBatch], can_undo: bool) -> anyhow::Result<ReviewChoice> {
    loop {
        let actions = available_actions(batches.len(), can_undo);
        let labels: Vec<&str> = actions.iter().map(|(label, _)| *label).collect();
        let Some(action) = select_in_frame(
            "Review the batch plan",
            &labels,
            0,
            "↑/↓ select · Enter confirm · Esc or Ctrl-C aborts",
            false,
        )?
        .map(|idx| actions[idx].1) else {
            return Ok(ReviewChoice::Abort);
        };

        match action {
            ReviewAction::Continue => return Ok(ReviewChoice::Continue),
            ReviewAction::Abort => return Ok(ReviewChoice::Abort),
            ReviewAction::UndoMerge => return Ok(ReviewChoice::UndoMerge),
            ReviewAction::Merge => {
                if let Some((first, second)) = merge_picks(batches)? {
                    return Ok(ReviewChoice::Merge(first, second));
                }
                // A cancelled pick falls through: re-offer the action menu.
            }
        }
    }
}

/// The two Batch picks behind "Merge two Batches…". The second pick re-asks
/// while it equals the first — a self-merge is meaningless, and silently
/// skipping it would read as the menu ignoring the user. `None` when the
/// user cancels either pick (Esc / Ctrl-C / closed stdin): the caller
/// re-offers the action menu rather than aborting the Run.
fn merge_picks(batches: &[BatchPlanBatch]) -> anyhow::Result<Option<(usize, usize)>> {
    let labels: Vec<String> = batches
        .iter()
        .enumerate()
        .map(|(i, b)| batch_label(i, b))
        .collect();
    let rows: Vec<&str> = labels.iter().map(String::as_str).collect();
    let Some(first) = select_in_frame(
        "Merge which Batch first?",
        &rows,
        0,
        "↑/↓ select · Enter confirm · Esc back",
        true,
    )?
    else {
        return Ok(None);
    };
    let second = loop {
        let Some(pick) = select_in_frame(
            "…and which other Batch?",
            &rows,
            0,
            "↑/↓ select · Enter confirm · Esc back",
            true,
        )?
        else {
            return Ok(None);
        };
        if pick != first {
            break pick;
        }
    };
    Ok(Some((first, second)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::llm::generator::test_util::{batch, change, plan};
    use crate::render::display::DisplayWrite;
    use crate::workflow::run::WorkdirSnapshot;

    /// Discarding sink so unit tests never paint stderr.
    struct Quiet;

    impl DisplayWrite for Quiet {
        fn write_line(&self, _line: &str) {}
        fn clear_last(&self, _n: usize) {}
    }

    fn quiet_display() -> Display {
        Display::with(Quiet)
    }

    /// Capturing sink for row-width assertions: records every emitted line
    /// (margin included) so a test can check the one-row-per-Batch contract
    /// the erase accounting depends on.
    struct Capture(Arc<parking_lot::Mutex<Vec<String>>>);

    impl DisplayWrite for Capture {
        fn write_line(&self, line: &str) {
            self.0.lock().push(line.to_string());
        }
        fn clear_last(&self, _n: usize) {}
    }

    /// Menu that pops decisions from a queue and panics when exhausted, so
    /// an under-specified test fails loudly (same contract as e2e's
    /// `review_queue`).
    fn menu_queue(decisions: Vec<ReviewChoice>) -> ReviewMenu {
        let queue = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::from(
            decisions,
        )));
        Box::new(move |_batches: &[BatchPlanBatch], _can_undo: bool| {
            Ok(queue
                .lock()
                .pop_front()
                .expect("batch review menu queue exhausted"))
        })
    }

    fn interactive(menu: ReviewMenu) -> BatchReview {
        BatchReview::Interactive { menu }
    }

    /// Merging folds two Batches into one at the earlier index: same-file
    /// hunk lists concatenate, other files append, and the reasons join with
    /// " · ". The argument order (1,0 vs 0,1) must not matter.
    #[test]
    fn merge_batches_coalesces_same_file_and_joins_reasons() {
        let mut plan = plan(vec![
            batch(vec![change("a.rs", &[1, 2])], Some("imports")),
            batch(
                vec![change("a.rs", &[3]), change("b.rs", &[1])],
                Some("docs"),
            ),
        ]);
        merge_batches(&mut plan, 1, 0).unwrap();
        assert_eq!(plan.batches.len(), 1, "two batches must fold into one");
        let merged = &plan.batches[0];
        assert_eq!(merged.changes.len(), 2);
        assert_eq!(merged.changes[0].file, "a.rs");
        assert_eq!(merged.changes[0].hunks, vec![1, 2, 3]);
        assert_eq!(merged.changes[1].file, "b.rs");
        assert_eq!(merged.reason.as_deref(), Some("imports · docs"));

        // The merged plan is still an exact partition.
        generator::validate_batch_plan(&plan, &[("a.rs".to_string(), 3), ("b.rs".to_string(), 1)])
            .unwrap();
    }

    /// A `None` reason contributes nothing: Some + None keeps the Some side,
    /// None + None stays None.
    #[test]
    fn merge_batches_tolerates_missing_reasons() {
        let mut p = plan(vec![
            batch(vec![change("a.rs", &[1])], None),
            batch(vec![change("b.rs", &[1])], Some("docs")),
        ]);
        merge_batches(&mut p, 0, 1).unwrap();
        assert_eq!(p.batches[0].reason.as_deref(), Some("docs"));

        let mut p = plan(vec![
            batch(vec![change("a.rs", &[1])], None),
            batch(vec![change("b.rs", &[1])], None),
        ]);
        merge_batches(&mut p, 0, 1).unwrap();
        assert_eq!(p.batches[0].reason, None);
    }

    /// Self-merges and out-of-range indices are refused without mutating
    /// the plan — unreachable via the production menu, but scripted seams
    /// must not corrupt a plan.
    #[test]
    fn merge_batches_rejects_self_merge_and_out_of_range() {
        let mut plan = plan(vec![
            batch(vec![change("a.rs", &[1])], None),
            batch(vec![change("b.rs", &[1])], None),
        ]);
        let err = merge_batches(&mut plan, 0, 0).expect_err("self-merge must fail");
        assert!(format!("{err:#}").contains("with itself"), "got: {err:#}");
        let err = merge_batches(&mut plan, 0, 9).expect_err("out-of-range must fail");
        assert!(format!("{err:#}").contains("out of range"), "got: {err:#}");
        assert_eq!(plan.batches.len(), 2, "a refused merge must not mutate");
    }

    /// Hunk indices fold into compact ranges for the listing row.
    #[test]
    fn fold_ranges_compacts_runs() {
        assert_eq!(fold_ranges(&[1]), "1");
        assert_eq!(fold_ranges(&[1, 2, 3]), "1-3");
        assert_eq!(fold_ranges(&[1, 3, 4, 7]), "1, 3-4, 7");
        assert_eq!(fold_ranges(&[5, 1]), "1, 5", "unsorted input still folds");
        assert_eq!(fold_ranges(&[]), "");
    }

    /// `paint_plan` truncates header and Batch rows to the terminal's text
    /// width so a Batch never wraps onto a second terminal row — the erase
    /// accounting (`clear_last(rows)`) counts exact rows, and a wrapped row
    /// would leave residue. `Display::with` pins `FALLBACK_COLS`, so the
    /// budget here is deterministic.
    #[test]
    fn paint_plan_truncates_rows_to_the_width_budget() {
        let lines = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let display = Display::with(Capture(lines.clone()));
        let counts = vec![("a.rs".to_string(), 3)];
        let plan = plan(vec![batch(
            vec![change(&"very/long/path/".repeat(10), &[1, 2, 3])],
            Some(&"why ".repeat(20)),
        )]);
        assert_eq!(paint_plan(&display, &plan, &counts), 3);
        let rows = lines.lock().clone();
        assert_eq!(rows.len(), 3, "header + Batch row + trailing blank");
        assert_eq!(rows[2], "", "the trailing blank stays bare");
        assert!(rows[1].ends_with('…'), "the over-budget Batch row is cut");
        for row in &rows[..2] {
            assert!(
                row.chars().count() <= 80,
                "row must fit one FALLBACK_COLS line: {row}"
            );
        }
    }

    /// A non-painting frame for the review_plan tests: step accounting
    /// without any terminal output (stderr is piped under `cargo test`, so
    /// `RunFrame::new` builds the bookkeeping-only flavor).
    fn frame() -> RunFrame {
        futures::executor::block_on(RunFrame::new(vec!["Review plan".into()]))
    }

    /// The gate off never reaches the menu — the non-interactive default
    /// Run stays non-interactive.
    #[test]
    fn review_plan_passes_through_when_disabled() {
        let counts = vec![("a.rs".to_string(), 1)];
        let one = plan(vec![batch(vec![change("a.rs", &[1])], None)]);
        let out = review_plan(
            one,
            &WorkdirSnapshot::from_hunk_counts(counts),
            &quiet_display(),
            &BatchReview::Disabled,
            &frame(),
            StepIdx::FIRST,
        )
        .unwrap();
        assert_eq!(out.batches.len(), 1);
    }

    /// A single-Batch plan still reaches the menu when the gate is on (the
    /// plan is listed and abortable before any drafting is paid for) —
    /// issue #157's "shows the plan" for opted-in users even when nothing
    /// is mergeable.
    #[test]
    fn review_plan_single_batch_still_reaches_the_menu() {
        let counts = vec![("a.rs".to_string(), 1)];
        let one = plan(vec![batch(vec![change("a.rs", &[1])], None)]);
        let err = review_plan(
            one,
            &WorkdirSnapshot::from_hunk_counts(counts),
            &quiet_display(),
            &interactive(menu_queue(vec![ReviewChoice::Abort])),
            &frame(),
            StepIdx::FIRST,
        )
        .expect_err("the single-Batch menu must honor Abort");
        assert!(
            format!("{err:#}").contains("aborted — no commit made"),
            "expected the clean abort wording, got: {err:#}"
        );
    }

    /// Merge then Continue returns the merged (re-validated) plan and the
    /// loop re-renders between decisions.
    #[test]
    fn review_plan_merge_then_continue_returns_merged_plan() {
        let counts = vec![("a.rs".to_string(), 2), ("b.rs".to_string(), 1)];
        let plan = plan(vec![
            batch(vec![change("a.rs", &[1])], Some("first hunk")),
            batch(
                vec![change("a.rs", &[2]), change("b.rs", &[1])],
                Some("rest"),
            ),
        ]);
        let out = review_plan(
            plan,
            &WorkdirSnapshot::from_hunk_counts(counts.clone()),
            &quiet_display(),
            &interactive(menu_queue(vec![
                ReviewChoice::Merge(1, 0),
                ReviewChoice::Continue,
            ])),
            &frame(),
            StepIdx::FIRST,
        )
        .unwrap();
        assert_eq!(out.batches.len(), 1);
        assert_eq!(out.batches[0].changes[0].hunks, vec![1, 2]);
        assert_eq!(out.batches[0].reason.as_deref(), Some("first hunk · rest"));
        generator::validate_batch_plan(&out, &counts).unwrap();
    }

    /// Undo restores the plan as it stood before the last merge — the
    /// go-back the commit-confirmation menu pattern calls for — and the
    /// restored plan is the exact original, still a valid partition.
    #[test]
    fn review_plan_undo_restores_pre_merge_plan() {
        let counts = vec![("a.rs".to_string(), 2), ("b.rs".to_string(), 1)];
        let plan = plan(vec![
            batch(vec![change("a.rs", &[1])], Some("first hunk")),
            batch(
                vec![change("a.rs", &[2]), change("b.rs", &[1])],
                Some("rest"),
            ),
        ]);
        let out = review_plan(
            plan,
            &WorkdirSnapshot::from_hunk_counts(counts.clone()),
            &quiet_display(),
            &interactive(menu_queue(vec![
                ReviewChoice::Merge(0, 1),
                ReviewChoice::UndoMerge,
                ReviewChoice::Continue,
            ])),
            &frame(),
            StepIdx::FIRST,
        )
        .unwrap();
        assert_eq!(out.batches.len(), 2, "undo must restore both Batches");
        assert_eq!(out.batches[0].reason.as_deref(), Some("first hunk"));
        assert_eq!(out.batches[1].reason.as_deref(), Some("rest"));
        generator::validate_batch_plan(&out, &counts).unwrap();
    }

    /// Abort bails with the single-commit path's clean wording — nothing
    /// staged, nothing committed, so there is nothing to unwind.
    #[test]
    fn review_plan_abort_bails_with_no_commit_wording() {
        let counts = vec![("a.rs".to_string(), 1), ("b.rs".to_string(), 1)];
        let plan = plan(vec![
            batch(vec![change("a.rs", &[1])], None),
            batch(vec![change("b.rs", &[1])], None),
        ]);
        let err = review_plan(
            plan,
            &WorkdirSnapshot::from_hunk_counts(counts),
            &quiet_display(),
            &interactive(menu_queue(vec![ReviewChoice::Abort])),
            &frame(),
            StepIdx::FIRST,
        )
        .expect_err("abort must end the Run");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("aborted — no commit made"),
            "expected the clean abort wording, got: {msg}"
        );
    }

    /// The label↔action table is the single source — pins the display order
    /// (Continue first so the cursor-0 default proceeds, Abort last) and
    /// that every row carries a distinct action.
    #[test]
    fn review_actions_table_is_self_describing() {
        assert_eq!(REVIEW_ACTIONS[0].0, "Continue");
        assert_eq!(REVIEW_ACTIONS[1].0, "Merge two Batches…");
        assert_eq!(REVIEW_ACTIONS[2].0, "Undo last merge");
        assert_eq!(REVIEW_ACTIONS[3].0, "Abort");
        let actions: Vec<_> = REVIEW_ACTIONS.iter().map(|(_, a)| *a).collect();
        assert_eq!(actions.len(), 4);
        assert!(actions.contains(&ReviewAction::Continue));
        assert!(actions.contains(&ReviewAction::Merge));
        assert!(actions.contains(&ReviewAction::UndoMerge));
        assert!(actions.contains(&ReviewAction::Abort));
    }

    /// Merging down to one Batch must not strand the menu: with fewer than
    /// two Batches the Merge row disappears, and the Undo row appears only
    /// once a merge exists — so the second-pick re-ask can never trap over
    /// a single row, and Undo is never a dead row.
    #[test]
    fn available_actions_filter_merge_and_undo() {
        assert_eq!(available_actions(2, false).len(), 3);
        assert_eq!(available_actions(5, false).len(), 3);
        let one = available_actions(1, false);
        assert_eq!(one.len(), 2);
        assert_eq!(one[0].0, "Continue");
        assert_eq!(one[1].0, "Abort");
        // A merge exists: the Undo row joins, even over a single Batch.
        let undone = available_actions(1, true);
        assert_eq!(undone.len(), 3);
        assert_eq!(undone[1].0, "Undo last merge");
        assert_eq!(available_actions(2, true).len(), 4);
    }
}
