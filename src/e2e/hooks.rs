//! git hooks fire (and can veto) during a Run (issues #20, #30).

use super::common::*;

/// Issue #20 AC1: git hooks fire during a full Run. The Run commits through
/// the real `git commit` CLI (`Git::commit`), so `pre-commit` and `commit-msg`
/// execute mid-Run. Under the pre-#19 libgit2 commit path neither ever ran
/// (libgit2 has no hook machinery) — this e2e test pins the shell-out behavior
/// from the orchestration layer down: LLM stubbed, repo real, hooks installed
/// in the repo's `.git/hooks`.
#[tokio::test]
async fn commit_run_runs_pre_commit_and_commit_msg_hooks() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // pre-commit drops a sentinel file in the worktree — proves the hook
    // executed during the Run.
    gh::install_hook(dir.path(), "pre-commit", "echo ran > sentinel.txt");
    // commit-msg appends a trailer to the message file ($1) — proves the hook
    // executed AND its edit survived into the landed commit.
    gh::install_hook(
        dir.path(),
        "commit-msg",
        "echo 'Signed-off-by: aic-e2e' >> \"$1\"",
    );

    // One change site → one hunk → one batch. Same shape as the marquee
    // split test, minus the split.
    std::fs::write(dir.path().join("tracked.txt"), "changed by hook test\n").unwrap();
    let plan = plan_single_batch("tracked.txt", "hook test");

    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("chore: hook run"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "Run with hooks installed should succeed: {:?}",
        result
    );

    // The change itself landed in the commit (the Run did not silently drop
    // the file while hooks ran).
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "tracked.txt"),
        "changed by hook test\n"
    );
    // pre-commit ran → its sentinel exists.
    assert!(
        dir.path().join("sentinel.txt").exists(),
        "pre-commit hook must run during a Run"
    );
    // commit-msg ran → its trailer is in the committed message, alongside the
    // authored subject.
    let msg = git_out(dir.path(), &["log", "-1", "--pretty=%B"]);
    assert!(
        msg.contains("Signed-off-by: aic-e2e"),
        "commit-msg hook must run during a Run; got message:\n{msg}"
    );
    assert!(
        msg.contains("chore: hook run"),
        "the authored subject must survive alongside the hook's trailer"
    );
}

/// Issue #20 AC2: a hook that vetoes the commit aborts the Run cleanly with
/// the hook's own message, and lands nothing — the staged index survives
/// intact. `git commit` refuses before writing the commit object, so the
/// batch's staged hunks stay staged, ready for a re-run. Guards against a
/// regression back to libgit2, which would commit past the veto silently.
#[tokio::test]
async fn commit_run_hook_veto_aborts_with_index_intact() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    gh::install_hook(
        dir.path(),
        "pre-commit",
        "echo 'blocked by policy' >&2; exit 1",
    );

    std::fs::write(dir.path().join("tracked.txt"), "changed by hook test\n").unwrap();
    let plan = plan_single_batch("tracked.txt", "hook veto");

    let git = Git::at(dir.path()).unwrap();
    let before = commit_count(dir.path());

    let err = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("chore: vetoed"),
            confirm: Confirm::Disabled,
        },
    )
    .await
    .expect_err("a vetoing pre-commit hook must abort the Run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("blocked by policy"),
        "the hook's stderr must surface, got: {msg}"
    );
    assert!(
        msg.contains("aborted on batch 1 of 1"),
        "the Run must abort cleanly at the batch boundary, got: {msg}"
    );

    // No commit landed.
    assert_eq!(
        commit_count(dir.path()),
        before,
        "a vetoed Run must not create a commit"
    );
    // The staged index is intact: the batch's hunks are still staged.
    let staged = git.diff(Some("tracked.txt")).unwrap();
    assert!(
        staged.contains("changed by hook test"),
        "staged hunks must survive the veto; staged diff:\n{staged}"
    );
}

/// Issue #30: a `commit-msg` veto aborts the Run the same way a `pre-commit`
/// veto does (pinned by [`commit_run_hook_veto_aborts_with_index_intact`]).
/// `commit-msg` fires *after* the message is drafted but *before* the commit
/// object is written, so the recoverable-state contract must hold here too:
/// no commit lands, the batch's hunks stay staged, and the abort message
/// names the batch boundary. Guards a regression to libgit2 (which has no
/// hook machinery and would commit past the veto) and, more subtly, a
/// regression where the message-drafting step ran but the shell-out's
/// non-zero exit was swallowed.
///
/// The vetoing message is distinct ("blocked by commit-msg policy") so a
/// future regression that fired the wrong hook (or re-ran `pre-commit`)
/// would change the surfaced text and fail loudly, not ship green.
#[tokio::test]
async fn commit_run_commit_msg_veto_aborts_with_index_intact() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    gh::install_hook(
        dir.path(),
        "commit-msg",
        "echo 'blocked by commit-msg policy' >&2; exit 1",
    );

    std::fs::write(dir.path().join("tracked.txt"), "changed by hook test\n").unwrap();
    let plan = plan_single_batch("tracked.txt", "hook veto");

    let git = Git::at(dir.path()).unwrap();
    let before = commit_count(dir.path());

    let err = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("chore: vetoed"),
            confirm: Confirm::Disabled,
        },
    )
    .await
    .expect_err("a vetoing commit-msg hook must abort the Run");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("blocked by commit-msg policy"),
        "the commit-msg hook's stderr must surface, got: {msg}"
    );
    assert!(
        msg.contains("aborted on batch 1 of 1"),
        "the Run must abort cleanly at the batch boundary, got: {msg}"
    );

    // No commit landed — git refused before writing the commit object.
    assert_eq!(
        commit_count(dir.path()),
        before,
        "a vetoed Run must not create a commit"
    );
    // The staged index is intact: the batch's hunks are still staged, ready
    // for a re-run once the offending hook is fixed.
    let staged = git.diff(Some("tracked.txt")).unwrap();
    assert!(
        staged.contains("changed by hook test"),
        "staged hunks must survive the commit-msg veto; staged diff:\n{staged}"
    );
}
