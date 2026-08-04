//! `aic resolve` workflow (ADR 0005).

use super::common::*;

/// `aic resolve` on a clean repo short-circuits and never calls the resolver
/// or the prompt.
#[tokio::test]
async fn resolve_clean_repo_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let (resolver, seen) = resolver_recording();
    let prompt = prompt_queue(vec![]); // empty — must not be asked
    let git = Git::at(dir.path()).unwrap();

    let result = run_resolve_workflow_impl(&git, resolver, prompt, sink()).await;
    assert!(result.is_ok(), "clean repo should not error: {:?}", result);
    assert!(
        seen.lock().is_empty(),
        "resolver must not run on clean repo"
    );
    assert!(is_clean(dir.path()));
}

/// Contract shared by every `aic resolve` refusal (ADR 0005): rebase and am
/// states are detected but never finalized in v1. Pinned once here so each
/// refusal path is a one-line test — a bail message naming the state, no
/// Resolver call, and the repo left in its conflicted state.
async fn assert_resolve_refused(setup: fn(&Path), label: &str) {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());

    let (resolver, seen) = resolver_recording();
    let git = Git::at(dir.path()).unwrap();

    let err = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![]), sink())
        .await
        .expect_err("refused state must error, not succeed");
    let msg = format!("{err:#}");
    // Pin the literal "<label> state" phrase, not just the bare label — a bare
    // `contains("am")` would false-pass on common words like "stream".
    assert!(
        msg.contains(&format!("{label} state")) && msg.contains("v1"),
        "expected {label} refusal, got: {msg}"
    );
    assert!(
        seen.lock().is_empty(),
        "resolver must not run on refused state"
    );
    assert!(!is_clean(dir.path()), "{label} must not be finalized");
}

/// `aic resolve` on a rebase state is detected but refused in v1 (ADR 0005).
#[tokio::test]
async fn resolve_refuses_rebase_state() {
    assert_resolve_refused(rebase_conflict, "rebase").await;
}

/// `aic resolve` on an am (ApplyMailbox) state is detected but refused in v1
/// (ADR 0005). The am path shares the rebase bail code but had no test
/// before (issue #33): this pins the same refusal contract.
#[tokio::test]
async fn resolve_refuses_am_state() {
    assert_resolve_refused(am_conflict, "am").await;
}

/// Marquee happy path: one content conflict → resolver → review → approve →
/// finalize. Repo ends clean, file holds the resolution, no markers remain.
#[tokio::test]
async fn resolve_full_flow_finalizes_merge() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let resolver = resolver_returning("merged\n");
    let git = Git::at(dir.path()).unwrap();

    let result = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![true]), sink()).await;
    assert!(result.is_ok(), "happy path should succeed: {:?}", result);

    assert!(is_clean(dir.path()), "merge must be finalized");
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
    assert!(!file_has_markers(dir.path(), "tracked.txt"));
}

/// Sticky partial approval (ADR 0005): approve one file, reject the other.
/// The approved file is staged clean; the rejected one stays unmerged with
/// markers; the merge is *not* finalized (git's `--continue` would block).
#[tokio::test]
async fn resolve_partial_approval_keeps_approved_staged() {
    let dir = tempfile::tempdir().unwrap();
    merge_two_conflicts(dir.path());

    let resolver = resolver_returning("merged\n");
    // Path-based approval so the verdict follows the file regardless of the
    // order `conflicted_files()` returns them in: approve tracked.txt, reject
    // second.txt. (A position-based queue would be order-dependent.)
    let prompt: Prompt = Box::new(|label: &str| Ok(label.contains("tracked.txt")));
    let git = Git::at(dir.path()).unwrap();

    let result = run_resolve_workflow_impl(&git, resolver, prompt, sink()).await;
    assert!(
        result.is_ok(),
        "partial approval handoff should not error: {:?}",
        result
    );

    assert!(
        !is_clean(dir.path()),
        "must not finalize with a rejected file"
    );

    // Approved: written + staged clean.
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
    assert!(!staged_blob_has_markers(dir.path(), "tracked.txt"));
    assert!(!is_unmerged(dir.path(), "tracked.txt"));

    // Rejected: untouched, still unmerged with markers.
    assert!(file_has_markers(dir.path(), "second.txt"));
    assert!(is_unmerged(dir.path(), "second.txt"));
}

/// Unresolvable (binary) files are skipped per-file and do not abort the run,
/// but they do block finalize. The resolvable text file is still resolved +
/// staged — partial progress is preserved (ADR 0005).
#[tokio::test]
async fn resolve_skips_binary_and_stages_text() {
    let dir = tempfile::tempdir().unwrap();
    merge_text_and_binary(dir.path());

    let resolver = resolver_returning("merged\n");
    let git = Git::at(dir.path()).unwrap();

    let result = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![true]), sink()).await;
    assert!(
        result.is_ok(),
        "binary skip should hand off, not error: {:?}",
        result
    );

    assert!(!is_clean(dir.path()), "binary conflict blocks finalize");

    // Text file resolved + staged clean.
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
    assert!(!staged_blob_has_markers(dir.path(), "tracked.txt"));

    // Binary file still unmerged (aic can't resolve it).
    assert!(is_unmerged(dir.path(), "binary.bin"));

    // And classify confirms it was seen as Binary.
    let files = git.conflicted_files().unwrap();
    let binary = files
        .iter()
        .find(|f| f.path == "binary.bin")
        .expect("binary.bin should be a conflicted file");
    assert_eq!(binary.kind, git::ConflictKind::Binary);
}

/// Marker validation auto-retry (ADR 0005): the LLM returns markers on the
/// first call, clean content on the retry. The file is resolved + finalized.
#[tokio::test]
async fn resolve_retries_after_markers_then_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, calls) =
        resolver_then("<<<<<<< HEAD\nbad\n=======\nworse\n>>>>>>> x\n", "merged\n");
    let git = Git::at(dir.path()).unwrap();

    let result = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![true]), sink()).await;
    assert!(
        result.is_ok(),
        "retry-then-clean should succeed: {:?}",
        result
    );

    assert_eq!(*calls.lock(), 2, "exactly one retry (2 calls)");
    assert!(is_clean(dir.path()), "merge must be finalized after retry");
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
}

/// If the LLM keeps returning markers after the retry, the file is skipped as
/// failed, `plans` is empty, and the workflow bails with the explicit
/// "no files could be resolved" message.
#[tokio::test]
async fn resolve_gives_up_when_markers_persist() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, calls) = resolver_always_markers();
    let git = Git::at(dir.path()).unwrap();

    let err = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![]), sink())
        .await
        .expect_err("must bail when no file could be resolved");
    assert!(
        format!("{err:#}").contains("no files could be resolved"),
        "expected give-up message, got: {err:#}"
    );
    assert_eq!(*calls.lock(), 2, "one attempt + one retry");
    assert!(!is_clean(dir.path()), "merge must not be finalized");
    assert!(
        file_has_markers(dir.path(), "tracked.txt"),
        "file left untouched"
    );
}

/// Markers-then-error refinement (#83): the first attempt returns marker-laden
/// output (retryable), but the retry itself fails with an LLM error. The file
/// is still soft-skipped as `failed` and the workflow bails — but the emitted
/// skip reason is the truthful "LLM error", not the old catch-all "markers
/// remain after retry". The retry-attempt error must not be masked.
#[tokio::test]
async fn resolve_reports_llm_error_when_retry_fails() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, calls) = resolver_then_error(
        "<<<<<<< HEAD\nbad\n=======\nworse\n>>>>>>> x\n",
        "LLM unreachable on retry",
    );
    let git = Git::at(dir.path()).unwrap();

    let buf = BufferWrite::default();
    let display = Display::with(buf.clone());
    let err = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![]), display)
        .await
        .expect_err("must bail when the only file's resolution fails");
    assert!(
        format!("{err:#}").contains("no files could be resolved"),
        "expected bail, got: {err:#}"
    );
    assert_eq!(*calls.lock(), 2, "one attempt + one retry");

    // The truthful reason: the retry failed, so markers do *not* remain.
    let lines = buf.lines();
    let skip = lines
        .iter()
        .find(|l| l.contains("skipped"))
        .expect("expected a per-file skipped line");
    assert!(
        skip.contains("LLM error:") && skip.contains("LLM unreachable on retry"),
        "skip must report the retry error, got: {skip:?}"
    );
    assert!(
        !skip.contains("markers remain after retry"),
        "retry error must not be masked as markers-remain, got: {skip:?}"
    );

    assert!(!is_clean(dir.path()), "merge must not be finalized");
    assert!(
        file_has_markers(dir.path(), "tracked.txt"),
        "file left untouched after LLM error"
    );
}

/// Conflicted state but the index has no unmerged entries (user resolved every
/// file by hand): the workflow offers finalize, and on `yes` runs git's
/// finalize. The resolver must not be invoked.
#[tokio::test]
async fn resolve_offers_finalize_when_all_manual() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    // Resolve by hand + stage, leaving the repo in the Merge state with no
    // unmerged entries.
    std::fs::write(dir.path().join("tracked.txt"), "hand-merged\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let (resolver, seen) = resolver_recording();
    let git = Git::at(dir.path()).unwrap();

    let result = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![true]), sink()).await;
    assert!(
        result.is_ok(),
        "manual-finalize should succeed: {:?}",
        result
    );
    assert!(
        seen.lock().is_empty(),
        "resolver must not run when nothing's unmerged"
    );
    assert!(is_clean(dir.path()), "merge must be finalized");
}

/// The escape hatch of [`resolve_offers_finalize_when_all_manual`]: same
/// setup (mid-merge, every file resolved + staged by hand so the index has
/// no unmerged entries), but the user declines `finalize now?`. No Finalize
/// operation runs, the repo is left untouched in its conflicted state, the
/// Resolver is never called, and the workflow returns `Ok` — the user's
/// "I staged my hand-merge but I'm not ready to commit yet" stays intact.
/// Pinned by issue #28, which guards this branch alongside its `yes`
/// counterpart so a regression that silently finalizes (or errors) on a
/// decline would fail loudly instead of shipping green.
#[tokio::test]
async fn resolve_declines_finalize_when_all_manual() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    // Resolve by hand + stage, leaving the repo in the Merge state with no
    // unmerged entries — identical entry condition to the `yes` test.
    std::fs::write(dir.path().join("tracked.txt"), "hand-merged\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let (resolver, seen) = resolver_recording();
    let git = Git::at(dir.path()).unwrap();

    // Confirm the entry condition holds before the workflow runs: mid-merge,
    // but nothing unmerged in the index. (`Git::state` / `conflicted_files`
    // read the process CWD, so the guard must already be held.)
    assert_eq!(
        git.state().unwrap(),
        git::RepoState::Merge,
        "setup must leave the repo mid-merge"
    );
    assert!(
        git.conflicted_files().unwrap().is_empty(),
        "setup must leave no unmerged entries"
    );

    let before = commit_count(dir.path());

    // Answer "finalize now?" with no — the only prompt on this path.
    let result = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![false]), sink()).await;
    assert!(
        result.is_ok(),
        "declining finalize should not error: {:?}",
        result
    );

    // No finalize ran: the repo is still mid-merge, and no commit landed.
    assert_eq!(
        git.state().unwrap(),
        git::RepoState::Merge,
        "repo must stay mid-merge when finalize is declined"
    );
    assert!(!is_clean(dir.path()), "decline must not finalize the merge");
    assert_eq!(
        commit_count(dir.path()),
        before,
        "declining finalize must not create a commit"
    );

    // The hand-merge is untouched on disk and in the index: the staged blob
    // is exactly what the user staged, with no markers and no re-resolution.
    assert_eq!(
        read_file(dir.path(), "tracked.txt"),
        "hand-merged\n",
        "hand-merge must be preserved untouched"
    );
    assert!(!staged_blob_has_markers(dir.path(), "tracked.txt"));

    // The Resolver was never reached — declining finalize must short-circuit
    // before any per-file work (and there are no unmerged files to resolve
    // anyway).
    assert!(
        seen.lock().is_empty(),
        "resolver must not run when finalize is declined"
    );
}

/// Resolve a setup conflict and assert the workflow finalizes the repo back to
/// a clean, empty working tree with the resolved content (`"merged\n"`).
/// Shared tail of the four finalize-state tests (`CherryPick`/`Revert` and
/// their `*Sequence` siblings — ADR 0005, issue #29). Each test owns its
/// setup and its own `Git` handle, then hands off here; the `*Sequence` tests append
/// their own "clean first item landed" assertion afterwards.
///
/// `expected` is asserted first so a setup regression — e.g. a sequence
/// collapsing to its single-shot sibling — trips before the finalize claim is
/// tested. `is_clean` alone only proves no operation is in progress; a
/// finalize that committed the resolution but left a stray staged or untracked
/// entry would still pass it, so the strict `worktree_is_empty` guarantee is
/// pinned too.
async fn resolve_finalizes_clean(dir: &Path, expected: git::RepoState) {
    let git = Git::at(dir).unwrap();
    assert_eq!(
        git.state().unwrap(),
        expected,
        "setup must leave the repo in the expected conflict state"
    );

    let resolver = resolver_returning("merged\n");

    let result = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![true]), sink()).await;
    assert!(
        result.is_ok(),
        "{expected:?} resolve should succeed: {:?}",
        result
    );

    assert!(is_clean(dir), "{expected:?} must be finalized");
    assert!(
        worktree_is_empty(dir),
        "{expected:?} must leave nothing staged or untracked"
    );
    assert_eq!(read_file(dir, "tracked.txt"), "merged\n");
    assert!(!file_has_markers(dir, "tracked.txt"));
}

/// Cherry-pick conflict finalized end-to-end (ADR 0005): `git cherry-pick
/// --continue` actually clears the `CherryPick` state in a real repo, not
/// merely the `finalize_invocation` enum mapping (unit-tested in git.rs).
#[tokio::test]
async fn resolve_finalizes_cherry_pick() {
    let dir = tempfile::tempdir().unwrap();
    cherry_pick_conflict(dir.path());
    resolve_finalizes_clean(dir.path(), git::RepoState::CherryPick).await;
}

/// Revert conflict finalized end-to-end (ADR 0005): `git revert --continue`
/// clears the `Revert` state in a real repo.
#[tokio::test]
async fn resolve_finalizes_revert() {
    let dir = tempfile::tempdir().unwrap();
    revert_conflict(dir.path());
    resolve_finalizes_clean(dir.path(), git::RepoState::Revert).await;
}

/// Cherry-pick *sequence* finalize (issue #29): a multi-commit `git cherry-pick
/// A B` activates the sequencer, so the repo sits in `CherryPickSequence` —
/// distinct from the single-shot [`resolve_finalizes_cherry_pick`]. Both map
/// to the same `cherry-pick --continue` Finalize; this pins that it drains the
/// sequencer and clears the *sequence* state in a real repo. Shared
/// post-conditions in [`resolve_finalizes_clean`].
#[tokio::test]
async fn resolve_finalizes_cherry_pick_sequence() {
    let dir = tempfile::tempdir().unwrap();
    cherry_pick_sequence_conflict(dir.path());
    resolve_finalizes_clean(dir.path(), git::RepoState::CherryPickSequence).await;
    // The clean first commit (adding extra.txt) must land before the second
    // hits its conflict — its presence proves the sequence advanced past A.
    assert_eq!(read_file(dir.path(), "extra.txt"), "feature\n");
}

/// Revert *sequence* finalize (issue #29): a multi-commit `git revert P Q`
/// activates the sequencer, so the repo sits in `RevertSequence` — distinct
/// from the single-shot [`resolve_finalizes_revert`]. Both map to the same
/// `revert --continue` Finalize; this pins that it drains the sequencer and
/// clears the *sequence* state in a real repo. Shared post-conditions in
/// [`resolve_finalizes_clean`].
#[tokio::test]
async fn resolve_finalizes_revert_sequence() {
    let dir = tempfile::tempdir().unwrap();
    revert_sequence_conflict(dir.path());
    resolve_finalizes_clean(dir.path(), git::RepoState::RevertSequence).await;
    // The clean first revert (removing fileP.txt) must land before the second
    // hits its conflict — its absence proves the sequence advanced past P.
    assert!(!dir.path().join("fileP.txt").exists());
}

/// Delete/modify conflict (master deleted, other modified) is classified
/// `DeleteModify` and skipped — never reaches the LLM. With no resolvable
/// files, the workflow bails (ADR 0005: structural conflicts need manual
/// resolution).
#[tokio::test]
async fn resolve_skips_delete_modify_conflict() {
    let dir = tempfile::tempdir().unwrap();
    merge_delete_modify_conflict(dir.path());

    let (resolver, seen) = resolver_recording();
    let git = Git::at(dir.path()).unwrap();

    let files = git.conflicted_files().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "tracked.txt");
    assert_eq!(files[0].kind, git::ConflictKind::DeleteModify);

    let err = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![]), sink())
        .await
        .expect_err("delete/modify has no resolvable file");
    assert!(
        format!("{err:#}").contains("no files could be resolved"),
        "expected bail, got: {err:#}"
    );
    assert!(
        seen.lock().is_empty(),
        "resolver must not run on a DeleteModify file"
    );
    assert!(!is_clean(dir.path()), "merge must not be finalized");
}

/// An oversized text file (> `MAX_CONFLICT_LINES`) is classified `Oversized`
/// and skipped, while a normal text conflict in the same merge still resolves +
/// stages (ADR 0005: oversized doesn't abort the run). Finalize is blocked by
/// the oversized file.
#[tokio::test]
async fn resolve_skips_oversized_and_stages_text() {
    let dir = tempfile::tempdir().unwrap();
    merge_oversized_and_text(dir.path());

    let resolver = resolver_returning("merged\n");
    let git = Git::at(dir.path()).unwrap();

    let result = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![true]), sink()).await;
    assert!(
        result.is_ok(),
        "oversized skip should hand off, not error: {:?}",
        result
    );

    assert!(!is_clean(dir.path()), "oversized conflict blocks finalize");

    // Small text file resolved + staged clean.
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
    assert!(!staged_blob_has_markers(dir.path(), "tracked.txt"));

    // Big file classified Oversized, still unmerged.
    let files = git.conflicted_files().unwrap();
    let big = files
        .iter()
        .find(|f| f.path == "big.txt")
        .expect("big.txt should be conflicted");
    assert!(
        matches!(big.kind, git::ConflictKind::Oversized { .. }),
        "expected Oversized, got {:?}",
        big.kind
    );
    assert!(is_unmerged(dir.path(), "big.txt"));
}

/// An LLM error on the only resolvable file skips it as `failed` and bails
/// (plans empty) — same workflow outcome as persistent markers, but via the
/// `skipped_failed` arm. The file is left untouched for a re-run. Errors do
/// not trigger the marker-retry path (only marker-laden `Ok` does).
#[tokio::test]
async fn resolve_llm_error_bails_when_only_file_fails() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, calls) = resolver_error();
    let git = Git::at(dir.path()).unwrap();

    let err = run_resolve_workflow_impl(&git, resolver, prompt_queue(vec![]), sink())
        .await
        .expect_err("must bail when the LLM call fails");
    assert!(
        format!("{err:#}").contains("no files could be resolved"),
        "expected bail, got: {err:#}"
    );
    assert_eq!(
        *calls.lock(),
        1,
        "resolver called once; errors do not retry"
    );
    assert!(!is_clean(dir.path()), "merge must not be finalized");
    assert!(
        file_has_markers(dir.path(), "tracked.txt"),
        "file left untouched after LLM error"
    );
}

/// Mixed-blocker scenario (1 approved + 1 rejected + 1 LLM-error + 1 binary)
/// drives every blocker category into the hand-off message at once. Before the
/// `DisplayWrite` seam, this wording was asserted by zero tests: the three
/// existing single-blocker tests only assert repo state, so a regression that
/// swapped labels, dropped a category, or mis-counted would ship green. This
/// test captures the emitted line via [`BufferWrite`] and pins the full
/// three-way breakdown on a single line.
#[tokio::test]
async fn resolve_handoff_lists_all_three_blocker_kinds() {
    let dir = tempfile::tempdir().unwrap();
    merge_mixed_blockers(dir.path());

    // Fail exactly `third.txt` (its content carries the LLM_FAIL sentinel);
    // the other text files resolve to "merged\n".
    let (resolver, calls) = resolver_error_on("LLM_FAIL");
    // Approve tracked.txt, reject second.txt — path-based so it follows the
    // file regardless of the order conflicted_files() returns them in.
    let prompt: Prompt = Box::new(|label: &str| Ok(label.contains("tracked.txt")));
    let git = Git::at(dir.path()).unwrap();

    let buf = BufferWrite::default();
    let display = Display::with(buf.clone());
    let result = run_resolve_workflow_impl(&git, resolver, prompt, display).await;
    assert!(
        result.is_ok(),
        "mixed-blocker handoff should not error: {:?}",
        result
    );

    // --- repo state: approved staged, the other three still blocking ---
    assert!(
        !is_clean(dir.path()),
        "binary + rejected + failed block finalize"
    );
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
    assert!(!staged_blob_has_markers(dir.path(), "tracked.txt"));
    assert!(!is_unmerged(dir.path(), "tracked.txt"));
    // Rejected: untouched, still unmerged with markers.
    assert!(file_has_markers(dir.path(), "second.txt"));
    assert!(is_unmerged(dir.path(), "second.txt"));
    // Failed: untouched, still unmerged with markers.
    assert!(file_has_markers(dir.path(), "third.txt"));
    assert!(is_unmerged(dir.path(), "third.txt"));
    // Binary: aic can't resolve, still unmerged.
    assert!(is_unmerged(dir.path(), "binary.bin"));

    assert_eq!(
        *calls.lock(),
        3,
        "resolver runs once per resolvable text file (errors do not retry)"
    );

    // --- the whole point: the emitted hand-off line carries all three blocker
    // categories with correct counts on one line, plus the single approved
    // resolution reported separately (not conflated with the blockers). ---
    assert_not_finalized_handoff(
        &buf,
        1,
        &[
            "1 rejected",
            "1 failed to resolve",
            "1 need manual resolution",
        ],
    );

    // The buffer sink reports no color capability, so nothing it captures
    // should carry ANSI escapes — guards the sink-derived `colors` fix.
    assert!(
        buf.lines().iter().all(|l| !l.contains('\u{1b}')),
        "buffer sink must emit plain text (no ANSI), got: {:?}",
        buf.lines()
    );
}
