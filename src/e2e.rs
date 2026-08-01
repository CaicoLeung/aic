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

use crate::display::{Display, DisplayWrite};
use crate::git;
use crate::git::tests as gh;
use crate::{
    BatchPlanner, BoxFuture, CommitMessenger, Prompt, Resolver, generator,
    run_commit_workflow_impl, run_resolve_workflow_impl,
};
use git2::Repository;
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::path::Path;
use std::process::Command;
use std::sync::Arc;

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
            *calls2.lock() += 1;
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
                let mut g = calls2.lock();
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
            seen2.lock().push(content);
            Box::pin(async move { Ok("resolved\n".to_string()) })
        },
    );
    (r, seen)
}

/// Resolver that always fails, with a call counter so a test can assert the
/// error branch was actually reached (not a different skip path). Exercises
/// the `skipped_failed` arm of the resolve loop.
fn resolver_error() -> (Resolver, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0u32));
    let calls2 = calls.clone();
    let r: Resolver = Box::new(
        move |_content: String| -> BoxFuture<anyhow::Result<String>> {
            *calls2.lock() += 1;
            Box::pin(async { Err(anyhow::anyhow!("LLM unreachable (stub)")) })
        },
    );
    (r, calls)
}

/// Resolver that returns `"merged\n"` for every file *except* ones whose
/// conflicted content contains `marker`, which it fails. Used by the
/// mixed-blocker test to drive exactly one file down the `skipped_failed` arm
/// while the other text files resolve cleanly. Includes a call counter.
fn resolver_error_on(marker: &str) -> (Resolver, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0u32));
    let calls2 = calls.clone();
    let marker = marker.to_string();
    let r: Resolver = Box::new(
        move |content: String| -> BoxFuture<anyhow::Result<String>> {
            *calls2.lock() += 1;
            let fail = content.contains(&marker);
            Box::pin(async move {
                if fail {
                    Err(anyhow::anyhow!("LLM unreachable (stub)"))
                } else {
                    Ok("merged\n".to_string())
                }
            })
        },
    );
    (r, calls)
}

/// Prompt that pops answers from a queue; panics if exhausted so an
/// under-specified test fails loudly instead of silently defaulting.
fn prompt_queue(answers: Vec<bool>) -> Prompt {
    let q = Arc::new(Mutex::new(VecDeque::from(answers)));
    Box::new(move |_label: &str| match q.lock().pop_front() {
        Some(b) => Ok(b),
        None => panic!("prompt_queue exhausted — test did not provide enough answers"),
    })
}

/// Planner that returns the same plan regardless of input. Drives the batch
/// loop with a fixed hunk partition so the orchestration — not the LLM — is
/// what's under test.
fn planner_fixed(plan: generator::BatchPlanOutput) -> BatchPlanner {
    Box::new(
        move |_diff: String| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
            let plan = plan.clone();
            Box::pin(async move { Ok(plan) })
        },
    )
}

/// Planner that records every diff it was called with, then returns a fixed
/// plan. The fmt-before-diff e2e test (issue #27) needs to inspect the exact
/// diff string the workflow handed the planner — that diff must reflect the
/// *formatted* source, proving `format_rust_files` ran before capture.
fn planner_recording(plan: generator::BatchPlanOutput) -> (BatchPlanner, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let seen2 = seen.clone();
    let p: BatchPlanner = Box::new(
        move |diff: String| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
            seen2.lock().push(diff);
            let plan = plan.clone();
            Box::pin(async move { Ok(plan) })
        },
    );
    (p, seen)
}

/// A one-batch plan carrying hunk 1 of a single file — the whole change, in
/// one commit. The hook e2e tests (issue #20) only need one commit, so the
/// plan is trivially small; the split tests build their multi-batch plans
/// inline instead.
fn plan_single_batch(file: &str, reason: &str) -> generator::BatchPlanOutput {
    generator::BatchPlanOutput {
        batches: vec![generator::BatchPlanBatch {
            changes: vec![generator::BatchChange {
                file: file.to_string(),
                hunks: vec![1],
            }],
            reason: Some(reason.into()),
        }],
    }
}

/// Messenger that returns the same commit message regardless of input, so the
/// batch loop's stage + commit mechanics are exercised without an LLM.
fn messenger_fixed(msg: &str) -> CommitMessenger {
    let msg = msg.to_string();
    Box::new(
        move |_diff: String| -> BoxFuture<anyhow::Result<generator::CommitOutput>> {
            let m = msg.clone();
            Box::pin(async move {
                Ok(generator::CommitOutput {
                    message: m,
                    body: None,
                })
            })
        },
    )
}

/// Messenger that succeeds for the first `ok_for` calls then errors, with a
/// call counter. Drives the partial-failure path: an early batch commits, then
/// a later batch's message step fails mid-loop.
fn messenger_then_error(ok_for: usize) -> (CommitMessenger, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0u32));
    let calls2 = calls.clone();
    let m: CommitMessenger = Box::new(
        move |_diff: String| -> BoxFuture<anyhow::Result<generator::CommitOutput>> {
            let n = {
                let mut g = calls2.lock();
                *g += 1;
                *g
            };
            Box::pin(async move {
                if n <= ok_for as u32 {
                    Ok(generator::CommitOutput {
                        message: format!("ok {n}"),
                        body: None,
                    })
                } else {
                    Err(anyhow::anyhow!("messenger failure (stub)"))
                }
            })
        },
    );
    (m, calls)
}

/// Planner stub that panics if called — for tests whose path exits before the
/// batch-plan step, so a regression that reaches the LLM fails loudly instead
/// of silently hitting the network.
fn unreachable_planner() -> BatchPlanner {
    Box::new(
        |_diff: String| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
            Box::pin(async { panic!("BatchPlanner reached on a path that must skip the LLM") })
        },
    )
}

/// Messenger stub that panics if called — same purpose as
/// [`unreachable_planner`] for the commit-message step.
fn unreachable_messenger() -> CommitMessenger {
    Box::new(
        |_diff: String| -> BoxFuture<anyhow::Result<generator::CommitOutput>> {
            Box::pin(async { panic!("CommitMessenger reached on a path that must skip the LLM") })
        },
    )
}

/// Resolver stub that panics if called — same purpose as
/// [`unreachable_planner`] for the conflict-resolution step.
fn unreachable_resolver() -> Resolver {
    Box::new(|_content: String| -> BoxFuture<anyhow::Result<String>> {
        Box::pin(async { panic!("Resolver reached on a path that must skip the LLM") })
    })
}

// ---------------------------------------------------------------------
// Display seam: an in-memory sink so emitted wording can be asserted.
// ---------------------------------------------------------------------

/// In-memory [`DisplayWrite`] capturing every line the workflow emits. Clones
/// share the underlying buffer so a test can hand one clone to `Display::with`
/// and read the lines back from the other. Most tests just want a quiet sink
/// (via [`sink`]); the mixed-blocker test inspects the captured lines.
#[derive(Clone, Default)]
struct BufferWrite(Arc<Mutex<Vec<String>>>);

impl BufferWrite {
    /// Snapshot of every line written so far, in order.
    fn lines(&self) -> Vec<String> {
        self.0.lock().clone()
    }
}

impl DisplayWrite for BufferWrite {
    fn write_line(&self, line: &str) {
        self.0.lock().push(line.to_string());
    }
}

/// A `Display` backed by a fresh, discarded [`BufferWrite`] — a quiet sink for
/// tests that don't assert on emitted wording. Keeps `cargo test` output free
/// of real merge-conflict noise the workflow would otherwise write to stderr.
fn sink() -> Display {
    Display::with(BufferWrite::default())
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

/// Run a read-only `git` command in `dir` and return its stdout. Used by the
/// state-assertion helpers (`file_at_ref`, `commit_count`) and the hook e2e
/// tests (issue #20), which all shell out to inspect repo state the workflow
/// just produced. Panics on failure — these are test assertions, not logic.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-C"])
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `init_test_repo` + diverge `tracked.txt` on two branches, then merge so the
/// repo ends in the Merge state with a content conflict in `tracked.txt`.
/// `make_content_conflict` assumes the repo is already initialized (its git.rs
/// callers always run `init_test_repo` first), so we do too.
fn merge_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    gh::make_content_conflict(dir);
}

/// Minimal cargo project on top of [`gh::init_test_repo`]: a dependency-free
/// `Cargo.toml`, a formatted `src/main.rs`, and a `/target` `.gitignore`, all
/// committed. `format_rust_files` runs `cargo fmt --all` from the workflow's
/// [`gh::CwdGuard`], which needs a manifest to operate on — impossible in a
/// plain `init_test_repo` git repo. Used by the fmt-before-diff e2e test
/// (issue #27).
///
/// The base `main.rs` has its two edit sites (lines 3 and 12) ≥8 lines apart,
/// so the formatted diff splits into two hunks under git's default three-line
/// context — the geometry the hunk-stability test relies on.
fn init_cargo_repo(dir: &Path) {
    gh::init_test_repo(dir);
    std::fs::write(
        dir.join("Cargo.toml"),
        "[package]\nname = \"fmttest\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\n",
    )
    .unwrap();
    // `cargo fmt` may emit build metadata or a lockfile; keep both out of
    // `Git::status` so only the Rust source under test appears as unstaged.
    std::fs::write(dir.join(".gitignore"), "/target\nCargo.lock\n").unwrap();
    std::fs::create_dir_all(dir.join("src")).unwrap();
    std::fs::write(
        dir.join("src/main.rs"),
        "fn main() {\n    // edit site 1\n    let value = 1;\n    // pad 0\n    // pad 1\n    // pad 2\n    // pad 3\n    // pad 4\n    // pad 5\n    // pad 6\n    // pad 7\n    let other = 2;\n}\n",
    )
    .unwrap();
    git_in(dir, &["add", "Cargo.toml", ".gitignore", "src/main.rs"]);
    git_in(dir, &["commit", "-m", "formatted base"]);
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

/// Join `n` lines `"{prefix} {i}\n"`. Used to build a file over the
/// `MAX_CONFLICT_LINES` cap so `classify_worktree` returns `Oversized`.
fn make_lines(prefix: &str, n: usize) -> String {
    (0..n).map(|i| format!("{prefix} {i}\n")).collect()
}

/// Cherry-pick conflict: master and topic both change `tracked.txt` from the
/// initial commit differently; `git cherry-pick topic` on master hits a content
/// conflict. Repo ends in the CherryPick state.
fn cherry_pick_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    git_in(dir, &["branch", "topic"]);
    std::fs::write(dir.join("tracked.txt"), "on master\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "master side"]);
    git_in(dir, &["checkout", "topic"]);
    std::fs::write(dir.join("tracked.txt"), "on topic\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "topic side"]);
    git_in(dir, &["checkout", "master"]);
    git_in(dir, &["cherry-pick", "topic"]);
}

/// Revert conflict: commit A changes `tracked.txt`, commit B changes it again,
/// then reverting A conflicts with B. Repo ends in the Revert state.
fn revert_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "change tracked"]);
    std::fs::write(dir.join("tracked.txt"), "master override\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "master override"]);
    git_in(dir, &["revert", "HEAD~1"]);
}

/// Delete/modify conflict: master deletes `tracked.txt`, other modifies it.
/// The index carries no `our` stage for the path, so `conflicted_files()`
/// classifies it `DeleteModify`. Repo ends in the Merge state.
fn merge_delete_modify_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    git_in(dir, &["branch", "other"]);
    std::fs::remove_file(dir.join("tracked.txt")).unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "master deletes"]);
    git_in(dir, &["checkout", "other"]);
    std::fs::write(dir.join("tracked.txt"), "modified on other\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "other modifies"]);
    git_in(dir, &["checkout", "master"]);
    git_in(dir, &["merge", "other"]);
}

/// One small text conflict (`tracked.txt`) and one oversized text conflict
/// (`big.txt`, > `MAX_CONFLICT_LINES` on both sides) in one merge. Repo ends in
/// the Merge state.
fn merge_oversized_and_text(dir: &Path) {
    gh::init_test_repo(dir);
    let big_base = make_lines("base", 2500);
    std::fs::write(dir.join("big.txt"), &big_base).unwrap();
    git_in(dir, &["add", "big.txt"]);
    git_in(dir, &["commit", "-m", "add big base"]);
    git_in(dir, &["branch", "other"]);
    std::fs::write(dir.join("tracked.txt"), "master\n").unwrap();
    std::fs::write(dir.join("big.txt"), make_lines("master", 2500)).unwrap();
    git_in(dir, &["add", "tracked.txt", "big.txt"]);
    git_in(dir, &["commit", "-m", "master side"]);
    git_in(dir, &["checkout", "other"]);
    std::fs::write(dir.join("tracked.txt"), "other\n").unwrap();
    std::fs::write(dir.join("big.txt"), make_lines("other", 2500)).unwrap();
    git_in(dir, &["add", "tracked.txt", "big.txt"]);
    git_in(dir, &["commit", "-m", "other side"]);
    git_in(dir, &["checkout", "master"]);
    git_in(dir, &["merge", "other"]);
}

/// Four conflicts in one merge, one of each disposition the resolve workflow
/// can land on, so the hand-off message must carry every blocker category at
/// once:
///   - `tracked.txt`  resolvable text, approved   → staged
///   - `second.txt`   resolvable text, rejected    → rejected
///   - `third.txt`    resolvable text, LLM errors   → failed to resolve
///   - `binary.bin`   binary conflict              → need manual resolution
///
/// `third.txt` carries the `LLM_FAIL` sentinel so [`resolver_error_on`] can
/// fail exactly that file while leaving the other text files clean. Repo ends
/// in the Merge state.
fn merge_mixed_blockers(dir: &Path) {
    gh::init_test_repo(dir);
    git_in(dir, &["branch", "other"]);
    // master side
    std::fs::write(dir.join("tracked.txt"), "master\n").unwrap();
    std::fs::write(dir.join("second.txt"), "from master\n").unwrap();
    std::fs::write(dir.join("third.txt"), "LLM_FAIL master\n").unwrap();
    std::fs::write(dir.join("binary.bin"), [0u8, 1, 2]).unwrap();
    git_in(
        dir,
        &[
            "add",
            "tracked.txt",
            "second.txt",
            "third.txt",
            "binary.bin",
        ],
    );
    git_in(dir, &["commit", "-m", "master side"]);
    // other side
    git_in(dir, &["checkout", "other"]);
    std::fs::write(dir.join("tracked.txt"), "other\n").unwrap();
    std::fs::write(dir.join("second.txt"), "from other\n").unwrap();
    std::fs::write(dir.join("third.txt"), "LLM_FAIL other\n").unwrap();
    std::fs::write(dir.join("binary.bin"), [0u8, 3, 4]).unwrap();
    git_in(
        dir,
        &[
            "add",
            "tracked.txt",
            "second.txt",
            "third.txt",
            "binary.bin",
        ],
    );
    git_in(dir, &["commit", "-m", "other side"]);
    git_in(dir, &["checkout", "master"]);
    git_in(dir, &["merge", "other"]);
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
/// actually landed a commit. NOTE: this is *operational* cleanliness only;
/// a Clean state can still carry staged, unstaged, or untracked entries. Use
/// [`worktree_is_empty`] when a test must also assert nothing is left
/// uncommitted.
fn is_clean(dir: &Path) -> bool {
    Repository::open(dir).unwrap().state() == git2::RepositoryState::Clean
}

/// `true` if `git status --porcelain` is empty — no staged, unstaged, or
/// untracked entries. The strict "working tree is clean" guarantee the
/// commit-workflow tests pin: a regression that committed *and* re-left the
/// change staged (or dropped it entirely) would keep `is_clean` green but
/// trip this.
fn worktree_is_empty(dir: &Path) -> bool {
    git_out(dir, &["status", "--porcelain"]).trim().is_empty()
}

/// File content as recorded at a git revision (e.g. "HEAD", "HEAD~1"), via
/// `git show`. Asserts what each batch commit actually contains.
fn file_at_ref(dir: &Path, rev: &str, rel: &str) -> String {
    git_out(dir, &["show", &format!("{rev}:{rel}")])
}

/// Total commits reachable from HEAD.
fn commit_count(dir: &Path) -> usize {
    git_out(dir, &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .unwrap()
}

// =====================================================================
// Tests
// =====================================================================

/// `aic resolve` on a clean repo short-circuits and never calls the resolver
/// or the prompt.
#[tokio::test]
async fn resolve_clean_repo_is_a_noop() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let (resolver, seen) = resolver_recording();
    let prompt = prompt_queue(vec![]); // empty — must not be asked
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt, sink()).await;
    assert!(result.is_ok(), "clean repo should not error: {:?}", result);
    assert!(
        seen.lock().is_empty(),
        "resolver must not run on clean repo"
    );
    assert!(is_clean(dir.path()));
}

/// `aic` on a clean repo (nothing staged, nothing unstaged) prints the
/// nothing-to-commit notice and returns without calling the LLM or prompting.
#[tokio::test]
async fn commit_clean_repo_is_a_noop() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let (resolver, seen) = resolver_recording();
    let prompt = prompt_queue(vec![]); // empty — must not be asked
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_commit_workflow_impl(
        resolver,
        prompt,
        sink(),
        unreachable_planner(),
        unreachable_messenger(),
    )
    .await;
    assert!(result.is_ok(), "clean repo should not error: {:?}", result);
    assert!(
        seen.lock().is_empty(),
        "LLM resolver must not run when there are no changes"
    );
    assert!(is_clean(dir.path()));
}

/// `aic resolve` on a rebase state is detected but refused in v1 (ADR 0005).
#[tokio::test]
async fn resolve_refuses_rebase_state() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    rebase_conflict(dir.path());

    let (resolver, seen) = resolver_recording();
    let _guard = gh::CwdGuard::new(dir.path());

    let err = run_resolve_workflow_impl(resolver, prompt_queue(vec![]), sink())
        .await
        .expect_err("rebase must be refused");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("rebase") && msg.contains("v1"),
        "expected rebase refusal, got: {msg}"
    );
    assert!(
        seen.lock().is_empty(),
        "resolver must not run on refused state"
    );
    assert!(!is_clean(dir.path()), "rebase must not be finalized");
}

/// Default `aic` run auto-detects a conflicted repo and, when the user declines
/// `resolve now?`, aborts with a clear redirect — never reaching the resolver
/// or the normal commit flow.
#[tokio::test]
async fn commit_run_auto_detect_aborts_when_user_declines() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, seen) = resolver_recording();
    let _guard = gh::CwdGuard::new(dir.path());

    let err = run_commit_workflow_impl(
        resolver,
        prompt_queue(vec![false]),
        sink(),
        unreachable_planner(),
        unreachable_messenger(),
    )
    .await
    .expect_err("must abort when user declines resolve");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("aborted") && msg.contains("mid-merge"),
        "expected abort message, got: {msg}"
    );
    assert!(seen.lock().is_empty(), "resolver must not run on decline");
    assert!(!is_clean(dir.path()));
}

/// Default `aic` run: user accepts `resolve now?`, resolver returns clean
/// content, user approves — the conflicted repo is resolved and finalized
/// through the commit-workflow entry point.
#[tokio::test]
async fn commit_run_auto_detect_yes_routes_to_full_resolve() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let resolver = resolver_returning("merged\n");
    // [0] = "resolve now?" yes, [1] = "apply tracked.txt?" yes
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_commit_workflow_impl(
        resolver,
        prompt_queue(vec![true, true]),
        sink(),
        unreachable_planner(),
        unreachable_messenger(),
    )
    .await;
    assert!(
        result.is_ok(),
        "full resolve via commit run should succeed: {:?}",
        result
    );

    assert!(is_clean(dir.path()), "merge must be finalized");
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
    assert!(!file_has_markers(dir.path(), "tracked.txt"));
}

/// Headline hunk-split behavior: one file with two unrelated changes lands as
/// TWO atomic commits, each carrying only its assigned hunk. Drives the full
/// batch loop (capture diff → validate → per-batch stage + commit) against a
/// real repo with a stub plan and stub commit messages, so no LLM is contacted.
#[tokio::test]
async fn commit_splits_one_file_across_two_batches() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // Two change sites far enough apart that git emits two separate hunks.
    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}c0\n");
    let changed = format!("a1\n{pad}c1\n");
    std::fs::write(dir.path().join("tracked.txt"), &base).unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);
    git_in(dir.path(), &["commit", "-m", "base"]);
    std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

    // Stub plan: split the single file's two hunks across two batches.
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
    let _g = gh::CwdGuard::new(dir.path());
    let result = run_commit_workflow_impl(
        resolver_returning(""),
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: stub"),
    )
    .await;
    assert!(
        result.is_ok(),
        "two-batch split should succeed: {:?}",
        result
    );

    // initial + base + 2 batch commits.
    assert_eq!(commit_count(dir.path()), 4);
    // Batch 1 (HEAD~1): only hunk 1 (a1) applied; hunk 2 (c1) still absent.
    let after_batch1 = file_at_ref(dir.path(), "HEAD~1", "tracked.txt");
    assert!(after_batch1.contains("a1"), "batch 1 must include hunk 1");
    assert!(
        !after_batch1.contains("c1"),
        "batch 1 must NOT include hunk 2"
    );
    // Batch 2 (HEAD): both hunks present, working tree clean.
    let after_batch2 = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(after_batch2.contains("a1") && after_batch2.contains("c1"));
    assert!(
        is_clean(dir.path()),
        "working tree must be clean at the end"
    );
}

/// The other Run commit shape (issue #26): when files are already staged, the
/// default Run re-stages them, drafts one message via the `CommitMessenger`,
/// and commits — never reaching the `BatchPlanner`. This is the simpler of the
/// two Run shapes and the one the README leads with ("stage a diff, get one
/// commit"). The unstaged multi-Batch path is pinned by
/// [`commit_splits_one_file_across_two_batches`]; this pins its staged
/// counterpart so a regression that drops the staged file or routes staged
/// work into the planner would fail loudly instead of shipping green.
#[tokio::test]
async fn commit_staged_files_in_one_commit() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // Modify a tracked file and stage it — the entry condition for the staged
    // single-commit path. A non-Rust file keeps this test focused on the
    // commit shape; staged-Rust formatting is a separate coverage gap.
    std::fs::write(dir.path().join("tracked.txt"), "staged change\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let before = commit_count(dir.path());
    let _g = gh::CwdGuard::new(dir.path());

    let result = run_commit_workflow_impl(
        unreachable_resolver(), // non-conflicted path must NOT resolve
        prompt_queue(vec![]),
        sink(),
        unreachable_planner(), // staged path must NOT plan
        messenger_fixed("feat: staged change"),
    )
    .await;
    assert!(
        result.is_ok(),
        "staged single-commit Run should succeed: {:?}",
        result
    );

    // Exactly one new commit landed.
    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "the staged path must land exactly one commit"
    );
    // Its tree contains the staged change.
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "tracked.txt"),
        "staged change\n",
        "the commit's tree must contain the staged change"
    );
    // The drafted message survived into the commit verbatim — pins that the
    // messenger (not some skip path) produced it. Exact because `Git::commit`
    // writes the message with `--cleanup=verbatim`.
    assert_eq!(
        git_out(dir.path(), &["log", "-1", "--pretty=%B"]).trim(),
        "feat: staged change",
        "the messenger's drafted message must land in the commit"
    );
    // The working tree is clean afterward: no merge/rebase state, and nothing
    // left staged or unstaged.
    assert!(is_clean(dir.path()), "no merge/rebase state must remain");
    assert_eq!(
        git_out(dir.path(), &["status", "--porcelain"]).trim(),
        "",
        "working tree must be clean after the staged commit"
    );
}

/// A mid-loop failure (here: the 2nd batch's message step errors after batch 1
/// already committed) must abort with the unified message naming how many
/// batches committed — and those earlier commits must persist in the repo.
/// Guards the [important] partial-failure UX contract.
#[tokio::test]
async fn commit_batch_loop_aborts_after_partial_commit() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
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
    let (messenger, calls) = messenger_then_error(1); // batch 1 ok, batch 2 fails
    let _g = gh::CwdGuard::new(dir.path());
    let err = run_commit_workflow_impl(
        resolver_returning(""),
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger,
    )
    .await
    .expect_err("must abort when a later batch fails");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("aborted on batch 2"),
        "expected batch-2 abort, got: {msg}"
    );
    assert!(
        msg.contains("1 batch(es) committed"),
        "expected 1 committed, got: {msg}"
    );
    assert_eq!(*calls.lock(), 2, "messenger called once per batch");
    // Batch 1 DID commit — its hunk is in HEAD despite the later failure; the
    // failed batch 2's hunk is staged but NOT committed.
    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(head.contains("a1"), "batch 1 must be committed");
    assert!(!head.contains("c1"), "batch 2 must NOT be committed");
}

// =====================================================================
// cargo fmt runs before the unstaged diff is captured (issue #27)
// =====================================================================
//
// Before an unstaged Run captures per-file diffs and asks the model for a
// Batch plan, aic formats Rust files so the hunk numbers the model returns
// still line up with what gets staged — formatting *after* capture would
// shift hunks out from under their indices and stage the wrong lines. The
// staged-files shape of the Run is pinned separately by
// [`commit_staged_files_in_one_commit`]; this section pins the fmt ordering
// on the unstaged/Batch shape.

/// Issue #27: prove `cargo fmt` runs *before* the unstaged diff is captured
/// (hunk-stability guard). The workdir edit is a genuine change (1 → 2 and
/// 2 → 3) wrapped in formatting violations; `cargo fmt --all` rewrites it to
/// formatted Rust before the workflow captures the diff the planner sees and
/// stages from.
///
/// The base file's two edit sites are ≥8 lines apart, so the diff has two
/// hunks. The stub plan splits the file across two batches by hunk index
/// (batch 1: hunk 2, batch 2: hunk 1). The fingerprints of the unformatted
/// edit (`value=2`, `other=3`) are text rustfmt never leaves, so a passing
/// run proves the planner saw the post-format diff — and that hunk 2 staged
/// exactly the formatted `other` change (hunk 1 untouched until batch 2)
/// proves the staged hunk indices matched that post-format diff, not a
/// shifted one.
#[tokio::test]
async fn commit_run_formats_rust_before_capturing_diff() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    init_cargo_repo(dir.path());

    // Unstaged edit: both edit sites get a real change wrapped in a
    // formatting violation, with the separating pad lines untouched so the
    // diff keeps its two-hunk structure.
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {\n    // edit site 1\n    let value=2;\n    // pad 0\n    // pad 1\n    // pad 2\n    // pad 3\n    // pad 4\n    // pad 5\n    // pad 6\n    // pad 7\n    let other=3;\n}\n",
    )
    .unwrap();

    // Stub plan: hunk 2 first, hunk 1 second — indices that only exist
    // against the formatted two-hunk diff.
    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "src/main.rs".to_string(),
                    hunks: vec![2],
                }],
                reason: Some("value bump".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "src/main.rs".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("other bump".into()),
            },
        ],
    };
    let (planner, seen) = planner_recording(plan);
    let _g = gh::CwdGuard::new(dir.path());
    let before = commit_count(dir.path());

    let result = run_commit_workflow_impl(
        resolver_returning(""),
        prompt_queue(vec![]),
        sink(),
        planner,
        messenger_fixed("chore: value bump"),
    )
    .await;
    assert!(
        result.is_ok(),
        "fmt-before-diff Run should succeed: {:?}",
        result
    );

    // The planner WAS reached — the unstaged/Batch path ran, not a skip.
    let captured = seen.lock().clone();
    assert_eq!(captured.len(), 1, "planner must be called exactly once");

    // Headline assertion: the diff the planner received reflects the
    // FORMATTED source with both hunks present, not the crammed single-hunk
    // edit. `value=2`/`other=3` are fingerprints rustfmt never leaves; the
    // formatted forms are always `value = 2` / `other = 3`.
    let diff = &captured[0];
    assert!(
        diff.contains("let value = 2"),
        "planner must see the formatted diff; got:\n{diff}"
    );
    assert!(
        diff.contains("let other = 3"),
        "planner must see the formatted diff; got:\n{diff}"
    );
    assert!(
        !diff.contains("value=2") && !diff.contains("other=3"),
        "planner must NOT see the unformatted workdir text — cargo fmt must \
         run before diff capture; got:\n{diff}"
    );

    // Exactly two batch commits land.
    assert_eq!(
        commit_count(dir.path()),
        before + 2,
        "two batch commits must land"
    );

    // Batch 1 staged hunk 2 of the post-format diff: the `other` change
    // only. The `value` change (hunk 1) must be absent — a fmt-after-diff
    // regression hands the model a one-hunk diff where hunk 2 does not
    // exist, so this landing proves the staged hunk index matched the
    // post-format diff.
    let after_batch1 = file_at_ref(dir.path(), "HEAD~1", "src/main.rs");
    assert!(
        after_batch1.contains("let other = 3;"),
        "batch 1 must carry hunk 2 (the `other` change); got:\n{after_batch1}"
    );
    assert!(
        !after_batch1.contains("let value = 2"),
        "batch 1 must NOT carry hunk 1 (the `value` change); got:\n{after_batch1}"
    );
    assert!(
        after_batch1.contains("let value = 1;"),
        "batch 1 must leave hunk 1 unstaged; got:\n{after_batch1}"
    );

    // Batch 2 staged hunk 1; the final commit holds the fully formatted
    // file, and the working tree is empty — every hunk landed exactly once.
    let head = file_at_ref(dir.path(), "HEAD", "src/main.rs");
    assert!(
        head.contains("let value = 2;") && head.contains("let other = 3;"),
        "final commit must hold both formatted changes; got:\n{head}"
    );
    assert!(
        !head.contains("value=2"),
        "final commit must not carry the unformatted edit; got:\n{head}"
    );
    assert!(
        is_clean(dir.path()),
        "repo must be in a clean state at the end"
    );
    assert!(
        worktree_is_empty(dir.path()),
        "working tree must be empty at the end — both hunks staged and committed"
    );
}

// =====================================================================
// Git hooks run during a Run (issue #20)
// =====================================================================

/// Issue #20 AC1: git hooks fire during a full Run. The Run commits through
/// the real `git commit` CLI (`Git::commit`), so `pre-commit` and `commit-msg`
/// execute mid-Run. Under the pre-#19 libgit2 commit path neither ever ran
/// (libgit2 has no hook machinery) — this e2e test pins the shell-out behavior
/// from the orchestration layer down: LLM stubbed, repo real, hooks installed
/// in the repo's `.git/hooks`.
#[tokio::test]
async fn commit_run_runs_pre_commit_and_commit_msg_hooks() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
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

    let _g = gh::CwdGuard::new(dir.path());
    let result = run_commit_workflow_impl(
        resolver_returning(""),
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: hook run"),
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
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    gh::install_hook(
        dir.path(),
        "pre-commit",
        "echo 'blocked by policy' >&2; exit 1",
    );

    std::fs::write(dir.path().join("tracked.txt"), "changed by hook test\n").unwrap();
    let plan = plan_single_batch("tracked.txt", "hook veto");

    let _g = gh::CwdGuard::new(dir.path());
    let before = commit_count(dir.path());

    let err = run_commit_workflow_impl(
        resolver_returning(""),
        prompt_queue(vec![]),
        sink(),
        planner_fixed(plan),
        messenger_fixed("chore: vetoed"),
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
    let staged = git::Git::diff(Some("tracked.txt")).unwrap();
    assert!(
        staged.contains("changed by hook test"),
        "staged hunks must survive the veto; staged diff:\n{staged}"
    );
}

/// Marquee happy path: one content conflict → resolver → review → approve →
/// finalize. Repo ends clean, file holds the resolution, no markers remain.
#[tokio::test]
async fn resolve_full_flow_finalizes_merge() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let resolver = resolver_returning("merged\n");
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true]), sink()).await;
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
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_two_conflicts(dir.path());

    let resolver = resolver_returning("merged\n");
    // Path-based approval so the verdict follows the file regardless of the
    // order `conflicted_files()` returns them in: approve tracked.txt, reject
    // second.txt. (A position-based queue would be order-dependent.)
    let prompt: Prompt = Box::new(|label: &str| Ok(label.contains("tracked.txt")));
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt, sink()).await;
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
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_text_and_binary(dir.path());

    let resolver = resolver_returning("merged\n");
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true]), sink()).await;
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
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, calls) =
        resolver_then("<<<<<<< HEAD\nbad\n=======\nworse\n>>>>>>> x\n", "merged\n");
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true]), sink()).await;
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
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, calls) = resolver_always_markers();
    let _guard = gh::CwdGuard::new(dir.path());

    let err = run_resolve_workflow_impl(resolver, prompt_queue(vec![]), sink())
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

/// Conflicted state but the index has no unmerged entries (user resolved every
/// file by hand): the workflow offers finalize, and on `yes` runs git's
/// finalize. The resolver must not be invoked.
#[tokio::test]
async fn resolve_offers_finalize_when_all_manual() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    // Resolve by hand + stage, leaving the repo in the Merge state with no
    // unmerged entries.
    std::fs::write(dir.path().join("tracked.txt"), "hand-merged\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let (resolver, seen) = resolver_recording();
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true]), sink()).await;
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
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    // Resolve by hand + stage, leaving the repo in the Merge state with no
    // unmerged entries — identical entry condition to the `yes` test.
    std::fs::write(dir.path().join("tracked.txt"), "hand-merged\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let (resolver, seen) = resolver_recording();
    let _guard = gh::CwdGuard::new(dir.path());

    // Confirm the entry condition holds before the workflow runs: mid-merge,
    // but nothing unmerged in the index. (`Git::state` / `conflicted_files`
    // read the process CWD, so the guard must already be held.)
    assert_eq!(
        git::Git::state().unwrap(),
        git::RepoState::Merge,
        "setup must leave the repo mid-merge"
    );
    assert!(
        git::Git::conflicted_files().unwrap().is_empty(),
        "setup must leave no unmerged entries"
    );

    let before = commit_count(dir.path());

    // Answer "finalize now?" with no — the only prompt on this path.
    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![false]), sink()).await;
    assert!(
        result.is_ok(),
        "declining finalize should not error: {:?}",
        result
    );

    // No finalize ran: the repo is still mid-merge, and no commit landed.
    assert_eq!(
        git::Git::state().unwrap(),
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

// =====================================================================
// Coverage: non-Merge finalize states, conflict-kind classification, LLM
// error path. These close the gaps flagged in the PR #8 e2e review.
// =====================================================================

/// Cherry-pick conflict resolved + finalized end-to-end (ADR 0005). Unlike
/// Merge, finalize shells out to `git cherry-pick --continue` — this verifies
/// that path actually clears the CherryPick state in a real repo, not merely
/// that `finalize_invocation` maps the enum (unit-tested in git.rs).
#[tokio::test]
async fn resolve_finalizes_cherry_pick() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    cherry_pick_conflict(dir.path());

    let _guard = gh::CwdGuard::new(dir.path());

    assert_eq!(
        git::Git::state().unwrap(),
        git::RepoState::CherryPick,
        "setup must leave the repo mid-cherry-pick"
    );

    let resolver = resolver_returning("merged\n");

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true]), sink()).await;
    assert!(
        result.is_ok(),
        "cherry-pick resolve should succeed: {:?}",
        result
    );

    assert!(is_clean(dir.path()), "cherry-pick must be finalized");
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
    assert!(!file_has_markers(dir.path(), "tracked.txt"));
}

/// Revert conflict resolved + finalized end-to-end (ADR 0005). Finalize runs
/// `git revert --continue`; verifies the Revert state is cleared in a real repo.
#[tokio::test]
async fn resolve_finalizes_revert() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    revert_conflict(dir.path());

    let _guard = gh::CwdGuard::new(dir.path());

    assert_eq!(
        git::Git::state().unwrap(),
        git::RepoState::Revert,
        "setup must leave the repo mid-revert"
    );

    let resolver = resolver_returning("merged\n");

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true]), sink()).await;
    assert!(
        result.is_ok(),
        "revert resolve should succeed: {:?}",
        result
    );

    assert!(is_clean(dir.path()), "revert must be finalized");
    assert_eq!(read_file(dir.path(), "tracked.txt"), "merged\n");
    assert!(!file_has_markers(dir.path(), "tracked.txt"));
}

/// Delete/modify conflict (master deleted, other modified) is classified
/// `DeleteModify` and skipped — never reaches the LLM. With no resolvable
/// files, the workflow bails (ADR 0005: structural conflicts need manual
/// resolution).
#[tokio::test]
async fn resolve_skips_delete_modify_conflict() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_delete_modify_conflict(dir.path());

    let (resolver, seen) = resolver_recording();
    let _guard = gh::CwdGuard::new(dir.path());

    let files = git::Git::conflicted_files().unwrap();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].path, "tracked.txt");
    assert_eq!(files[0].kind, git::ConflictKind::DeleteModify);

    let err = run_resolve_workflow_impl(resolver, prompt_queue(vec![]), sink())
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
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_oversized_and_text(dir.path());

    let resolver = resolver_returning("merged\n");
    let _guard = gh::CwdGuard::new(dir.path());

    let result = run_resolve_workflow_impl(resolver, prompt_queue(vec![true]), sink()).await;
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
    let files = git::Git::conflicted_files().unwrap();
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
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, calls) = resolver_error();
    let _guard = gh::CwdGuard::new(dir.path());

    let err = run_resolve_workflow_impl(resolver, prompt_queue(vec![]), sink())
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

// =====================================================================
// Display seam: assert the emitted hand-off wording (issue #9).
// =====================================================================

/// Mixed-blocker scenario (1 approved + 1 rejected + 1 LLM-error + 1 binary)
/// drives every blocker category into the hand-off message at once. Before the
/// `DisplayWrite` seam, this wording was asserted by zero tests: the three
/// existing single-blocker tests only assert repo state, so a regression that
/// swapped labels, dropped a category, or mis-counted would ship green. This
/// test captures the emitted line via [`BufferWrite`] and pins the full
/// three-way breakdown on a single line.
#[tokio::test]
async fn resolve_handoff_lists_all_three_blocker_kinds() {
    let _lock = gh::GIT_CWD_MUTEX.lock();
    let dir = tempfile::tempdir().unwrap();
    merge_mixed_blockers(dir.path());

    // Fail exactly `third.txt` (its content carries the LLM_FAIL sentinel);
    // the other text files resolve to "merged\n".
    let (resolver, calls) = resolver_error_on("LLM_FAIL");
    // Approve tracked.txt, reject second.txt — path-based so it follows the
    // file regardless of the order conflicted_files() returns them in.
    let prompt: Prompt = Box::new(|label: &str| Ok(label.contains("tracked.txt")));
    let _guard = gh::CwdGuard::new(dir.path());

    let buf = BufferWrite::default();
    let display = Display::with(buf.clone());
    let result = run_resolve_workflow_impl(resolver, prompt, display).await;
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

    // --- the whole point: the emitted hand-off line carries all three
    // categories with correct counts, on one line. ---
    let handoff = buf
        .lines()
        .into_iter()
        .find(|l| l.contains("not finalized"))
        .expect("expected a hand-off 'not finalized' line");
    assert!(
        handoff.contains("1 rejected")
            && handoff.contains("1 failed to resolve")
            && handoff.contains("1 need manual resolution"),
        "hand-off line must list all three blocker kinds with counts, got: {handoff:?}"
    );
    // And the approved count is reported separately, not conflated.
    assert!(
        buf.lines()
            .iter()
            .any(|l| l.contains("1 resolved + staged")),
        "expected an 'approved' line separate from the blockers, got: {:?}",
        buf.lines()
    );

    // The buffer sink reports no color capability, so nothing it captures
    // should carry ANSI escapes — guards the sink-derived `colors` fix.
    assert!(
        buf.lines().iter().all(|l| !l.contains('\u{1b}')),
        "buffer sink must emit plain text (no ANSI), got: {:?}",
        buf.lines()
    );
}
