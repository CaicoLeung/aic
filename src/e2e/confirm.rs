//! Pre-commit confirmation (issue #78): with `confirm_before_commit` enabled,
//! the workflow shows the drafted message + file list, then offers a
//! Commit / Re-generate / Edit / Abort menu (via the menu seam) before each
//! commit lands. Abort ends the run — the current batch commits nothing,
//! earlier batches stay committed, and the remaining changes stay recoverable
//! (staged, exactly like a hook veto). Re-generate and Edit loop back to the
//! menu; only the final Commit lands.

use super::common::*;

/// Staged single-commit path + Abort: nothing commits, the abort message
/// names the outcome ("no commit made"), and the staged change survives so a
/// re-run can retry.
#[tokio::test]
async fn commit_confirm_abort_aborts_staged_single_commit() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "staged change\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let err = run_commit_workflow_impl(
        &git,
        unreachable_resolver(),
        prompt_queue(vec![]),
        sink(),
        unreachable_planner(), // staged path must NOT plan
        messenger_fixed("feat: staged change"),
        Confirm::Interactive {
            menu: menu_queue(vec![ConfirmChoice::Abort]),
            editor: unreachable_editor(),
        },
    )
    .await
    .expect_err("aborting the confirmation must abort the Run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("aborted — no commit made"),
        "expected the 'no commit made' abort, got: {msg}"
    );

    // No commit landed.
    assert_eq!(
        commit_count(dir.path()),
        before,
        "an aborted confirmation must not create a commit"
    );
    // The change is still staged — re-running `aic` picks it up via the
    // staged single-commit path.
    assert_eq!(
        status_porcelain(dir.path()).trim(),
        "M  tracked.txt",
        "the aborted batch must stay staged, got: {:?}",
        status_porcelain(dir.path())
    );
}

/// Staged single-commit path + Commit: the confirmation shows the drafted
/// message and file list, then commits normally and prints the post-commit
/// line — output includes the preview block.
#[tokio::test]
async fn commit_confirm_commit_commits_staged_single_commit() {
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
        prompt_queue(vec![]),
        display,
        unreachable_planner(),
        messenger_fixed("feat: staged change"),
        Confirm::Interactive {
            menu: menu_queue(vec![ConfirmChoice::Commit]),
            editor: unreachable_editor(),
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "a confirmed commit should land: {:?}",
        result
    );

    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "the confirmed staged commit must land"
    );
    assert_eq!(
        git_out(dir.path(), &["log", "-1", "--pretty=%B"]).trim(),
        "feat: staged change",
        "the confirmed message must be the one committed"
    );
    assert!(worktree_is_empty(dir.path()), "working tree must be clean");

    // The confirmed draft is erased — no residue next to the ✓ line.
    let lines = buf.lines();
    assert!(
        !lines.iter().any(|l| l.contains("proposed commit:")),
        "a confirmed draft must not linger, got: {lines:?}"
    );
    // The post-commit line is what remains — with the file-stats footer
    // (the preview's footer, re-rendered on the landed line).
    assert!(
        lines.iter().any(|l| l.contains("\u{2713}")),
        "post-commit line missing, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("+1 −1  tracked.txt")),
        "committed line must show the file stats footer, got: {lines:?}"
    );
}

/// Re-generate → Commit: the messenger runs twice on the same diff, the menu
/// shows after each draft, and the second draft is what lands.
#[tokio::test]
async fn commit_confirm_regenerate_then_commit_lands_new_message() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "staged change\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let before = commit_count(dir.path());
    let buf = BufferWrite::default();
    let display = Display::with(buf.clone());
    let git = Git::at(dir.path()).unwrap();
    let (messenger, calls) = messenger_sequence(&["feat: first draft", "feat: second draft"]);

    let result = run_commit_workflow_impl(
        &git,
        unreachable_resolver(),
        prompt_queue(vec![]),
        display,
        unreachable_planner(),
        messenger,
        Confirm::Interactive {
            menu: menu_queue(vec![ConfirmChoice::Regenerate, ConfirmChoice::Commit]),
            editor: unreachable_editor(),
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "Regenerate then Commit should succeed: {:?}",
        result
    );

    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "exactly one commit must land"
    );
    assert_eq!(*calls.lock(), 2, "the messenger must run once per draft");
    assert_eq!(
        git_out(dir.path(), &["log", "-1", "--pretty=%B"]).trim(),
        "feat: second draft",
        "the regenerated message must be the one committed"
    );
    // Each draft got its own preview (the messenger ran twice) and each was
    // erased once superseded or confirmed — only the landed ✓ line remains
    // (its subject legitimately contains the second-draft text; a lingering
    // draft would still carry the "proposed commit:" header).
    let lines = buf.lines();
    assert!(
        !lines.iter().any(|l| l.contains("proposed commit:")),
        "no draft may linger after the run, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("\u{2713}")),
        "post-commit line missing, got: {lines:?}"
    );
}

/// Edit → Commit: the editor rewrites the message, the edited version is what
/// lands (subject + body).
#[tokio::test]
async fn commit_confirm_edit_then_commit_lands_edited_message() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "staged change\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let result = run_commit_workflow_impl(
        &git,
        unreachable_resolver(),
        prompt_queue(vec![]),
        sink(),
        unreachable_planner(),
        messenger_fixed("feat: draft"),
        Confirm::Interactive {
            menu: menu_queue(vec![ConfirmChoice::Edit, ConfirmChoice::Commit]),
            editor: editor_fixed("feat: edited", Some("edited body")),
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "Edit then Commit should succeed: {:?}",
        result
    );

    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "the edited commit must land"
    );
    assert_eq!(
        git_out(dir.path(), &["log", "-1", "--pretty=%B"]).trim(),
        "feat: edited\n\nedited body",
        "the edited message must be the one committed"
    );
}

/// Edit cancelled → Commit: the editor returns the draft untouched (the
/// cancel path), so the original message is what lands.
#[tokio::test]
async fn commit_confirm_edit_cancel_keeps_original_message() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    std::fs::write(dir.path().join("tracked.txt"), "staged change\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let result = run_commit_workflow_impl(
        &git,
        unreachable_resolver(),
        prompt_queue(vec![]),
        sink(),
        unreachable_planner(),
        messenger_fixed("feat: draft"),
        Confirm::Interactive {
            menu: menu_queue(vec![ConfirmChoice::Edit, ConfirmChoice::Commit]),
            editor: editor_cancel(),
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "a cancelled edit then Commit should succeed: {:?}",
        result
    );

    assert_eq!(commit_count(dir.path()), before + 1, "the commit must land");
    assert_eq!(
        git_out(dir.path(), &["log", "-1", "--pretty=%B"]).trim(),
        "feat: draft",
        "a cancelled edit must keep the drafted message"
    );
}

/// Batch mode + Abort on batch 2: batch 1 commits, batch 2's confirmation is
/// aborted, and the abort message names the batch boundary. Batch 2's hunk
/// stays staged.
#[tokio::test]
async fn commit_confirm_abort_on_later_batch_keeps_earlier_commits() {
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
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: stub"),
        Confirm::Interactive {
            menu: menu_queue(vec![ConfirmChoice::Commit, ConfirmChoice::Abort]),
            editor: unreachable_editor(),
        },
    )
    .await
    .expect_err("aborting batch 2 must abort the Run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("declined on batch 2 of 2"),
        "expected batch-2 decline, got: {msg}"
    );
    assert!(
        msg.contains("1 batch committed"),
        "expected 1 committed, got: {msg}"
    );
    assert!(
        msg.contains("re-run `aic` to continue"),
        "decline must point at re-running aic, got: {msg}"
    );
    // The decline is three readable lines, not one wall of text.
    let abort_lines: Vec<&str> = msg.lines().collect();
    assert_eq!(
        abort_lines.len(),
        3,
        "expected a 3-line decline message, got: {msg:?}"
    );
    assert_eq!(
        abort_lines[0],
        "declined on batch 2 of 2 after 1 batch committed."
    );
    assert_eq!(
        abort_lines[1],
        "The remaining changes are still staged in the index."
    );
    assert_eq!(
        abort_lines[2], "re-run `aic` to continue.",
        "unexpected third line: {}",
        abort_lines[2]
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
        "the aborted batch's hunk must stay staged, got:\n{staged}"
    );
}

/// Batch mode + Commit on every batch: one menu per batch, all land, and the
/// working tree ends clean.
#[tokio::test]
async fn commit_confirm_commits_every_batch() {
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
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: stub"),
        Confirm::Interactive {
            menu: menu_queue(vec![ConfirmChoice::Commit, ConfirmChoice::Commit]),
            editor: unreachable_editor(),
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "confirming every batch should succeed: {:?}",
        result
    );

    assert_eq!(
        commit_count(dir.path()),
        before + 2,
        "two confirmed batches must land two commits"
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

/// Batch mode + Abort on the FIRST batch: nothing commits at all, the abort
/// reports zero committed, and the declined hunk stays staged.
#[tokio::test]
async fn commit_confirm_abort_first_batch_commits_nothing() {
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
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: stub"),
        Confirm::Interactive {
            menu: menu_queue(vec![ConfirmChoice::Abort]),
            editor: unreachable_editor(),
        },
    )
    .await
    .expect_err("aborting batch 1 must abort the Run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("declined on batch 1 of 2"),
        "expected batch-1 decline, got: {msg}"
    );
    assert!(
        msg.contains("0 batches committed"),
        "expected 0 committed, got: {msg}"
    );

    assert_eq!(
        commit_count(dir.path()),
        before,
        "a first-batch abort must not create any commit"
    );
}
