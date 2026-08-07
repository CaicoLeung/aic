//! git hooks fire (and can veto, and can restage whole files) during a Run
//! (issues #20, #30, AIC-11).

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
    let result = run_commit_workflow_impl(
        &git,
        resolver_returning(""),
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: hook run"),
        Confirm::Disabled,
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

    let err = run_commit_workflow_impl(
        &git,
        resolver_returning(""),
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: vetoed"),
        Confirm::Disabled,
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

    let err = run_commit_workflow_impl(
        &git,
        resolver_returning(""),
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: vetoed"),
        Confirm::Disabled,
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

/// AIC-11: the CONTEXT.md "Batch staging" invariant — staging re-reads the
/// current diff before each Batch lands (never the plan-time snapshot), so a
/// pre-commit hook that restages whole files (the lint-staged/prettier
/// pattern: format, then `git add`) cannot desync later Batches of the same
/// Run.
///
/// The desync this pins is cross-file: Batch A stages hunk 1 of `alpha.txt`;
/// the hook then restages the *whole* file, so Batch A's commit silently
/// lands alpha's hunk 2 as well — a hunk the plan assigned to Batch B. When
/// Batch B runs, its `alpha.txt` hunk has nothing left to stage. Replaying
/// the plan-time snapshot would die with `git apply`'s "patch does not
/// apply"; the fresh-diff re-read skips the swallowed hunks (with a notice)
/// and still stages + commits the rest of Batch B (`beta.txt`) exactly per
/// plan. The Run survives with: no lost hunks (alpha's hunk 2 landed in
/// Batch A's commit via the hook), no duplicate commits (exactly one commit
/// per Batch), and no unplanned files (Batch A = alpha only, Batch B = beta
/// only).
#[tokio::test]
async fn commit_run_batch_b_survives_pre_commit_restaging_of_whole_files() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // alpha.txt has two change sites far enough apart that git emits two
    // separate hunks (>= ~6 unchanged lines between them); beta.txt has one.
    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    std::fs::write(dir.path().join("alpha.txt"), format!("a0\n{pad}c0\n")).unwrap();
    std::fs::write(dir.path().join("beta.txt"), "b0\n").unwrap();
    git_in(dir.path(), &["add", "alpha.txt", "beta.txt"]);
    git_in(dir.path(), &["commit", "-m", "base"]);
    std::fs::write(dir.path().join("alpha.txt"), format!("a1\n{pad}c1\n")).unwrap();
    std::fs::write(dir.path().join("beta.txt"), "b1\n").unwrap();

    // Simulate lint-staged: the pre-commit hook restages the whole file it
    // formatted, so Batch A's commit of hunk 1 actually lands BOTH of alpha's
    // hunks — hunk 2 was planned for Batch B. (The commit-A content assertion
    // below pins that this hook really ran: without it, hunk 2 would still be
    // unstaged and c1 would not appear in commit A.)
    gh::install_hook(dir.path(), "pre-commit", "git add alpha.txt");

    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change a".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![
                    generator::BatchChange {
                        file: "alpha.txt".to_string(),
                        hunks: vec![2],
                    },
                    generator::BatchChange {
                        file: "beta.txt".to_string(),
                        hunks: vec![1],
                    },
                ],
                reason: Some("change c and b".into()),
            },
        ],
    };

    let buf = BufferWrite::default();
    let git = Git::at(dir.path()).unwrap();
    let before = commit_count(dir.path());
    let (messenger, _msg_calls) = messenger_sequence(&["feat: alpha change", "feat: beta change"]);

    let result = run_commit_workflow_impl(
        &git,
        resolver_returning(""),
        prompt_queue(vec![]),
        Display::with(buf.clone()),
        planner_fixed(plan),
        messenger,
        Confirm::Disabled,
    )
    .await;
    assert!(
        result.is_ok(),
        "the Run must survive a hook that restages whole files: {:?}",
        result
    );

    // No duplicate commits: exactly one commit per Batch. Batch B must not
    // re-commit alpha's hook-landed hunks, and nothing may land twice.
    assert_eq!(
        commit_count(dir.path()),
        before + 2,
        "each Batch must produce exactly one commit"
    );
    // Commit subjects in plan order, oldest first — proof that Batch B still
    // staged and committed per plan rather than being swallowed by the hook's
    // restage.
    let subjects = git_out(dir.path(), &["log", "--reverse", "--format=%s", "-2"]);
    assert_eq!(
        subjects, "feat: alpha change\nfeat: beta change\n",
        "Batch A then Batch B must commit in plan order, got:\n{subjects}"
    );

    // No lost hunks: alpha's hunk 2 was planned for Batch B but landed in
    // Batch A's commit via the hook's whole-file restage...
    let commit_a = file_at_ref(dir.path(), "HEAD~1", "alpha.txt");
    assert!(
        commit_a.contains("a1"),
        "alpha hunk 1 must be in Batch A's commit"
    );
    assert!(
        commit_a.contains("c1"),
        "alpha hunk 2 must land in Batch A's commit via the hook's restage"
    );
    // ...and Batch B still staged + committed its remaining file per plan.
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "beta.txt"),
        "b1\n",
        "Batch B must commit beta.txt per plan"
    );

    // No unplanned files: Batch A touches exactly alpha.txt and Batch B
    // exactly beta.txt — the hook's restage must not drag anything else into
    // a commit.
    let files_a_out = git_out(dir.path(), &["show", "--name-only", "--format=", "HEAD~1"]);
    let files_a: Vec<&str> = files_a_out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        files_a,
        vec!["alpha.txt"],
        "Batch A must touch only alpha.txt"
    );
    let files_b_out = git_out(dir.path(), &["show", "--name-only", "--format=", "HEAD"]);
    let files_b: Vec<&str> = files_b_out.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        files_b,
        vec!["beta.txt"],
        "Batch B must touch only beta.txt"
    );

    // The swallowed hunks are reported with a notice, not silently dropped.
    assert!(
        buf.lines()
            .iter()
            .any(|l| l.contains("alpha.txt") && l.contains("already committed")),
        "the skip of alpha's swallowed hunks must be reported; got: {:?}",
        buf.lines()
    );

    // Nothing left staged or unstaged after the Run.
    assert!(
        worktree_is_empty(dir.path()),
        "working tree must be clean after the Run"
    );
}
