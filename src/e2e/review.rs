//! Batch Review (issue #157): with `review_batch_plan` enabled, the Run
//! shows the validated plan before drafting — whatever its size — and its
//! menu can merge Batches and undo a merge (via the review seam).
//! Merge-then-Continue yields one combined commit drafted from the merged
//! diff; Undo restores the pre-merge plan so its Batches commit separately;
//! Abort ends the Run before drafting with nothing staged and nothing
//! committed — the cheapest exit in the workflow.

use super::common::*;

/// A two-Batch plan over the two-file fixture: alpha.txt hunk 1 and
/// beta.txt hunk 1, each in its own Batch.
fn two_batch_plan() -> generator::BatchPlanOutput {
    generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("alpha".to_string()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "beta.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("beta".to_string()),
            },
        ],
    }
}

/// Merge then Continue: the plan folds into one Batch, so the Run lands one
/// commit carrying both files — the Drafted Message is drafted for the
/// merged diff (one messenger call), not two per-Batch messages.
#[tokio::test]
async fn batch_review_merge_then_continue_commits_one_combined_commit() {
    let dir = tempfile::tempdir().unwrap();
    two_file_unstaged_repo(dir.path());

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    commit_run(
        &git,
        RunDeps {
            redact: Redact::On,
            display: sink(),
            planner: planner_fixed(two_batch_plan()),
            messenger: messenger_fixed("feat: combined change"),
            confirm: Confirm::Disabled,
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Merge(0, 1), ReviewChoice::Continue]),
            },
        },
    )
    .await
    .unwrap();

    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "a merged plan must land exactly one combined commit"
    );
    assert_eq!(file_at_ref(dir.path(), "HEAD", "alpha.txt"), "a1\n");
    assert_eq!(file_at_ref(dir.path(), "HEAD", "beta.txt"), "b1\n");
    assert!(worktree_is_empty(dir.path()), "both files fully committed");
}

/// Abort: the Run ends before drafting with the single-commit path's clean
/// wording, and the working tree is untouched — nothing staged, nothing
/// committed, both changes exactly as they started.
#[tokio::test]
async fn batch_review_abort_stages_nothing_and_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    two_file_unstaged_repo(dir.path());

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let err = commit_run(
        &git,
        RunDeps {
            redact: Redact::On,
            display: sink(),
            planner: planner_fixed(two_batch_plan()),
            // Abort precedes drafting — the messenger must never be called.
            messenger: unreachable_messenger(),
            confirm: Confirm::Disabled,
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Abort]),
            },
        },
    )
    .await
    .expect_err("aborting the Batch Review must abort the Run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("aborted — no commit made"),
        "expected the clean abort wording, got: {msg}"
    );

    assert_eq!(
        commit_count(dir.path()),
        before,
        "an aborted review must not create a commit"
    );
    // Nothing staged, nothing committed: both changes stay unstaged exactly
    // as the Run found them (issue #157's abort acceptance contract).
    assert_eq!(
        status_porcelain(dir.path()).trim_end(),
        " M alpha.txt\n M beta.txt",
        "the working tree must be untouched, got: {:?}",
        status_porcelain(dir.path())
    );
}

/// Undo: a merge followed by Undo last merge restores the original plan,
/// so the Run lands the two separately-planned commits — the go-back the
/// commit-confirmation menu pattern calls for, instead of aborting the
/// whole Run over a mistaken merge.
#[tokio::test]
async fn batch_review_undo_restores_the_two_batch_plan() {
    let dir = tempfile::tempdir().unwrap();
    two_file_unstaged_repo(dir.path());

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    commit_run(
        &git,
        RunDeps {
            redact: Redact::On,
            display: sink(),
            planner: planner_fixed(two_batch_plan()),
            messenger: messenger_fixed("feat: separate change"),
            confirm: Confirm::Disabled,
            review: BatchReview::Interactive {
                menu: review_queue(vec![
                    ReviewChoice::Merge(0, 1),
                    ReviewChoice::UndoMerge,
                    ReviewChoice::Continue,
                ]),
            },
        },
    )
    .await
    .unwrap();

    assert_eq!(
        commit_count(dir.path()),
        before + 2,
        "undo must restore the original two-Batch plan"
    );
    assert!(worktree_is_empty(dir.path()), "both files fully committed");
}

/// A single-Batch plan still shows the menu (the plan is listed and
/// abortable) — issue #157's "shows the plan" for opted-in users even when
/// nothing is mergeable. Abort on it leaves everything untouched.
#[tokio::test]
async fn batch_review_single_batch_plan_still_aborts_from_the_menu() {
    let dir = tempfile::tempdir().unwrap();
    two_file_unstaged_repo(dir.path());

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let one_batch = generator::BatchPlanOutput {
        batches: vec![generator::BatchPlanBatch {
            changes: vec![
                generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                },
                generator::BatchChange {
                    file: "beta.txt".to_string(),
                    hunks: vec![1],
                },
            ],
            reason: Some("everything".to_string()),
        }],
    };
    let err = commit_run(
        &git,
        RunDeps {
            redact: Redact::On,
            display: sink(),
            planner: planner_fixed(one_batch),
            // Abort precedes drafting — the messenger must never be called.
            messenger: unreachable_messenger(),
            confirm: Confirm::Disabled,
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Abort]),
            },
        },
    )
    .await
    .expect_err("aborting the single-Batch review must abort the Run");
    assert!(
        format!("{err:#}").contains("aborted — no commit made"),
        "expected the clean abort wording, got: {err:#}"
    );
    assert_eq!(
        commit_count(dir.path()),
        before,
        "an aborted review must not create a commit"
    );
    assert_eq!(
        status_porcelain(dir.path()).trim_end(),
        " M alpha.txt\n M beta.txt",
        "the working tree must be untouched"
    );
}
