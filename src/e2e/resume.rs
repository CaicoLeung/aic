//! Resume-and-retry for interrupted batch-plan runs.
//!
//! These pin the recovery contract end-to-end against real on-disk repos:
//!   - a mid-loop failure persists in-flight `.aic/active.json` state,
//!   - resume replays the pending batches from the frozen diffs,
//!   - a file mutated since plan time defers its batch (integrity),
//!   - the auto-detected resume offer routes accept→replay / decline→discard.

use super::common::*;

/// `tracked.txt` carrying two change sites (≥8 lines apart → two hunks under
/// git's default context), committed as a base, then both sites rewritten and
/// left unstaged. Returns a 2-batch plan splitting hunk 1 / hunk 2 across
/// batches — the shape that lets a mid-loop failure leave batch 1 committed and
/// batch 2 pending.
fn two_hunk_repo(dir: &Path) -> generator::BatchPlanOutput {
    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}c0\n");
    let changed = format!("a1\n{pad}c1\n");
    std::fs::write(dir.join("tracked.txt"), &base).unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "base"]);
    std::fs::write(dir.join("tracked.txt"), &changed).unwrap();
    generator::BatchPlanOutput {
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
    }
}

/// Drive a 2-batch plan to a partial failure: batch 1 commits, batch 2's
/// message step errors. Leaves `.aic/active.json` on disk with batch 1
/// committed and batch 2 pending. Caller holds `GIT_CWD_MUTEX` + `CwdGuard`.
async fn partial_failure(plan: generator::BatchPlanOutput) {
    let (messenger, _calls) = messenger_then_error(1); // batch 1 ok, batch 2 fails
    let err = run_commit_workflow_impl(
        resolver_returning(""),
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger,
    )
    .await;
    assert!(
        err.is_err(),
        "batch 2 must fail so an in-flight state is left behind"
    );
}

/// A mid-loop failure must persist `.aic/active.json` capturing how far the Run
/// got (batch 1 committed, batch 2 pending), and `.aic/` must stay invisible to
/// `git status` so it never pollutes the next Run's diff.
#[tokio::test]
async fn resume_partial_failure_persists_in_flight_state() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let plan = two_hunk_repo(dir.path());
    let _g = gh::CwdGuard::new(dir.path());
    partial_failure(plan).await;

    let rs = runstate::RunState::load()
        .unwrap()
        .expect("active.json must persist after a partial failure");
    assert_eq!(rs.batches.len(), 2);
    assert!(
        matches!(rs.batches[0], runstate::BatchEntry::Committed { .. }),
        "batch 1 must be recorded committed"
    );
    assert!(
        matches!(rs.batches[1], runstate::BatchEntry::Pending),
        "batch 2 must remain pending"
    );

    // Batch 1's hunk landed in HEAD; batch 2's did not.
    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(head.contains("a1"), "batch 1 must be committed");
    assert!(!head.contains("c1"), "batch 2 must NOT be committed");

    // `.aic/` is self-ignored — it never appears as an untracked entry.
    assert!(
        !status_porcelain(dir.path()).contains(".aic"),
        ".aic/ leaked into git status"
    );
}

/// Resuming an interrupted Run replays the pending batch from its frozen diff
/// (never re-plans), commits it, then clears the in-flight state.
#[tokio::test]
async fn resume_replays_pending_batch_and_clears_state() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let plan = two_hunk_repo(dir.path());
    let _g = gh::CwdGuard::new(dir.path());
    partial_failure(plan).await;

    let rs = runstate::RunState::load()
        .unwrap()
        .expect("state present before resume");
    run_resume_workflow_impl(sink(), messenger_fixed("resumed commit"), rs)
        .await
        .expect("resume replay must succeed");

    // Both hunks are now committed.
    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(
        head.contains("a1") && head.contains("c1"),
        "both batches must be committed after resume"
    );

    // Clean completion deletes the in-flight state.
    assert!(
        runstate::RunState::load().unwrap().is_none(),
        "active.json must be cleared after a completed resume"
    );
}

/// A file the user mutated since plan time must defer its batch on resume — the
/// batch is skipped, its change left unstaged, and the rest of the Run still
/// completes. Guards the integrity (no stale-snapshot replay) contract.
#[tokio::test]
async fn resume_defers_batch_whose_file_changed_since_plan() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    // `two_file_unstaged_repo` includes `init_test_repo` + a committed base
    // for alpha.txt/beta.txt, each left with a one-hunk unstaged change.
    two_file_unstaged_repo(dir.path());

    // Batch 1 = alpha.txt, batch 2 = beta.txt. Fail on batch 2 so batch 1
    // commits and batch 2 stays pending.
    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("alpha".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "beta.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("beta".into()),
            },
        ],
    };
    let _g = gh::CwdGuard::new(dir.path());
    partial_failure(plan).await;

    let commits_before_resume = commit_count(dir.path());

    // Mutate beta.txt after plan time — its fingerprint no longer matches.
    std::fs::write(dir.path().join("beta.txt"), "b1\nmutated since plan\n").unwrap();

    let buf = BufferWrite::default();
    let rs = runstate::RunState::load().unwrap().expect("state present");
    run_resume_workflow_impl(Display::with(buf.clone()), messenger_fixed("beta msg"), rs)
        .await
        .expect("resume with a deferral must still succeed");

    // Batch 2 was deferred, not committed — no new commit landed.
    assert_eq!(
        commit_count(dir.path()),
        commits_before_resume,
        "a deferred batch must not create a commit"
    );
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "beta.txt"),
        "b0\n",
        "beta must remain uncommitted (still at its base)"
    );
    assert_eq!(
        read_file(dir.path(), "beta.txt"),
        "b1\nmutated since plan\n",
        "the mutated change must survive untouched in the workdir"
    );

    // The deferral was reported, and the in-flight state was cleared.
    let rendered = buf.lines().join("\n");
    assert!(
        rendered.contains("deferred"),
        "expected a deferral notice, got: {rendered}"
    );
    assert!(
        runstate::RunState::load().unwrap().is_none(),
        "active.json must be cleared after resume-with-deferral"
    );
}

/// The auto-detected resume offer: accepting routes to the replay path, which
/// commits the remaining batch and clears state.
#[tokio::test]
async fn resume_offer_accepted_routes_to_replay() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let plan = two_hunk_repo(dir.path());
    let _g = gh::CwdGuard::new(dir.path());
    partial_failure(plan).await;

    // Second run: the offer is auto-detected; answer "yes" to resume.
    run_commit_workflow_impl(
        resolver_returning(""),
        prompt_queue(vec![true]), // accept "resume this run?"
        sink(),
        unreachable_planner(), // replay never re-plans
        messenger_fixed("replayed"),
    )
    .await
    .expect("accepted resume must complete");

    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(
        head.contains("a1") && head.contains("c1"),
        "both batches committed after an accepted resume"
    );
    assert!(
        runstate::RunState::load().unwrap().is_none(),
        "state cleared after accepted resume"
    );
}

/// The auto-detected resume offer: declining discards the frozen plan and runs
/// fresh — the in-flight state is gone before the new plan proceeds.
#[tokio::test]
async fn resume_offer_declined_discards_and_runs_fresh() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());
    let plan = two_hunk_repo(dir.path());
    let _g = gh::CwdGuard::new(dir.path());
    partial_failure(plan).await;

    // The failed batch 2 left its hunk staged; unstage so the fresh run sees a
    // clean unstaged change and re-enters the planner path.
    git_in(dir.path(), &["reset", "--quiet", "HEAD"]);
    let commits_before = commit_count(dir.path());

    let buf = BufferWrite::default();
    // Decline the offer, then a fresh 1-batch plan over the remaining hunk.
    let fresh = generator::BatchPlanOutput {
        batches: vec![generator::BatchPlanBatch {
            changes: vec![generator::BatchChange {
                file: "tracked.txt".to_string(),
                hunks: vec![1],
            }],
            reason: Some("remaining".into()),
        }],
    };
    run_commit_workflow_impl(
        resolver_returning(""),
        prompt_queue(vec![false]), // decline "resume this run?"
        Display::with(buf.clone()),
        planner_fixed(fresh),
        messenger_fixed("fresh commit"),
    )
    .await
    .expect("declined resume must run fresh and complete");

    // The remaining change was committed by the fresh plan.
    assert!(
        commit_count(dir.path()) > commits_before,
        "the fresh run must commit the remaining change"
    );
    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(head.contains("c1"), "the remaining hunk must be committed");

    // The previous run's state was discarded.
    let rendered = buf.lines().join("\n");
    assert!(
        rendered.contains("discarded"),
        "expected a discard notice, got: {rendered}"
    );
    assert!(
        runstate::RunState::load().unwrap().is_none(),
        "state cleared after the fresh run"
    );
}
