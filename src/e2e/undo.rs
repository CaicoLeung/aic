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
            redact: Redact::On,
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
/// Run start, returns both files as unstaged additions, and clears both
/// state files (start anchor and Run tip) — the repo is exactly pre-Run,
/// byte for byte.
#[tokio::test]
async fn undo_resets_run_commits_and_unstages_changes() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let start = run_two_batches(&dir).await;

    let git = Git::at(dir.path()).unwrap();
    assert_eq!(git.commit_count(&start, "HEAD").unwrap(), 2);
    assert!(git.git_dir().join("aic/undo-run").exists());
    assert!(git.git_dir().join("aic/undo-run-tip").exists());

    let seen = std::cell::RefCell::new(String::new());
    undo::undo_run(&git, &|label| {
        *seen.borrow_mut() = label.to_string();
        Ok(true)
    })
    .unwrap();
    assert_eq!(git.head_sha().unwrap(), start, "HEAD must be pre-Run");
    let label = seen.into_inner();
    assert!(
        label.contains("2 commits from the run will be reset"),
        "tip-known, no-post-run wording must pin plural-safe phrasing, got: {label}"
    );
    let status = git.status().unwrap();
    let unstaged: Vec<_> = status.iter().filter(|f| !f.staged).collect();
    let paths: Vec<&str> = unstaged.iter().map(|f| f.path.as_str()).collect();
    assert!(
        paths.contains(&"a.txt") && paths.contains(&"b.txt"),
        "{paths:?}"
    );
    assert!(
        !git.git_dir().join("aic/undo-run").exists()
            && !git.git_dir().join("aic/undo-run-tip").exists(),
        "state files must be cleared after undo"
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

/// The staged single-commit path is as undoable as the batch path: the
/// Run anchors are written around the shared commit step, so a Run over
/// pre-staged files records, commits, and resets back cleanly.
#[tokio::test]
async fn undo_covers_staged_single_commit_run() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let git = Git::at(dir.path()).unwrap();
    let start = git.head_sha().unwrap();
    std::fs::write(dir.path().join("staged.txt"), "staged\n").unwrap();
    git.add(&["staged.txt"]).unwrap();

    commit_run(
        &git,
        RunDeps {
            redact: Redact::On,
            display: sink(),
            planner: unreachable_planner(),
            messenger: messenger_fixed("feat: stub"),
            confirm: Confirm::Disabled,
        },
    )
    .await
    .unwrap();

    assert_eq!(git.commit_count(&start, "HEAD").unwrap(), 1);
    assert!(git.git_dir().join("aic/undo-run").exists());
    assert!(git.git_dir().join("aic/undo-run-tip").exists());

    undo::undo_run(&git, &|_| Ok(true)).unwrap();

    assert_eq!(git.head_sha().unwrap(), start);
    assert!(
        git.status()
            .unwrap()
            .iter()
            .any(|f| f.path == "staged.txt" && !f.staged),
        "the staged Run's file must return unstaged"
    );
}

/// Commits made *after* the Run (the user's own) are disclosed in the
/// confirmation — counted separately from the Run's — because the reset
/// strips them too; their changes still return to the working tree.
#[tokio::test]
async fn undo_prompt_discloses_commits_made_after_the_run() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let start = run_two_batches(&dir).await;

    // The user's own commit after the Run — inside the reset's blast
    // radius (start..HEAD), outside the Run's own commits (start..tip).
    std::fs::write(dir.path().join("mine.txt"), "mine\n").unwrap();
    let git = Git::at(dir.path()).unwrap();
    git.add(&["mine.txt"]).unwrap();
    git.run_git(&["commit", "-m", "user commit"], None, &[])
        .unwrap();

    let seen = std::cell::RefCell::new(String::new());
    undo::undo_run(&git, &|label| {
        *seen.borrow_mut() = label.to_string();
        Ok(true)
    })
    .unwrap();

    let label = seen.into_inner();
    assert!(
        label.contains("3 commits will be reset")
            && label.contains("2 commits from the run")
            && label.contains("1 commit made after it"),
        "prompt must disclose the split, got: {label}"
    );
    assert_eq!(git.head_sha().unwrap(), start);
    assert!(
        git.status()
            .unwrap()
            .iter()
            .any(|f| f.path == "mine.txt" && !f.staged),
        "the user's post-Run changes must survive, unstaged"
    );
}

/// The Run's own commits rewritten — HEAD amended while sitting on the
/// Run tip, so the start anchor is still an ancestor but the recorded
/// tip no longer is → undo must refuse on the *tip* check, the branch
/// the start-only proof cannot catch.
#[tokio::test]
async fn undo_refuses_when_run_tip_was_rewritten() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let _start = run_two_batches(&dir).await;

    let git = Git::at(dir.path()).unwrap();
    git.run_git(
        &["commit", "--amend", "-m", "rewritten run commit"],
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

/// Without a tip anchor (a Run predating tip tracking, or a crash between
/// start and first commit) the run/after split is unavailable — the prompt
/// must disclose that instead of mislabeling every commit as the Run's own.
#[test]
fn undo_without_tip_discloses_unknown_split() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let git = Git::at(dir.path()).unwrap();
    let start = git.head_sha().unwrap();
    undo::record(&git, &start).unwrap(); // start anchor only — no tip file
    std::fs::write(dir.path().join("c.txt"), "c\n").unwrap();
    git.add(&["c.txt"]).unwrap();
    git.run_git(&["commit", "-m", "x"], None, &[]).unwrap();

    let seen = std::cell::RefCell::new(String::new());
    undo::undo_run(&git, &|label| {
        *seen.borrow_mut() = label.to_string();
        Ok(true)
    })
    .unwrap();

    let label = seen.into_inner();
    assert!(
        label.contains("1 commit since the run started will be reset")
            && label.contains("run tip unknown"),
        "must disclose the unknown split, got: {label}"
    );
    assert_eq!(git.head_sha().unwrap(), start);
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
            redact: Redact::On,
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
