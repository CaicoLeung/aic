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
    let (dir, git) = with_git(gh::init_test_repo);

    stage_file(dir.path(), "tracked.txt", "staged change\n");

    let before = commit_count(dir.path());

    let err = commit_run(
        &git,
        // staged path must NOT plan,
        RunDeps {
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Abort]),
                editor: unreachable_editor(),
            },
            ..deps(
                unreachable_planner(),
                messenger_fixed("feat: staged change"),
            )
        },
    )
    .await
    .expect_err("aborting the confirmation must abort the Run");
    err_contains(&err, &["aborted — no commit made"]);

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

/// Staged single-commit path + Abort on a hunk-staged file (issue #150):
/// the abort must leave the user's hunk-level staging selection untouched —
/// hunk A stays staged in the index, hunk B stays unstaged in the workdir —
/// so a re-run sees exactly the same partial staging.
#[tokio::test]
async fn commit_confirm_abort_keeps_hunk_level_staging_selection() {
    let (dir, git) = with_git(gh::init_test_repo);
    let (_a_state, b_state) = partial_stage_two_hunk_file(dir.path());

    let before = commit_count(dir.path());

    let err = commit_run(
        &git,
        RunDeps {
            // staged path must NOT plan,
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Abort]),
                editor: unreachable_editor(),
            },
            ..deps(
                unreachable_planner(),
                messenger_fixed("feat: staged hunk only"),
            )
        },
    )
    .await
    .expect_err("aborting the confirmation must abort the Run");
    err_contains(&err, &["aborted — no commit made"]);

    // No commit landed.
    assert_eq!(
        commit_count(dir.path()),
        before,
        "an aborted confirmation must not create a commit"
    );
    // The index still holds exactly hunk A — the staged diff carries the
    // staged region and not the unstaged one.
    let cached = git_out(dir.path(), &["diff", "--cached"]);
    assert!(
        cached.contains("A-staged") && !cached.contains("B-unstaged"),
        "the index must keep hunk A alone, got cached diff:\n{cached}"
    );
    // The unstaged hunk survives in the workdir as an unstaged entry.
    assert_eq!(read_file(dir.path(), "tracked.txt"), b_state);
    assert_eq!(
        status_porcelain(dir.path()).trim_end(),
        "MM tracked.txt",
        "abort must preserve the split: staged hunk A + unstaged hunk B, got: {:?}",
        status_porcelain(dir.path())
    );
    assert!(is_clean(dir.path()), "no merge/rebase state must remain");
}

/// Staged single-commit path + Commit: the confirmation shows the drafted
/// message and file list, then commits normally and prints the post-commit
/// line — output includes the preview block.
#[tokio::test]
async fn commit_confirm_commit_commits_staged_single_commit() {
    let dir = with_repo(gh::init_test_repo);

    stage_file(dir.path(), "tracked.txt", "staged change\n");

    let buf = BufferWrite::default();
    let display = Display::with(buf.clone());

    run_expect_commits(
        dir.path(),
        RunDeps {
            display,
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Commit]),
                editor: unreachable_editor(),
            },
            ..deps(
                unreachable_planner(),
                messenger_fixed("feat: staged change"),
            )
        },
        1,
        "the confirmed staged commit must land",
    )
    .await;
    assert_eq!(
        git_out(dir.path(), &["log", "-1", "--pretty=%B"]).trim(),
        "feat: staged change",
        "the confirmed message must be the one committed"
    );
    assert!(worktree_is_empty(dir.path()), "working tree must be clean");

    // The confirmed draft is erased — no residue next to the ✓ line: a
    // lingering draft would still carry its `? ` pending marker on the
    // subject row (pinned to this draft's subject, not any `? ` row — the
    // confirm menu's own asks legitimately use `? `).
    let lines = buf.lines();
    assert!(
        !lines
            .iter()
            .any(|l| l.starts_with("  ? ") && l.contains("feat: staged change")),
        "a confirmed draft must not linger, got: {lines:?}"
    );
    // The post-commit line is what remains — with the file-stats footer
    // (the preview's footer, re-rendered on the landed line).
    assert!(
        lines.iter().any(|l| l.contains("\u{2713}")),
        "post-commit line missing, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("+1 −1   tracked.txt")),
        "committed line must show the file stats footer, got: {lines:?}"
    );
}

/// Re-generate → Commit: the messenger runs twice on the same diff, the menu
/// shows after each draft, and the second draft is what lands.
#[tokio::test]
async fn commit_confirm_regenerate_then_commit_lands_new_message() {
    let dir = with_repo(gh::init_test_repo);

    stage_file(dir.path(), "tracked.txt", "staged change\n");

    let buf = BufferWrite::default();
    let display = Display::with(buf.clone());
    let (messenger, calls) = messenger_sequence(&["feat: first draft", "feat: second draft"]);

    run_expect_commits(
        dir.path(),
        RunDeps {
            display,
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Regenerate, ConfirmChoice::Commit]),
                editor: unreachable_editor(),
            },
            ..deps(unreachable_planner(), messenger)
        },
        1,
        "exactly one commit must land",
    )
    .await;
    assert_eq!(*calls.lock(), 2, "the messenger must run once per draft");
    assert_eq!(
        git_out(dir.path(), &["log", "-1", "--pretty=%B"]).trim(),
        "feat: second draft",
        "the regenerated message must be the one committed"
    );
    // Each draft got its own preview (the messenger ran twice) and each was
    // erased once superseded or confirmed — only the landed ✓ line remains
    // (its subject legitimately contains the second-draft text; a lingering
    // draft would still carry its `? ` pending marker on its subject row —
    // pinned to the draft subjects, not any `? ` row, since the confirm
    // menu's own asks legitimately use `? `).
    let lines = buf.lines();
    assert!(
        !lines.iter().any(|l| {
            l.starts_with("  ? ")
                && (l.contains("feat: second draft") || l.contains("feat: first draft"))
        }),
        "no draft may linger after the run, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("\u{2713}")),
        "post-commit line missing, got: {lines:?}"
    );
}

/// `--no-redact` covers the Run's *whole* confirm loop (ADR 0017): the
/// override skips the initial gate AND the Re-generate redraft re-sends the
/// secret-shaped staged diff without a second gate surprise-blocking it — the
/// menu [Regenerate, Commit] flow drafts twice and the second message lands.
#[tokio::test]
async fn no_redact_run_survives_confirm_regenerate_on_secret_shaped_diff() {
    let dir = with_repo(gh::init_test_repo);

    stage_file(
        dir.path(),
        "tracked.txt",
        "key = \"AKIAIOSFODNN7EXAMPLE\"\n",
    );

    let (messenger, calls) = messenger_sequence(&["feat: first draft", "feat: second draft"]);

    run_expect_commits(
        dir.path(),
        RunDeps {
            redact: Redact::Off,
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Regenerate, ConfirmChoice::Commit]),
                editor: unreachable_editor(),
            },
            ..deps(unreachable_planner(), messenger)
        },
        1,
        "one commit lands",
    )
    .await;
    assert_eq!(*calls.lock(), 2, "the redraft must reach the messenger");
    assert_eq!(
        git_out(dir.path(), &["log", "-1", "--pretty=%B"]).trim(),
        "feat: second draft",
        "the regenerated message must be the one committed"
    );
}

/// Edit → Commit: the editor rewrites the message, the edited version is what
/// lands (subject + body).
#[tokio::test]
async fn commit_confirm_edit_then_commit_lands_edited_message() {
    let dir = with_repo(gh::init_test_repo);

    stage_file(dir.path(), "tracked.txt", "staged change\n");

    run_expect_commits(
        dir.path(),
        RunDeps {
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Edit, ConfirmChoice::Commit]),
                editor: editor_fixed("feat: edited", Some("edited body")),
            },
            ..deps(unreachable_planner(), messenger_fixed("feat: draft"))
        },
        1,
        "the edited commit must land",
    )
    .await;
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
    let dir = with_repo(gh::init_test_repo);

    stage_file(dir.path(), "tracked.txt", "staged change\n");

    run_expect_commits(
        dir.path(),
        RunDeps {
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Edit, ConfirmChoice::Commit]),
                editor: editor_cancel(),
            },
            ..deps(unreachable_planner(), messenger_fixed("feat: draft"))
        },
        1,
        "the commit must land",
    )
    .await;
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
    let (dir, git) = with_git(gh::init_test_repo);

    // One file, two change sites far enough apart that git emits two hunks.
    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}c0\n");
    let changed = format!("a1\n{pad}c1\n");
    commit_file(dir.path(), "tracked.txt", &base, "base");
    std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

    let plan = plan(vec![
        plan_batch("tracked.txt", &[1], "change a"),
        plan_batch("tracked.txt", &[2], "change c"),
    ]);
    let before = commit_count(dir.path());

    let err = commit_run(
        &git,
        RunDeps {
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Commit, ConfirmChoice::Abort]),
                editor: unreachable_editor(),
            },
            ..deps(planner_fixed(plan), messenger_fixed("chore: stub"))
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
    let (dir, git) = with_git(two_file_unstaged_repo);

    // One file per batch — batch 1 = alpha, batch 2 = beta.
    let plan = plan(vec![
        plan_batch("alpha.txt", &[1], "change alpha"),
        plan_batch("beta.txt", &[1], "change beta"),
    ]);
    let before = commit_count(dir.path());

    let result = commit_run(
        &git,
        RunDeps {
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Commit, ConfirmChoice::Commit]),
                editor: unreachable_editor(),
            },
            ..deps(planner_fixed(plan), messenger_fixed("chore: stub"))
        },
    )
    .await;
    assert_ok(&result, "confirming every batch should succeed");

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

/// A multi-file batch (two files in one commit) must render the Σ total row
/// on its landed ✓ line. This drives the full chain — stage → commit →
/// `committed_stats` → `commit_line` → `emit_file_stats` — with a capturing
/// sink, so a regression that drops the Σ row only end-to-end (e.g.
/// `committed_stats` losing a file to the `seen` filter, or a guard wired to
/// only the preview caller) fails here, where the per-call unit test cannot.
#[tokio::test]
async fn multi_file_batch_landed_line_shows_sigma_total() {
    let (_dir, git) = with_git(two_file_unstaged_repo);

    // One batch carrying both files — a genuine multi-file commit.
    let plan = plan(vec![plan_multi_batch(
        &[("alpha.txt", &[1]), ("beta.txt", &[1])],
        "both files together",
    )]);
    let buf = BufferWrite::default();

    let display = Display::with(buf.clone());

    let result = commit_run(
        &git,
        RunDeps {
            display,
            ..deps(planner_fixed(plan), messenger_fixed("chore: both"))
        },
    )
    .await;
    assert_ok(&result, "multi-file batch should land");

    let lines = buf.lines();
    assert!(
        lines.iter().any(|l| l.contains("alpha.txt")),
        "landed footer must list alpha.txt, got: {lines:?}"
    );
    assert!(
        lines.iter().any(|l| l.contains("beta.txt")),
        "landed footer must list beta.txt, got: {lines:?}"
    );
    assert!(
        lines
            .iter()
            .any(|l| { l.contains("Σ") && l.contains("+2") && l.contains("−2") }),
        "multi-file landed commit must show the Σ total row, got: {lines:?}"
    );
}

/// Batch mode + Abort on the FIRST batch: nothing commits at all, the abort
/// reports zero committed, and the declined hunk stays staged.
#[tokio::test]
async fn commit_confirm_abort_first_batch_commits_nothing() {
    let (dir, git) = with_git(gh::init_test_repo);

    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}c0\n");
    let changed = format!("a1\n{pad}c1\n");
    commit_file(dir.path(), "tracked.txt", &base, "base");
    std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

    let plan = plan(vec![
        plan_batch("tracked.txt", &[1], "change a"),
        plan_batch("tracked.txt", &[2], "change c"),
    ]);
    let before = commit_count(dir.path());

    let err = commit_run(
        &git,
        RunDeps {
            confirm: Confirm::Interactive {
                menu: menu_queue(vec![ConfirmChoice::Abort]),
                editor: unreachable_editor(),
            },
            ..deps(planner_fixed(plan), messenger_fixed("chore: stub"))
        },
    )
    .await
    .expect_err("aborting batch 1 must abort the Run");
    err_contains(&err, &["declined on batch 1 of 2", "0 batches committed"]);

    assert_eq!(
        commit_count(dir.path()),
        before,
        "a first-batch abort must not create any commit"
    );
}
