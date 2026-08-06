//! Pre-commit confirmation (issue #78): with `confirm_before_commit` enabled,
//! the workflow shows the drafted message + file list and asks the user (via
//! the prompt seam) before each commit lands. A decline aborts — the current
//! batch commits nothing, earlier batches stay committed, and the remaining
//! changes stay recoverable (staged, exactly like a hook veto).

use super::common::*;

/// Staged single-commit path + decline: nothing commits, the abort message
/// names the outcome ("no commit made"), and the staged change survives so a
/// re-run can retry.
#[tokio::test]
async fn commit_confirm_decline_aborts_staged_single_commit() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "staged change\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let err = run_commit_workflow_impl(
        &git,
        unreachable_resolver(),
        prompt_queue(vec![false]), // decline the confirmation
        sink(),
        unreachable_planner(), // staged path must NOT plan
        messenger_fixed("feat: staged change"),
        true,
    )
    .await
    .expect_err("declining the confirmation must abort the Run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("aborted — no commit made"),
        "expected the 'no commit made' abort, got: {msg}"
    );

    // No commit landed.
    assert_eq!(
        commit_count(dir.path()),
        before,
        "a declined confirmation must not create a commit"
    );
    // The change is still staged — re-running `aic` picks it up via the
    // staged single-commit path.
    assert_eq!(
        status_porcelain(dir.path()).trim(),
        "M  tracked.txt",
        "the declined batch must stay staged, got: {:?}",
        status_porcelain(dir.path())
    );
}

/// Staged single-commit path + accept: the confirmation shows the drafted
/// message and file list, then yes commits normally and prints the post-commit
/// line — output includes the preview block.
#[tokio::test]
async fn commit_confirm_accept_commits_staged_single_commit() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "staged change\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let before = commit_count(dir.path());
    let buf = BufferWrite::default();
    let display = Display::with(buf.clone());
    let git = Git::at(dir.path()).unwrap();

    let result = run_commit_workflow_impl(
        &git,
        unreachable_resolver(),
        prompt_queue(vec![true]), // approve
        display,
        unreachable_planner(),
        messenger_fixed("feat: staged change"),
        true,
    )
    .await;
    assert!(
        result.is_ok(),
        "an approved confirmation should commit: {:?}",
        result
    );

    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "the approved staged commit must land"
    );
    assert_eq!(
        git_out(dir.path(), &["log", "-1", "--pretty=%B"]).trim(),
        "feat: staged change",
        "the confirmed message must be the one committed"
    );
    assert!(worktree_is_empty(dir.path()), "working tree must be clean");

    // The preview block appeared before the commit: header, message, files.
    let lines = buf.lines();
    let preview = lines
        .iter()
        .position(|l| l.contains("proposed commit:"))
        .expect("expected a proposed-commit preview, got: {lines:?}");
    assert!(
        lines[preview + 1].contains("feat: staged change"),
        "preview must show the drafted subject, got: {:?}",
        &lines[preview..]
    );
    assert!(
        lines.iter().any(|l| l.contains("files: tracked.txt")),
        "preview must list the files, got: {lines:?}"
    );
    // The post-commit line still follows the approval.
    assert!(
        lines.iter().any(|l| l.contains("\u{2713}")),
        "post-commit line missing, got: {lines:?}"
    );
}

/// Batch mode + decline on batch 2: batch 1 commits, batch 2's confirmation is
/// declined, and the abort message names the batch boundary. Batch 2's hunk
/// stays staged.
#[tokio::test]
async fn commit_confirm_decline_on_later_batch_keeps_earlier_commits() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // One file, two change sites far enough apart that git emits two hunks.
    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}c0\n");
    let changed = format!("a1\n{pad}c1\n");
    std::fs::write(dir.path().join("tracked.txt"), &base).unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);
    git_in(dir.path(), &["commit", "-m", "base"]);
    std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "tracked.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change a".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "tracked.txt".to_string(),
                    hunks: vec![2],
                }],
                reason: Some("change c".into()),
            },
        ],
    };
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let err = run_commit_workflow_impl(
        &git,
        resolver_returning(""),
        prompt_queue(vec![true, false]), // approve batch 1, decline batch 2
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: stub"),
        true,
    )
    .await
    .expect_err("declining batch 2 must abort the Run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("aborted on batch 2 of 2"),
        "expected batch-2 abort, got: {msg}"
    );
    assert!(
        msg.contains("1 batch(es) committed"),
        "expected 1 committed, got: {msg}"
    );
    assert!(
        msg.contains("re-run `aic` to continue"),
        "abort must point at re-running aic, got: {msg}"
    );

    // Batch 1 committed (hunk 1 in HEAD); batch 2's hunk did not land.
    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "exactly batch 1 must be committed"
    );
    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(head.contains("a1"), "batch 1 must be committed");
    assert!(!head.contains("c1"), "batch 2 must NOT be committed");
    // Batch 2's change is still staged — recoverable by re-running `aic`.
    let staged = git.diff(Some("tracked.txt")).unwrap();
    assert!(
        staged.contains("c1"),
        "the declined batch's hunk must stay staged, got:\n{staged}"
    );
}

/// Batch mode + approve every batch: one confirmation per batch, all land, and
/// the working tree ends clean.
#[tokio::test]
async fn commit_confirm_approves_every_batch() {
    let dir = tempfile::tempdir().unwrap();
    two_file_unstaged_repo(dir.path());

    // One file per batch — batch 1 = alpha, batch 2 = beta.
    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change alpha".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "beta.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change beta".into()),
            },
        ],
    };
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let result = run_commit_workflow_impl(
        &git,
        resolver_returning(""),
        prompt_queue(vec![true, true]), // approve both batches
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: stub"),
        true,
    )
    .await;
    assert!(
        result.is_ok(),
        "approving every batch should succeed: {:?}",
        result
    );

    assert_eq!(
        commit_count(dir.path()),
        before + 2,
        "two approved batches must land two commits"
    );
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "alpha.txt"),
        "a1\n",
        "batch 1 must commit alpha's change"
    );
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "beta.txt"),
        "b1\n",
        "batch 2 must commit beta's change"
    );
    assert!(worktree_is_empty(dir.path()), "working tree must be clean");
}

/// Batch mode + decline on the FIRST batch: nothing commits at all, the abort
/// reports zero committed, and the declined hunk stays staged.
#[tokio::test]
async fn commit_confirm_decline_first_batch_commits_nothing() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}c0\n");
    let changed = format!("a1\n{pad}c1\n");
    std::fs::write(dir.path().join("tracked.txt"), &base).unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);
    git_in(dir.path(), &["commit", "-m", "base"]);
    std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "tracked.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change a".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "tracked.txt".to_string(),
                    hunks: vec![2],
                }],
                reason: Some("change c".into()),
            },
        ],
    };
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let err = run_commit_workflow_impl(
        &git,
        resolver_returning(""),
        prompt_queue(vec![false]), // decline the first batch
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: stub"),
        true,
    )
    .await
    .expect_err("declining batch 1 must abort the Run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("aborted on batch 1 of 2"),
        "expected batch-1 abort, got: {msg}"
    );
    assert!(
        msg.contains("0 batch(es) committed"),
        "expected 0 committed, got: {msg}"
    );

    assert_eq!(
        commit_count(dir.path()),
        before,
        "a first-batch decline must not create any commit"
    );
}
