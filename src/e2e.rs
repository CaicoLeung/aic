//! End-to-end tests for the `aic resolve` feature (ADR 0005).
//!
//! These drive the full workflow functions (`run_resolve_workflow_impl`,
//! `run_commit_workflow_impl`) against real on-disk git repositories in
//! tempdirs, with the LLM resolver and the y/n prompt replaced by stubs. Git
//! stays real: we set up actual merge / rebase conflicts via the `git` CLI and
//! libgit2, then assert on the resulting repo state (state machine, index
//! blobs, working-tree contents, finalize commit).
//!
//! Why stub the LLM and not the git layer: the LLM call is a thin wrapper over
//! a third-party HTTP client — its correctness is rig's problem, not aic's.
//! The feature's logic lives in the orchestration around it (state detection →
//! classification → per-file resolution → marker validation → sticky staging →
//! finalize-gating), and that is exactly what these tests exercise against a
//! real repository.

#![cfg(test)]
// Each e2e test holds `GIT_CWD_MUTEX` across its workflow `.await` by design:
// the whole point is to pin the process CWD to the tempdir for the duration of
// the real git operations the workflow drives. Tests serialize on this single
// mutex and run on single-threaded runtimes, so the guard can't deadlock.
#![allow(clippy::await_holding_lock)]

use crate::git;
use crate::git::tests as gh;
use crate::{BoxFuture, Prompt, Resolver, run_commit_workflow_impl, run_resolve_workflow_impl};
use git2::Repository;
use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------
// Test seams: a boxed resolver future and a queue-backed prompt.
// ---------------------------------------------------------------------

// Erased resolver / prompt closures. Both are `Box<dyn Fn>` (which itself
// implements `Fn`), so they pass straight into the workflow impls' concrete
// `Resolver` / `Prompt` parameters. `BoxFuture`, `Resolver`, and `Prompt` are
// re-used from the crate root so the stub types stay identical to the seam's.

fn resolver_returning(answer: &str) -> Resolver {
    let answer = answer.to_string();
    Box::new(
        move |_content: String| -> BoxFuture<anyhow::Result<String>> {
            let a = answer.clone();
            Box::pin(async move { Ok(a) })
        },
    )
}

/// Resolver that returns a marker-laden string every call. Exercises the
/// "markers remain after retry" skip path. Returns a call counter.
fn resolver_always_markers() -> (Resolver, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0u32));
    let calls2 = calls.clone();
    let marker = "<<<<<<< HEAD\nx\n=======\ny\n>>>>>>> other\n".to_string();
    let r: Resolver = Box::new(
        move |_content: String| -> BoxFuture<anyhow::Result<String>> {
            *calls2.lock().unwrap() += 1;
            let m = marker.clone();
            Box::pin(async move { Ok(m) })
        },
    );
    (r, calls)
}

/// First call returns `first`, every later call returns `then`. Exercises the
/// marker-retry path (LLM returns markers, retry returns clean).
fn resolver_then(first: &str, then: &str) -> (Resolver, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0u32));
    let calls2 = calls.clone();
    let first = first.to_string();
    let then = then.to_string();
    let r: Resolver = Box::new(
        move |_content: String| -> BoxFuture<anyhow::Result<String>> {
            let n = {
                let mut g = calls2.lock().unwrap();
                *g += 1;
                *g
            };
            let out = if n == 1 { first.clone() } else { then.clone() };
            Box::pin(async move { Ok(out) })
        },
    );
    (r, calls)
}

/// Resolver that records every input it was called with (for asserting the
/// resolver was *not* reached on early-exit paths).
fn resolver_recording() -> (Resolver, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    let r: Resolver = Box::new(
        move |content: String| -> BoxFuture<anyhow::Result<String>> {
            seen2.lock().unwrap().push(content);
            Box::pin(async move { Ok("resolved\n".to_string()) })
        },
    );
    (r, seen)
}

/// Prompt that pops answers from a queue; panics if exhausted so an
/// under-specified test fails loudly instead of silently defaulting.
fn prompt_queue(answers: Vec<bool>) -> Prompt {
    let q = Arc::new(Mutex::new(VecDeque::from(answers)));
    Box::new(move |_label: &str| match q.lock().unwrap().pop_front() {
        Some(b) => Ok(b),
        None => panic!("prompt_queue exhausted — test did not provide enough answers"),
    })
}

// ---------------------------------------------------------------------
// Shared repo setup helpers (git stays real).
// ---------------------------------------------------------------------

/// Run a `git` command in `dir`, ignoring exit status (merge / rebase return
/// non-zero on conflict, which is the whole point of these setups).
fn git_in(dir: &Path, args: &[&str]) {
    let _ = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(args)
        .status();
}

/// `init_test_repo` + diverge `tracked.txt` on two branches, then merge so the
/// repo ends in the Merge state with a content conflict in `tracked.txt`.
/// `make_content_conflict` assumes the repo is already initialized (its git.rs
/// callers always run `init_test_repo` first), so we do too.
fn merge_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    gh::make_content_conflict(dir);
}

/// Two content conflicts (`tracked.txt` modify/modify, `second.txt` add/add)
/// in one merge. Repo ends in the Merge state.
fn merge_two_conflicts(dir: &Path) {
    gh::init_test_repo(dir);
    git_in(dir, &["branch", "other"]);
    // master side
    std::fs::write(dir.join("tracked.txt"), "master\n").unwrap();
    std::fs::write(dir.join("second.txt"), "from master\n").unwrap();
    git_in(dir, &["add", "tracked.txt", "second.txt"]);
    git_in(dir, &["commit", "-m", "master side"]);
    // other side
    git_in(dir, &["checkout", "other"]);
    std::fs::write(dir.join("tracked.txt"), "other\n").unwrap();
    std::fs::write(dir.join("second.txt"), "from other\n").unwrap();
    git_in(dir, &["add", "tracked.txt", "second.txt"]);
    git_in(dir, &["commit", "-m", "other side"]);
    git_in(dir, &["checkout", "master"]);
    git_in(dir, &["merge", "other"]);
}

/// One text conflict (`tracked.txt`) and one binary conflict (`binary.bin`,
/// NUL bytes diverged on both sides). Repo ends in the Merge state.
fn merge_text_and_binary(dir: &Path) {
    gh::init_test_repo(dir);
    git_in(dir, &["branch", "other"]);
    std::fs::write(dir.join("tracked.txt"), "master\n").unwrap();
    std::fs::write(dir.join("binary.bin"), [0u8, 1, 2]).unwrap();
    git_in(dir, &["add", "tracked.txt", "binary.bin"]);
    git_in(dir, &["commit", "-m", "master side"]);
    git_in(dir, &["checkout", "other"]);
    std::fs::write(dir.join("tracked.txt"), "other\n").unwrap();
    std::fs::write(dir.join("binary.bin"), [0u8, 3, 4]).unwrap();
    git_in(dir, &["add", "tracked.txt", "binary.bin"]);
    git_in(dir, &["commit", "-m", "other side"]);
    git_in(dir, &["checkout", "master"]);
    git_in(dir, &["merge", "other"]);
}

/// Rebase that conflicts: both branches change `tracked.txt` differently,
/// then `git rebase master` on the topic branch hits a conflict. Repo ends in
/// a Rebase state.
fn rebase_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    git_in(dir, &["branch", "topic"]);
    std::fs::write(dir.join("tracked.txt"), "on master\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "master side"]);
    git_in(dir, &["checkout", "topic"]);
    std::fs::write(dir.join("tracked.txt"), "on topic\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "topic side"]);
    git_in(dir, &["rebase", "master"]);
}

// ---------------------------------------------------------------------
// Assertion helpers.
// ---------------------------------------------------------------------

fn read_file(dir: &Path, rel: &str) -> String {
    String::from_utf8_lossy(&std::fs::read(dir.join(rel)).unwrap()).into_owned()
}

fn file_has_markers(dir: &Path, rel: &str) -> bool {
    git::has_conflict_markers(&read_file(dir, rel))
}

/// Does the index still hold an unmerged entry for `rel`?
fn is_unmerged(dir: &Path, rel: &str) -> bool {
    let repo = Repository::open(dir).unwrap();
    let index = repo.index().unwrap();
    index.conflicts().unwrap().flatten().any(|c| {
        c.our
            .as_ref()
            .or(c.their.as_ref())
            .or(c.ancestor.as_ref())
            .is_some_and(|e| String::from_utf8_lossy(&e.path) == rel)
    })
}

/// Does the staged (stage-0) blob for `rel` contain conflict markers?
fn staged_blob_has_markers(dir: &Path, rel: &str) -> bool {
    let repo = Repository::open(dir).unwrap();
    let index = repo.index().unwrap();
    let Some(e) = index.get_path(Path::new(rel), 0) else {
        return false;
    };
    let blob = repo.find_blob(e.id).unwrap();
    git::has_conflict_markers(&String::from_utf8_lossy(blob.content()))
}

/// `true` if the repo is clean (no merge/rebase/… in progress) — i.e. finalize
/// actually landed a commit.
fn is_clean(dir: &Path) -> bool {
    Repository::open(dir).unwrap().state() == git2::RepositoryState::Clean
}

// =====================================================================
// Tests
// =====================================================================

/// `aic resolve` on a clean repo short-circuits and never calls the resolver
/// or the prompt.
#[tokio::test]
async fn resolve_clean_repo_is_a_noop() {
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let (resolver, seen) = resolver_recording();
    let prompt = prompt_queue(vec![]); // empty — must not be asked
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt).await;
    assert!(result.is_ok(), "clean repo should not error: {:?}", result);
    assert!(
        seen.lock().unwrap().is_empty(),
        "resolver must not run on clean repo"
    );
    assert!(is_clean(dir.path()));
}

/// `aic resolve` on a rebase state is detected but refused in v1 (ADR 0005).
#[tokio::test]
async fn resolve_refuses_rebase_state() {
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    rebase_conflict(dir.path());

    let (resolver, seen) = resolver_recording();
    let _guard = gh::CwdGuard::new(dir.path());

    let err = run_resolve_workflow_impl(resolver, prompt_queue(vec![]))
        .await
        .expect_err("rebase must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("rebase") && msg.contains("v1"),
        "expected rebase refusal, got: {msg}"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "resolver must not run on refused state"
    );
    assert!(!is_clean(dir.path()), "rebase must not be finalized");
}

/// Default `aic` run auto-detects a conflicted repo and, when the user declines
/// `resolve now?`, aborts with a clear redirect — never reaching the resolver
/// or the normal commit flow.
#[tokio::test]
async fn commit_run_auto_detect_aborts_when_user_declines() {
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, seen) = resolver_recording();
    let _guard = gh::CwdGuard::new(dir.path());

    let err = run_commit_workflow_impl(resolver, prompt_queue(vec![false]))
        .await
        .expect_err("must abort when user declines resolve");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("aborted") && msg.contains("mid-merge"),
        "expected abort message, got: {msg}"
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "resolver must not run on decline"
    );
    assert!(!is_clean(dir.path()));
}

/// Default `aic` run: user accepts `resolve now?`, resolver returns clean
/// content, user approves — the conflicted repo is resolved and finalized
/// through the commit-workflow entry point.
#[tokio::test]
async fn commit_run_auto_detect_yes_routes_to_full_resolve() {
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let resolver = resolver_returning("merged\n");
    // [0] = "resolve now?" yes, [1] = "apply tracked.txt?" yes
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_commit_workflow_impl(resolver, prompt_queue(vec![true, true])).await;
    assert!(
        result.is_ok(),
        "full resolve via commit run should succeed: {:?}",
        result
    );

    assert!(is_clean(dir.path()), "merge must be finalized");
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
    assert!(!file_has_markers(dir.path(), "tracked.txt"));
}

/// Marquee happy path: one content conflict → resolver → review → approve →
/// finalize. Repo ends clean, file holds the resolution, no markers remain.
#[tokio::test]
async fn resolve_full_flow_finalizes_merge() {
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let resolver = resolver_returning("merged\n");
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true])).await;
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
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    merge_two_conflicts(dir.path());

    let resolver = resolver_returning("merged\n");
    // Path-based approval so the verdict follows the file regardless of the
    // order `conflicted_files()` returns them in: approve tracked.txt, reject
    // second.txt. (A position-based queue would be order-dependent.)
    let prompt: Prompt = Box::new(|label: &str| Ok(label.contains("tracked.txt")));
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt).await;
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
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    merge_text_and_binary(dir.path());

    let resolver = resolver_returning("merged\n");
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true])).await;
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
    let files = git::Git::conflicted_files().unwrap();
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
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, calls) =
        resolver_then("<<<<<<< HEAD\nbad\n=======\nworse\n>>>>>>> x\n", "merged\n");
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true])).await;
    assert!(
        result.is_ok(),
        "retry-then-clean should succeed: {:?}",
        result
    );

    assert_eq!(*calls.lock().unwrap(), 2, "exactly one retry (2 calls)");
    assert!(is_clean(dir.path()), "merge must be finalized after retry");
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
}

/// If the LLM keeps returning markers after the retry, the file is skipped as
/// failed, `plans` is empty, and the workflow bails with the explicit
/// "no files could be resolved" message.
#[tokio::test]
async fn resolve_gives_up_when_markers_persist() {
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, calls) = resolver_always_markers();
    let _guard = gh::CwdGuard::new(dir.path());

    let err = run_resolve_workflow_impl(resolver, prompt_queue(vec![]))
        .await
        .expect_err("must bail when no file could be resolved");
    assert!(
        format!("{err:#}").contains("no files could be resolved"),
        "expected give-up message, got: {err:#}"
    );
    assert_eq!(*calls.lock().unwrap(), 2, "one attempt + one retry");
    assert!(!is_clean(dir.path()), "merge must not be finalized");
    assert!(
        file_has_markers(dir.path(), "tracked.txt"),
        "file left untouched"
    );
}

/// Conflicted state but the index has no unmerged entries (user resolved every
/// file by hand): the workflow offers finalize, and on `yes` runs git's
/// finalize. The resolver must not be invoked.
#[tokio::test]
async fn resolve_offers_finalize_when_all_manual() {
    let _lock = gh::GIT_CWD_MUTEX.lock().unwrap();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    // Resolve by hand + stage, leaving the repo in the Merge state with no
    // unmerged entries.
    std::fs::write(dir.path().join("tracked.txt"), "hand-merged\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let (resolver, seen) = resolver_recording();
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true])).await;
    assert!(
        result.is_ok(),
        "manual-finalize should succeed: {:?}",
        result
    );
    assert!(
        seen.lock().unwrap().is_empty(),
        "resolver must not run when nothing's unmerged"
    );
    assert!(is_clean(dir.path()), "merge must be finalized");
}
