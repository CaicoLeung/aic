// Shared e2e helpers: stub factories, repo setup, assertions, display sink.
// All `pub` so the per-feature test modules can pull them in via
// `use super::common::*;`.

pub(super) use crate::display::{Display, DisplayWrite};
pub(super) use crate::git;
pub(super) use crate::git::Git;
pub(super) use crate::git::tests as gh;
pub(super) use crate::{
    BatchPlanner, BoxFuture, CommitEditor, CommitMessenger, Confirm, ConfirmChoice, ConfirmMenu,
    Prompt, Resolver, generator, run_commit_workflow_impl, run_resolve_workflow_impl,
};
pub(super) use git2::Repository;
pub(super) use parking_lot::Mutex;
pub(super) use std::collections::VecDeque;
pub(super) use std::path::Path;
pub(super) use std::process::Command;
pub(super) use std::sync::Arc;

// Erased resolver / prompt closures. Both are `Box<dyn Fn>` (which itself
// implements `Fn`), so they pass straight into the workflow impls' concrete
// `Resolver` / `Prompt` parameters. `BoxFuture`, `Resolver`, and `Prompt` are
// re-used from the crate root so the stub types stay identical to the seam's.

pub fn resolver_returning(answer: &str) -> Resolver {
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
pub fn resolver_always_markers() -> (Resolver, Arc<Mutex<u32>>) {
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
pub fn resolver_then(first: &str, then: &str) -> (Resolver, Arc<Mutex<u32>>) {
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

/// First call returns marker-laden `first`; every later call errors with
/// `error`. Exercises the markers-then-error refinement (#83): the first
/// attempt is retryable (`Markers`), but the retry-attempt failure must
/// propagate as `Fatal` — the skip message reports the LLM error instead of
/// masking it as "markers remain after retry". Includes a call counter.
pub fn resolver_then_error(first: &str, error: &str) -> (Resolver, Arc<Mutex<u32>>) {
    let calls = Arc::new(Mutex::new(0u32));
    let calls2 = calls.clone();
    let first = first.to_string();
    let error = error.to_string();
    let r: Resolver = Box::new(
        move |_content: String| -> BoxFuture<anyhow::Result<String>> {
            let n = {
                let mut g = calls2.lock();
                *g += 1;
                *g
            };
            let first = first.clone();
            let error = error.clone();
            Box::pin(async move {
                if n == 1 {
                    Ok(first)
                } else {
                    Err(anyhow::anyhow!("{error}"))
                }
            })
        },
    );
    (r, calls)
}

/// Resolver that records every input it was called with (for asserting the
/// resolver was *not* reached on early-exit paths).
pub fn resolver_recording() -> (Resolver, Arc<Mutex<Vec<String>>>) {
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
pub fn resolver_error() -> (Resolver, Arc<Mutex<u32>>) {
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
pub fn resolver_error_on(marker: &str) -> (Resolver, Arc<Mutex<u32>>) {
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
pub fn prompt_queue(answers: Vec<bool>) -> Prompt {
    let q = Arc::new(Mutex::new(VecDeque::from(answers)));
    Box::new(move |_label: &str| match q.lock().pop_front() {
        Some(b) => Ok(b),
        None => panic!("prompt_queue exhausted — test did not provide enough answers"),
    })
}

/// Planner that returns the same plan regardless of input. Drives the batch
/// loop with a fixed hunk partition so the orchestration — not the LLM — is
/// what's under test.
pub fn planner_fixed(plan: generator::BatchPlanOutput) -> BatchPlanner {
    Box::new(
        move |_diff: String| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
            let plan = plan.clone();
            Box::pin(async move { Ok(plan) })
        },
    )
}

/// A one-batch plan carrying hunk 1 of a single file — the whole change, in
/// one commit. The hook e2e tests (issue #20) only need one commit, so the
/// plan is trivially small; the split tests build their multi-batch plans
/// inline instead.
pub fn plan_single_batch(file: &str, reason: &str) -> generator::BatchPlanOutput {
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
pub fn messenger_fixed(msg: &str) -> CommitMessenger {
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
pub fn messenger_then_error(ok_for: usize) -> (CommitMessenger, Arc<Mutex<u32>>) {
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

/// Messenger that returns each successive message in `messages`, with a call
/// counter. Drives the Re-generate action: the second call must yield the
/// message that actually lands.
pub fn messenger_sequence(messages: &[&str]) -> (CommitMessenger, Arc<Mutex<u32>>) {
    let msgs: Vec<String> = messages.iter().map(|m| m.to_string()).collect();
    let calls = Arc::new(Mutex::new(0u32));
    let calls2 = calls.clone();
    let m: CommitMessenger = Box::new(
        move |_diff: String| -> BoxFuture<anyhow::Result<generator::CommitOutput>> {
            let n = {
                let mut g = calls2.lock();
                *g += 1;
                *g as usize
            };
            let msg = msgs
                .get(n - 1)
                .cloned()
                .unwrap_or_else(|| panic!("messenger_sequence exhausted after {n} calls"));
            Box::pin(async move {
                Ok(generator::CommitOutput {
                    message: msg,
                    body: None,
                })
            })
        },
    );
    (m, calls)
}

/// Menu that pops choices from a queue; panics when exhausted so an
/// under-specified test fails loudly instead of silently defaulting.
pub fn menu_queue(choices: Vec<ConfirmChoice>) -> ConfirmMenu {
    let q = Arc::new(Mutex::new(VecDeque::from(choices)));
    Box::new(move |_message: &str| {
        q.lock()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("confirmation menu queue exhausted"))
    })
}

/// Editor that returns a predetermined (subject, body) on every call —
/// the "user edited the message" path.
pub fn editor_fixed(subject: &str, body: Option<&str>) -> CommitEditor {
    let subject = subject.to_string();
    let body = body.map(|b| b.to_string());
    Box::new(move |_s: &str, _b: Option<&str>| {
        Ok(generator::CommitOutput {
            message: subject.clone(),
            body: body.clone(),
        })
    })
}

/// Editor that returns its inputs unchanged — the "user cancelled the edit"
/// path.
pub fn editor_cancel() -> CommitEditor {
    Box::new(|subject: &str, body: Option<&str>| {
        Ok(generator::CommitOutput {
            message: subject.to_string(),
            body: body.map(|b| b.to_string()),
        })
    })
}

/// Editor stub that panics if called — same purpose as [`unreachable_planner`]
/// for the editor: a test whose path must never open an editor fails loudly
/// if a regression reaches it.
pub fn unreachable_editor() -> CommitEditor {
    Box::new(
        |_s: &str, _b: Option<&str>| -> anyhow::Result<generator::CommitOutput> {
            panic!("CommitEditor reached on a path that must not edit")
        },
    )
}

/// Planner stub that panics if called — for tests whose path exits before the
/// batch-plan step, so a regression that reaches the LLM fails loudly instead
/// of silently hitting the network.
pub fn unreachable_planner() -> BatchPlanner {
    Box::new(
        |_diff: String| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
            Box::pin(async { panic!("BatchPlanner reached on a path that must skip the LLM") })
        },
    )
}

/// Messenger stub that panics if called — same purpose as
/// [`unreachable_planner`] for the commit-message step.
pub fn unreachable_messenger() -> CommitMessenger {
    Box::new(
        |_diff: String| -> BoxFuture<anyhow::Result<generator::CommitOutput>> {
            Box::pin(async { panic!("CommitMessenger reached on a path that must skip the LLM") })
        },
    )
}

/// Resolver stub that panics if called — same purpose as
/// [`unreachable_planner`] for the conflict-resolution step.
pub fn unreachable_resolver() -> Resolver {
    Box::new(|_content: String| -> BoxFuture<anyhow::Result<String>> {
        Box::pin(async { panic!("Resolver reached on a path that must skip the LLM") })
    })
}

/// In-memory [`DisplayWrite`] capturing every line the workflow emits. Clones
/// share the underlying buffer so a test can hand one clone to `Display::with`
/// and read the lines back from the other. Most tests just want a quiet sink
/// (via [`sink`]); the mixed-blocker test inspects the captured lines.
#[derive(Clone, Default)]
pub struct BufferWrite(Arc<Mutex<Vec<String>>>);

impl BufferWrite {
    /// Snapshot of every line written so far, in order.
    pub fn lines(&self) -> Vec<String> {
        self.0.lock().clone()
    }
}

impl DisplayWrite for BufferWrite {
    fn write_line(&self, line: &str) {
        self.0.lock().push(line.to_string());
    }

    fn clear_last(&self, n: usize) {
        let mut lines = self.0.lock();
        let keep = lines.len().saturating_sub(n);
        lines.truncate(keep);
    }
}

/// A `Display` backed by a fresh, discarded [`BufferWrite`] — a quiet sink for
/// tests that don't assert on emitted wording. Keeps `cargo test` output free
/// of real merge-conflict noise the workflow would otherwise write to stderr.
pub fn sink() -> Display {
    Display::with(BufferWrite::default())
}

/// Assert the resolve hand-off wording captured in `buf`:
///   - the "not finalized" line exists and carries every `blocker` substring
///     (e.g. `"1 rejected"`, `"1 failed to resolve"`), and
///   - some line reports exactly `approved` staged resolutions
///     (`"N resolved + staged"`).
///
/// Substring-based so each call site keeps the exact wording it pins; shared
/// because the all-rejected and mixed-blocker tests assert the same hand-off
/// shape, and a third hand-off test would otherwise copy it again.
pub fn assert_not_finalized_handoff(buf: &BufferWrite, approved: u32, blockers: &[&str]) {
    let lines = buf.lines();
    let handoff = lines
        .iter()
        .find(|l| l.contains("not finalized"))
        .expect("expected a hand-off 'not finalized' line");
    for &blocker in blockers {
        assert!(
            handoff.contains(blocker),
            "hand-off must report {blocker:?}, got: {handoff:?}"
        );
    }
    let approved_count = format!("{approved} resolved + staged");
    assert!(
        lines.iter().any(|l| l.contains(approved_count.as_str())),
        "hand-off must report {approved_count}, got: {lines:?}"
    );
}

/// Run a `git` command in `dir`, ignoring exit status (merge / rebase return
/// non-zero on conflict, which is the whole point of these setups).
pub fn git_in(dir: &Path, args: &[&str]) {
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
pub fn git_out(dir: &Path, args: &[&str]) -> String {
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
pub fn merge_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    gh::make_content_conflict(dir);
}

/// `init_test_repo` + two tracked files (`alpha.txt`, `beta.txt`) committed at
/// a base value, then each rewritten to a new value and left unstaged — the
/// shared starting point for the inter-file Batch e2e tests (issue #31).
/// After this returns, `alpha.txt == "a1\n"` and `beta.txt == "b1\n"` are
/// unstaged changes over the committed `"a0\n"` / `"b0\n"` base, so a plan
/// can assign each file to its own batch or both to one. Each file holds
/// exactly one one-line change, so git emits a single hunk per file (index 1).
pub fn two_file_unstaged_repo(dir: &Path) {
    gh::init_test_repo(dir);
    std::fs::write(dir.join("alpha.txt"), "a0\n").unwrap();
    std::fs::write(dir.join("beta.txt"), "b0\n").unwrap();
    git_in(dir, &["add", "alpha.txt", "beta.txt"]);
    git_in(dir, &["commit", "-m", "base"]);
    std::fs::write(dir.join("alpha.txt"), "a1\n").unwrap();
    std::fs::write(dir.join("beta.txt"), "b1\n").unwrap();
}

/// Two content conflicts (`tracked.txt` modify/modify, `second.txt` add/add)
/// in one merge. Repo ends in the Merge state.
pub fn merge_two_conflicts(dir: &Path) {
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
pub fn merge_text_and_binary(dir: &Path) {
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
pub fn rebase_conflict(dir: &Path) {
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

/// `git am` that conflicts: master diverges `tracked.txt` from the patch's
/// expected base (`original`), so a hand-crafted mbox patch (original→patched)
/// fails to apply and leaves the repo in an ApplyMailbox state. This is the
/// second refusal path alongside [`rebase_conflict`] (issue #33): same bail
/// code, no test before. Without `--3way`, `git am` never falls back to a
/// 3-way merge, so the state is `ApplyMailbox` (not `ApplyMailboxOrRebase`);
/// both map to the `"am"` label and are refused.
pub fn am_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    // Diverge master so the patch's context (`original`) no longer matches.
    std::fs::write(dir.join("tracked.txt"), "modified on master\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "master override"]);
    // A single well-formed patch that fails to apply. `git am` of a valid mbox
    // that doesn't fit lands in the am state; the `From ` line and `Subject:`
    // header are what make it a mailbox rather than a bare diff.
    let mbox = "\
From 4b825dc642cb6eb9a060e54bf8d69288fbee4904 Mon Sep 17 00:00:00 2001
From: test <test@test.com>
Date: Thu, 1 Jan 1970 00:00:00 +0000
Subject: [PATCH] patch change

---
 tracked.txt | 2 +-
 1 file changed, 1 insertion(+), 1 deletion(-)

diff --git a/tracked.txt b/tracked.txt
--- a/tracked.txt
+++ b/tracked.txt
@@ -1 +1 @@
-original
+patched

";
    std::fs::write(dir.join("patch.mbox"), mbox).unwrap();
    git_in(dir, &["am", "patch.mbox"]);
}

/// Join `n` lines `"{prefix} {i}\n"`. Used to build a file over the
/// `MAX_CONFLICT_LINES` cap so `classify_worktree` returns `Oversized`.
pub fn make_lines(prefix: &str, n: usize) -> String {
    (0..n).map(|i| format!("{prefix} {i}\n")).collect()
}

/// Cherry-pick conflict: master and topic both change `tracked.txt` from the
/// initial commit differently; `git cherry-pick topic` on master hits a content
/// conflict. Repo ends in the CherryPick state.
pub fn cherry_pick_conflict(dir: &Path) {
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
pub fn revert_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "change tracked"]);
    std::fs::write(dir.join("tracked.txt"), "master override\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "master override"]);
    git_in(dir, &["revert", "HEAD~1"]);
}

/// Cherry-pick *sequence* conflict (issue #29): two commits applied by a single
/// `git cherry-pick A B`. The first applies cleanly (it adds `extra.txt`, which
/// master never touched); the second conflicts on `tracked.txt`. Because a
/// sequencer is now active, the repo ends in the `CherryPickSequence` state —
/// distinct from the single-shot `CherryPick` that [`cherry_pick_conflict`]
/// produces (libgit2 keys the two on the `.git/sequencer/` dir). The
/// conflicting commit is the *last* in the sequence, so finalizing it with
/// `git cherry-pick --continue` drains the sequencer and returns the repo to
/// Clean — the behavior the matching finalize test pins.
pub fn cherry_pick_sequence_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    // master advances tracked.txt from the initial "original".
    std::fs::write(dir.join("tracked.txt"), "on master\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "master side"]);
    // topic branches off the initial commit (no master side yet) with two
    // commits: a clean addition, then a tracked.txt change that collides with
    // master.
    git_in(dir, &["branch", "topic", "master~1"]);
    git_in(dir, &["checkout", "topic"]);
    std::fs::write(dir.join("extra.txt"), "feature\n").unwrap();
    git_in(dir, &["add", "extra.txt"]);
    git_in(dir, &["commit", "-m", "topic clean add"]);
    std::fs::write(dir.join("tracked.txt"), "on topic\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "topic tracked change"]);
    git_in(dir, &["checkout", "master"]);
    // Multi-commit cherry-pick: A (clean) then B (conflict). Activating the
    // sequencer is what flips the state to CherryPickSequence.
    git_in(dir, &["cherry-pick", "topic~1", "topic"]);
}

/// Revert *sequence* conflict (issue #29): two reverts issued by a single
/// `git revert P Q`. The first reverts cleanly (it removes `fileP.txt`, which
/// nothing else touched); the second collides with a later change to
/// `tracked.txt`. Because a sequencer is now active, the repo ends in the
/// `RevertSequence` state — distinct from the single-shot `Revert` that
/// [`revert_conflict`] produces. The conflicting revert is the *last* in the
/// sequence, so finalizing it with `git revert --continue` drains the
/// sequencer and returns the repo to Clean — the behavior the matching finalize
/// test pins.
pub fn revert_sequence_conflict(dir: &Path) {
    gh::init_test_repo(dir);
    // P: a standalone addition that reverts cleanly later.
    std::fs::write(dir.join("fileP.txt"), "p\n").unwrap();
    git_in(dir, &["add", "fileP.txt"]);
    git_in(dir, &["commit", "-m", "add fileP"]);
    // Q: change tracked.txt; R: overwrite it so reverting Q conflicts.
    std::fs::write(dir.join("tracked.txt"), "changed\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "change tracked"]);
    std::fs::write(dir.join("tracked.txt"), "master override\n").unwrap();
    git_in(dir, &["add", "tracked.txt"]);
    git_in(dir, &["commit", "-m", "master override"]);
    // Multi-commit revert: revert P (clean) then Q (conflict). Activating the
    // sequencer is what flips the state to RevertSequence. Q is HEAD~1.
    git_in(dir, &["revert", "--no-edit", "HEAD~2", "HEAD~1"]);
}

/// Delete/modify conflict: master deletes `tracked.txt`, other modifies it.
/// The index carries no `our` stage for the path, so `conflicted_files()`
/// classifies it `DeleteModify`. Repo ends in the Merge state.
pub fn merge_delete_modify_conflict(dir: &Path) {
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
pub fn merge_oversized_and_text(dir: &Path) {
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
pub fn merge_mixed_blockers(dir: &Path) {
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

pub fn read_file(dir: &Path, rel: &str) -> String {
    String::from_utf8_lossy(&std::fs::read(dir.join(rel)).unwrap()).into_owned()
}

pub fn file_has_markers(dir: &Path, rel: &str) -> bool {
    git::has_conflict_markers(&read_file(dir, rel))
}

/// Does the index still hold an unmerged entry for `rel`?
pub fn is_unmerged(dir: &Path, rel: &str) -> bool {
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
pub fn staged_blob_has_markers(dir: &Path, rel: &str) -> bool {
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
pub fn is_clean(dir: &Path) -> bool {
    Repository::open(dir).unwrap().state() == git2::RepositoryState::Clean
}

/// Raw `git status --porcelain` output, returned **untrimmed**. The leading
/// `XY` status codes are load-bearing: `" M"` means unstaged while `"M "`
/// means staged, so trimming away the leading space discards the exact signal
/// some tests assert on. Callers that only care about emptiness should use
/// [`worktree_is_empty`]; callers pinning a specific entry keep the leading
/// space and trim only the trailing newline themselves (see
/// [`commit_empty_batch_plan_is_rejected_before_the_loop`]).
pub fn status_porcelain(dir: &Path) -> String {
    git_out(dir, &["status", "--porcelain"])
}

/// `true` if `git status --porcelain` is empty — no staged, unstaged, or
/// untracked entries. The strict "working tree is clean" guarantee the
/// commit-workflow tests pin: a regression that committed *and* re-left the
/// change staged (or dropped it entirely) would keep `is_clean` green but
/// trip this.
pub fn worktree_is_empty(dir: &Path) -> bool {
    status_porcelain(dir).trim().is_empty()
}

/// File content as recorded at a git revision (e.g. "HEAD", "HEAD~1"), via
/// `git show`. Asserts what each batch commit actually contains.
pub fn file_at_ref(dir: &Path, rev: &str, rel: &str) -> String {
    git_out(dir, &["show", &format!("{rev}:{rel}")])
}

/// Total commits reachable from HEAD.
pub fn commit_count(dir: &Path) -> usize {
    git_out(dir, &["rev-list", "--count", "HEAD"])
        .trim()
        .parse()
        .unwrap()
}
