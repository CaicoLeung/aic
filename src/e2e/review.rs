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
    plan(vec![
        plan_batch("alpha.txt", &[1], "alpha"),
        plan_batch("beta.txt", &[1], "beta"),
    ])
}

/// Merge then Continue: the plan folds into one Batch, so the Run lands one
/// commit carrying both files — the Drafted Message is drafted for the
/// merged diff (one messenger call), not two per-Batch messages.
#[tokio::test]
async fn batch_review_merge_then_continue_commits_one_combined_commit() {
    let dir = with_repo(two_file_unstaged_repo);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Merge(0, 1), ReviewChoice::Continue]),
            },
            ..deps(
                planner_fixed(two_batch_plan()),
                messenger_fixed("feat: combined change"),
            )
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
    let dir = with_repo(two_file_unstaged_repo);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let err = commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Abort]),
            },
            // Abort precedes drafting — the messenger must never be called.
            ..deps(planner_fixed(two_batch_plan()), unreachable_messenger())
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
    let dir = with_repo(two_file_unstaged_repo);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![
                    ReviewChoice::Merge(0, 1),
                    ReviewChoice::UndoMerge,
                    ReviewChoice::Continue,
                ]),
            },
            ..deps(
                planner_fixed(two_batch_plan()),
                messenger_fixed("feat: separate change"),
            )
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
    let dir = with_repo(two_file_unstaged_repo);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let one_batch = plan(vec![plan_multi_batch(
        &[("alpha.txt", &[1]), ("beta.txt", &[1])],
        "everything",
    )]);
    let err = commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Abort]),
            },
            // Abort precedes drafting — the messenger must never be called.
            ..deps(planner_fixed(one_batch), unreachable_messenger())
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

/// The review gate applies to whichever plan proceeds — the LLM's or the
/// deterministic fallback's (`run/mod.rs` promises "the LLM's or the
/// fallback's"): an invalid plan (zero batches over real work) warns,
/// regroups, and the *fallback's* batches reach the menu, where they still
/// merge into one combined commit. A regression that showed the menu only
/// for LLM plans, or merged the discarded plan's stale batches, ships green
/// without this pin.
#[tokio::test]
async fn batch_review_operates_on_the_deterministic_fallback_plan() {
    let dir = with_repo(gh::init_test_repo);
    // Two single-hunk edits in different directories: the conservative
    // fallback keeps them as two batches, so a Merge choice proves the menu
    // held the fallback plan (a one-batch regroup could not merge).
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "base a\n").unwrap();
    std::fs::write(dir.path().join("docs/b.md"), "base b\n").unwrap();
    git_in(dir.path(), &["add", "."]);
    git_in(dir.path(), &["commit", "-m", "base tree"]);
    std::fs::write(dir.path().join("src/a.rs"), "new a\n").unwrap();
    std::fs::write(dir.path().join("docs/b.md"), "new b\n").unwrap();

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();
    let buf = BufferWrite::default();

    commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Merge(0, 1), ReviewChoice::Continue]),
            },
            redact: Redact::On,
            display: Display::with(buf.clone()),
            // Zero batches over real work — the most degenerate invalid plan.
            planner: planner_fixed(plan(vec![])),
            messenger: messenger_fixed("feat: combined fallback work"),
            confirm: Confirm::Disabled,
        },
    )
    .await
    .unwrap();

    // The fallback notice rendered, then the menu merged the fallback's two
    // batches into one commit carrying both files.
    let lines = buf.lines();
    assert!(
        lines
            .iter()
            .any(|l| l.contains("Regrouping deterministically")),
        "expected the fallback notice before the review menu, got: {lines:?}"
    );
    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "the merged fallback plan must land exactly one commit"
    );
    assert_eq!(file_at_ref(dir.path(), "HEAD", "src/a.rs"), "new a\n");
    assert_eq!(file_at_ref(dir.path(), "HEAD", "docs/b.md"), "new b\n");
    assert!(is_clean(dir.path()), "all work must be committed");
}

/// The two opt-in gates compose: the Batch Review merge folds the plan
/// first, then the per-commit confirmation menu gates the one combined
/// commit — Commit lands it. Pins that neither gate's queue ordering or
/// rendering breaks the other when both are enabled.
#[tokio::test]
async fn batch_review_merge_then_confirm_commit_lands_the_combined_commit() {
    let dir = with_repo(two_file_unstaged_repo);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    commit_run(
        &git,
        RunDeps {
            review: BatchReview::Interactive {
                menu: review_queue(vec![ReviewChoice::Merge(0, 1), ReviewChoice::Continue]),
            },
            redact: Redact::On,
            display: sink(),
            planner: planner_fixed(two_batch_plan()),
            messenger: messenger_fixed("feat: combined change"),
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Commit]),
                editor: unreachable_editor(),
            },
        },
    )
    .await
    .unwrap();

    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "review merge + confirm Commit must land the one combined commit"
    );
    assert_eq!(file_at_ref(dir.path(), "HEAD", "alpha.txt"), "a1\n");
    assert_eq!(file_at_ref(dir.path(), "HEAD", "beta.txt"), "b1\n");
    assert!(worktree_is_empty(dir.path()), "both files fully committed");
}
