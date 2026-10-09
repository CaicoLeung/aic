//! `aic undo` e2e — the Run-start record, the reset-back core, and every
//! refusal path (no state, zero-commit run, rewritten history, declined
//! prompt). Drives `commit_run` over a real repo to land actual commits,
//! then `undo_run` with a scripted prompt.

use super::common::*;

use crate::workflow::undo;

/// Drive one full Run (planner + messenger stubs, no confirmation) over a
/// two-batch plan: hunk 1 of `a.txt` then hunk 1 of `b.txt`, each its own
/// commit. Returns the Run-start HEAD OID captured before the Run.
async fn run_two_batches(dir: &tempfile::TempDir) -> String {
    let git = Git::at(dir.path()).unwrap();
    let start = git.head_sha().unwrap();

    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();

    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "a.txt".into(),
                    hunks: vec![1],
                }],
                reason: Some("add a".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "b.txt".into(),
                    hunks: vec![1],
                }],
                reason: Some("add b".into()),
            },
        ],
    };

    commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("feat: stub"),
            confirm: Confirm::Disabled,
        },
    )
    .await
    .unwrap();
    start
}

/// The happy path: after a two-commit Run, `aic undo` resets HEAD back to the
/// Run start, returns both files as unstaged additions, and clears the state
/// file — the repo is exactly pre-Run, byte for byte.
#[tokio::test]
async fn undo_resets_run_commits_and_unstages_changes() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let start = run_two_batches(&dir).await;

    let git = Git::at(dir.path()).unwrap();
    assert_eq!(git.commit_count(&start, "HEAD").unwrap(), 2);
    assert!(git.git_dir().join("aic/undo-run").exists());

    undo::undo_run(&git, &|_| Ok(true)).unwrap();

    assert_eq!(git.head_sha().unwrap(), start, "HEAD must be pre-Run");
    let status = git.status().unwrap();
    let unstaged: Vec<_> = status.iter().filter(|f| !f.staged).collect();
    let paths: Vec<&str> = unstaged.iter().map(|f| f.path.as_str()).collect();
    assert!(
        paths.contains(&"a.txt") && paths.contains(&"b.txt"),
        "{paths:?}"
    );
    assert!(
        !git.git_dir().join("aic/undo-run").exists(),
        "state file must be cleared after undo"
    );
}

/// Declining the confirmation changes nothing: same HEAD, same state file.
#[tokio::test]
async fn undo_declined_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let start = run_two_batches(&dir).await;

    let git = Git::at(dir.path()).unwrap();
    let err = undo::undo_run(&git, &|_| Ok(false)).unwrap_err();
    assert!(
        format!("{err:#}").contains("aborted"),
        "expected abort, got: {err:#}"
    );
    assert_eq!(git.commit_count(&start, "HEAD").unwrap(), 2);
    assert!(git.git_dir().join("aic/undo-run").exists());
}

/// No recorded Run → a clear refusal, not a crash or a silent success.
#[test]
fn undo_without_state_file_refuses() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let git = Git::at(dir.path()).unwrap();
    let err = undo::undo_run(&git, &|_| Ok(true)).unwrap_err();
    assert!(
        format!("{err:#}").contains("no aic run to undo"),
        "got: {err:#}"
    );
}

/// A recorded start with zero commits since (the Run recorded, then its
/// commit failed) is a stale record: reported as nothing-to-undo and
/// cleared, so the next undo isn't stuck on it.
#[test]
fn undo_after_zero_commit_run_clears_stale_state() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let git = Git::at(dir.path()).unwrap();
    undo::record(&git, &git.head_sha().unwrap()).unwrap();

    let err = undo::undo_run(&git, &|_| Ok(true)).unwrap_err();
    let msg = format!("{err:#}");
    assert!(
        msg.contains("made no commits") && msg.contains("stale state cleared"),
        "got: {msg}"
    );
    assert!(!git.git_dir().join("aic/undo-run").exists());
}

/// History rewritten past the recorded start (the baseline itself was
/// amended or reset away) → undo refuses and points at the reflog instead of
/// orphaning commits the user never meant to hand back.
#[tokio::test]
async fn undo_refuses_when_history_rewritten_past_start() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let _start = run_two_batches(&dir).await;

    let git = Git::at(dir.path()).unwrap();
    // Rewind past the Run's commits to the recorded start (the baseline),
    // then rewrite the baseline itself — the recorded start is no longer
    // an ancestor of HEAD.
    git.reset_mixed("HEAD~2").unwrap();
    std::fs::write(dir.path().join("tracked.txt"), "rewritten\n").unwrap();
    git.add(&["tracked.txt"]).unwrap();
    git.run_git(
        &["commit", "--amend", "-m", "rewritten baseline"],
        None,
        &[],
    )
    .unwrap();

    let err = undo::undo_run(&git, &|_| Ok(true)).unwrap_err();
    assert!(
        format!("{err:#}").contains("refusing to undo"),
        "got: {err:#}"
    );
}

/// A Run that aborts mid-loop (batch 2's draft fails after batch 1
/// committed) is still undoable — the record is written before the first
/// commit, not after the Run.
#[tokio::test]
async fn partial_run_is_undoable_for_landed_batches() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let git = Git::at(dir.path()).unwrap();
    let start = git.head_sha().unwrap();
    std::fs::write(dir.path().join("a.txt"), "one\n").unwrap();
    std::fs::write(dir.path().join("b.txt"), "two\n").unwrap();

    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "a.txt".into(),
                    hunks: vec![1],
                }],
                reason: Some("add a".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "b.txt".into(),
                    hunks: vec![1],
                }],
                reason: Some("add b".into()),
            },
        ],
    };
    // Drafts fan out concurrently (ADR 0014): both batches' messengers run
    // before any commit, so an error on the second draft surfaces after
    // batch 1 has committed — the partial-failure contract under test.
    let (messenger, _calls) = messenger_then_error(1);
    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger,
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(result.is_err(), "partial run must error");

    assert_eq!(git.commit_count(&start, "HEAD").unwrap(), 1);
    undo::undo_run(&git, &|_| Ok(true)).unwrap();
    assert_eq!(git.head_sha().unwrap(), start);
}
