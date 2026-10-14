//! Batch Review (issue #157): the opt-in gate that interrupts the Run
//! between batch-plan validation and Batch drafting. Owns the gate itself
//! ([`BatchReview`]), the menu seam, the production terminal menu, and the
//! review loop that merges Batches until the user continues or aborts. The
//! menu shows each Batch's files, hunk ranges, and reason; a merge folds two
//! Batches into one and re-validates the plan as an exact hunk partition
//! before the menu re-renders. Abort ends the Run before anything is staged
//! or drafted — the cheapest exit in the whole workflow.
//!
//! `review_plan` borrows the workflow's [`Display`] rather than owning it:
//! Batch Review is a phase of the commit Run, not a standalone service (same
//! shape as `confirm_draft`). See CONTEXT.md "Batch Review".

use std::collections::HashMap;

use anyhow::Context;

use crate::llm::generator::{self, BatchPlanBatch, BatchPlanOutput};
use crate::render::display::Display;
use crate::render::palette;
use crate::workflow::confirm::inquire_opt;

/// One decision from the Batch Review menu (issue #157). Continue proceeds
/// to drafting with the plan as shown; Abort ends the Run with nothing
/// staged and nothing committed; Merge folds Batches `a` and `b` (0-based)
/// into one and returns to the menu with the plan re-rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReviewChoice {
    Continue,
    Abort,
    Merge(usize, usize),
}

/// Erased Batch Review menu: given the current Batches, returns the user's
/// decision. Boxed for the same reason as the confirm-menu seam — production
/// wires it to a terminal menu, tests inject a scripted decision sequence.
pub(crate) type ReviewMenu = Box<dyn Fn(&[BatchPlanBatch]) -> anyhow::Result<ReviewChoice>>;

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
/// merge Batches until satisfied, and return the re-validated plan for
/// drafting. A pass-through when the gate is off — or the plan has fewer
/// than two Batches (nothing is mergeable, so the menu would be noise).
///
/// Each merge re-runs `validate_batch_plan`: merging two members of a valid
/// partition is always still a valid partition, so the check is defensive
/// only — but it guards the future plan-model extensions that will edit
/// plans in richer ways than a merge. Abort bails with the same clean
/// wording as the staged single-commit path's decline: nothing has been
/// staged or drafted, so there is nothing to unwind.
pub(crate) fn review_plan(
    mut plan: BatchPlanOutput,
    file_hunk_counts: &[(String, usize)],
    display: &Display,
    review: &BatchReview,
) -> anyhow::Result<BatchPlanOutput> {
    let BatchReview::Interactive { menu } = review else {
        return Ok(plan);
    };
    if plan.batches.len() < 2 {
        return Ok(plan);
    }

    loop {
        let rows = paint_plan(display, &plan, file_hunk_counts);
        match menu(&plan.batches)? {
            ReviewChoice::Continue => {
                display.clear_last(rows);
                return Ok(plan);
            }
            ReviewChoice::Abort => {
                display.clear_last(rows);
                anyhow::bail!("aborted — no commit made");
            }
            ReviewChoice::Merge(a, b) => {
                display.clear_last(rows);
                merge_batches(&mut plan, a, b)?;
                generator::validate_batch_plan(&plan, file_hunk_counts)
                    .context("merged batch plan failed validation")?;
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
pub(crate) fn merge_batches(plan: &mut BatchPlanOutput, a: usize, b: usize) -> anyhow::Result<()> {
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
/// or re-renders. Rows are truncated (files at 64 chars, reason at 40) so
/// each Batch stays on exactly one terminal row and the row count stays
/// exact.
fn paint_plan(
    display: &Display,
    plan: &BatchPlanOutput,
    file_hunk_counts: &[(String, usize)],
) -> usize {
    let pending = palette::pending();
    display.emit(&format!(
        "{} {}",
        display.styled("?", pending),
        display.styled(
            &format!("batch plan — {} batches", plan.batches.len()),
            palette::pending(),
        )
    ));
    for (i, batch) in plan.batches.iter().enumerate() {
        display.emit(&format!(
            "{} {}",
            display.styled("?", palette::pending()),
            display.styled(&batch_summary(i, batch, file_hunk_counts), palette::muted()),
        ));
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
        .map(|r| format!(" — {}", truncate(r, 40)))
        .unwrap_or_default();
    format!("#{} {}{}", i + 1, truncate(&files, 64), reason)
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
    Abort,
}

const REVIEW_ACTIONS: [(&str, ReviewAction); 3] = [
    ("Continue", ReviewAction::Continue),
    ("Merge two Batches…", ReviewAction::Merge),
    ("Abort", ReviewAction::Abort),
];

/// The actions offered for a plan of `len` Batches: all three, minus Merge
/// when fewer than two Batches remain (merging down to one must not strand
/// the menu — a "Merge" row over a single Batch would trap the second pick
/// in its distinct-batch re-ask loop forever).
fn available_actions(len: usize) -> Vec<(&'static str, ReviewAction)> {
    REVIEW_ACTIONS
        .iter()
        .filter(|(_, action)| len >= 2 || *action != ReviewAction::Merge)
        .copied()
        .collect()
}

/// Production Batch Review menu (issue #157): an `inquire::Select` over the
/// offered actions; Merge then walks two Batch picks. Esc / Ctrl-C / closed
/// stdin at any prompt aborts — nothing has been staged yet, so there is
/// nothing to go back to (same philosophy as the confirm menu).
fn review_menu(batches: &[BatchPlanBatch]) -> anyhow::Result<ReviewChoice> {
    let actions = available_actions(batches.len());
    let labels: Vec<&str> = actions.iter().map(|(label, _)| *label).collect();
    let Some(action) = select_row(
        "Review the batch plan",
        &labels,
        "↑/↓ select · Enter confirm · Esc or Ctrl-C aborts",
    )?
    .map(|idx| actions[idx].1) else {
        return Ok(ReviewChoice::Abort);
    };

    match action {
        ReviewAction::Continue => Ok(ReviewChoice::Continue),
        ReviewAction::Abort => Ok(ReviewChoice::Abort),
        ReviewAction::Merge => {
            let labels: Vec<String> = batches
                .iter()
                .enumerate()
                .map(|(i, b)| batch_label(i, b))
                .collect();
            let rows: Vec<&str> = labels.iter().map(String::as_str).collect();
            let Some(first) = select_row(
                "Merge which Batch first?",
                &rows,
                "↑/↓ select · Enter confirm · Esc aborts",
            )?
            else {
                return Ok(ReviewChoice::Abort);
            };
            // Re-ask the second pick while it equals the first — a
            // self-merge is meaningless, and silently skipping it would
            // read as the menu ignoring the user.
            let second = loop {
                let Some(pick) = select_row(
                    "…and which other Batch?",
                    &rows,
                    "↑/↓ select · Enter confirm · Esc aborts",
                )?
                else {
                    return Ok(ReviewChoice::Abort);
                };
                if pick != first {
                    break pick;
                }
            };
            Ok(ReviewChoice::Merge(first, second))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::llm::generator::BatchChange;
    use crate::render::display::DisplayWrite;

    /// Discarding sink so unit tests never paint stderr.
    struct Quiet;

    impl DisplayWrite for Quiet {
        fn write_line(&self, _line: &str) {}
        fn clear_last(&self, _n: usize) {}
    }

    fn quiet_display() -> Display {
        Display::with(Quiet)
    }

    fn change(file: &str, hunks: &[usize]) -> BatchChange {
        BatchChange {
            file: file.to_string(),
            hunks: hunks.to_vec(),
        }
    }

    fn batch(changes: Vec<BatchChange>, reason: Option<&str>) -> BatchPlanBatch {
        BatchPlanBatch {
            changes,
            reason: reason.map(String::from),
        }
    }

    /// Menu that pops decisions from a queue and panics when exhausted, so
    /// an under-specified test fails loudly (same contract as e2e's
    /// `menu_queue`).
    fn menu_queue(decisions: Vec<ReviewChoice>) -> ReviewMenu {
        let queue = std::sync::Arc::new(parking_lot::Mutex::new(std::collections::VecDeque::from(
            decisions,
        )));
        Box::new(move |_batches: &[BatchPlanBatch]| {
            Ok(queue
                .lock()
                .pop_front()
                .expect("batch review menu queue exhausted"))
        })
    }

    /// Menu that panics if called — proves the review loop never reaches
    /// the menu on a skip path.
    fn unreachable_menu() -> ReviewMenu {
        Box::new(|_: &[BatchPlanBatch]| -> anyhow::Result<ReviewChoice> {
            panic!("batch review menu must not be called")
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
        let mut plan = BatchPlanOutput {
            batches: vec![
                batch(vec![change("a.rs", &[1, 2])], Some("imports")),
                batch(
                    vec![change("a.rs", &[3]), change("b.rs", &[1])],
                    Some("docs"),
                ),
            ],
        };
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
        let mut plan = BatchPlanOutput {
            batches: vec![
                batch(vec![change("a.rs", &[1])], None),
                batch(vec![change("b.rs", &[1])], Some("docs")),
            ],
        };
        merge_batches(&mut plan, 0, 1).unwrap();
        assert_eq!(plan.batches[0].reason.as_deref(), Some("docs"));

        let mut plan = BatchPlanOutput {
            batches: vec![
                batch(vec![change("a.rs", &[1])], None),
                batch(vec![change("b.rs", &[1])], None),
            ],
        };
        merge_batches(&mut plan, 0, 1).unwrap();
        assert_eq!(plan.batches[0].reason, None);
    }

    /// Self-merges and out-of-range indices are refused without mutating
    /// the plan — unreachable via the production menu, but scripted seams
    /// must not corrupt a plan.
    #[test]
    fn merge_batches_rejects_self_merge_and_out_of_range() {
        let mut plan = BatchPlanOutput {
            batches: vec![
                batch(vec![change("a.rs", &[1])], None),
                batch(vec![change("b.rs", &[1])], None),
            ],
        };
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

    /// The gate off, or a plan with fewer than two Batches, never reaches
    /// the menu — the non-interactive default Run stays non-interactive.
    #[test]
    fn review_plan_skips_menu_when_disabled_or_single_batch() {
        let counts = vec![("a.rs".to_string(), 1)];
        let one = BatchPlanOutput {
            batches: vec![batch(vec![change("a.rs", &[1])], None)],
        };

        let out = review_plan(
            one.clone(),
            &counts,
            &quiet_display(),
            &BatchReview::Disabled,
        )
        .unwrap();
        assert_eq!(out.batches.len(), 1);

        // Interactive gate but a single Batch: still skipped — the menu
        // stub panics if the loop reaches it.
        let out = review_plan(
            one,
            &counts,
            &quiet_display(),
            &interactive(unreachable_menu()),
        )
        .unwrap();
        assert_eq!(out.batches.len(), 1);
    }

    /// Merge then Continue returns the merged (re-validated) plan and the
    /// loop re-renders between decisions.
    #[test]
    fn review_plan_merge_then_continue_returns_merged_plan() {
        let counts = vec![("a.rs".to_string(), 2), ("b.rs".to_string(), 1)];
        let plan = BatchPlanOutput {
            batches: vec![
                batch(vec![change("a.rs", &[1])], Some("first hunk")),
                batch(
                    vec![change("a.rs", &[2]), change("b.rs", &[1])],
                    Some("rest"),
                ),
            ],
        };
        let out = review_plan(
            plan,
            &counts,
            &quiet_display(),
            &interactive(menu_queue(vec![
                ReviewChoice::Merge(1, 0),
                ReviewChoice::Continue,
            ])),
        )
        .unwrap();
        assert_eq!(out.batches.len(), 1);
        assert_eq!(out.batches[0].changes[0].hunks, vec![1, 2]);
        assert_eq!(out.batches[0].reason.as_deref(), Some("first hunk · rest"));
        generator::validate_batch_plan(&out, &counts).unwrap();
    }

    /// Abort bails with the single-commit path's clean wording — nothing
    /// staged, nothing committed, so there is nothing to unwind.
    #[test]
    fn review_plan_abort_bails_with_no_commit_wording() {
        let counts = vec![("a.rs".to_string(), 1), ("b.rs".to_string(), 1)];
        let plan = BatchPlanOutput {
            batches: vec![
                batch(vec![change("a.rs", &[1])], None),
                batch(vec![change("b.rs", &[1])], None),
            ],
        };
        let err = review_plan(
            plan,
            &counts,
            &quiet_display(),
            &interactive(menu_queue(vec![ReviewChoice::Abort])),
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
        assert_eq!(REVIEW_ACTIONS[2].0, "Abort");
        let actions: Vec<_> = REVIEW_ACTIONS.iter().map(|(_, a)| *a).collect();
        assert_eq!(actions.len(), 3);
        assert!(actions.contains(&ReviewAction::Continue));
        assert!(actions.contains(&ReviewAction::Merge));
        assert!(actions.contains(&ReviewAction::Abort));
    }

    /// Merging down to one Batch must not strand the menu: with fewer than
    /// two Batches the Merge row disappears (Continue / Abort only), so the
    /// second-pick re-ask can never trap over a single row.
    #[test]
    fn available_actions_drop_merge_below_two_batches() {
        assert_eq!(available_actions(2).len(), 3);
        assert_eq!(available_actions(5).len(), 3);
        let one = available_actions(1);
        assert_eq!(one.len(), 2);
        assert_eq!(one[0].0, "Continue");
        assert_eq!(one[1].0, "Abort");
    }
}
