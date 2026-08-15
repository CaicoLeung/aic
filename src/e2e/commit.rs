//! default `aic` Run workflow — auto-detect, batch split, staged single commit, partial failure, empty plan.

use super::common::*;

/// `aic` on a clean repo (nothing staged, nothing unstaged) prints the
/// nothing-to-commit notice and returns without calling the LLM or prompting.
#[tokio::test]
async fn commit_clean_repo_is_a_noop() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let (_resolver, seen) = resolver_recording();
    let _prompt = prompt_queue(vec![]); // empty — must not be asked
    let git = Git::at(dir.path()).unwrap();

    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: unreachable_planner(),
            messenger: unreachable_messenger(),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(result.is_ok(), "clean repo should not error: {:?}", result);
    assert!(
        seen.lock().is_empty(),
        "LLM resolver must not run when there are no changes"
    );
    assert!(is_clean(dir.path()));
}

/// Default `aic` run auto-detects a conflicted repo and, when the user declines
/// `resolve now?`, aborts with a clear redirect — never reaching the resolver
/// or the normal commit flow.
#[tokio::test]
async fn commit_run_auto_detect_aborts_when_user_declines() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let (resolver, seen) = resolver_recording();
    let git = Git::at(dir.path()).unwrap();

    let err = default_run(
        &git,
        ResolveDeps {
            resolve: resolver,
            prompt: prompt_queue(vec![false]),
            display: sink(),
        },
        RunDeps {
            display: sink(),
            planner: unreachable_planner(),
            messenger: unreachable_messenger(),
            confirm: Confirm::Disabled,
        },
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

/// Default `aic` run on a rebase state: `aic resolve` refuses rebase/am (ADR
/// 0005), so the run never offers it — it aborts immediately, naming the
/// manual continuation instead of redirecting to a command that would refuse.
#[tokio::test]
async fn commit_run_rebase_state_aborts_with_manual_continuation() {
    let dir = tempfile::tempdir().unwrap();
    rebase_conflict(dir.path());

    let (resolver, seen) = resolver_recording();
    let git = Git::at(dir.path()).unwrap();

    let err = default_run(
        &git,
        ResolveDeps {
            resolve: resolver,
            // Empty queue: the resolve offer must never fire — if it does,
            // the exhausted queue panics and fails this test loudly.
            prompt: prompt_queue(vec![]),
            display: sink(),
        },
        RunDeps {
            display: sink(),
            planner: unreachable_planner(),
            messenger: unreachable_messenger(),
            confirm: Confirm::Disabled,
        },
    )
    .await
    .expect_err("rebase state must abort without offering resolve");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mid-rebase") && msg.contains("git rebase --continue"),
        "expected manual-continuation abort, got: {msg}"
    );
    assert!(
        !msg.contains("aic resolve"),
        "must not point at a command that refuses rebase: {msg}"
    );
    assert!(seen.lock().is_empty(), "resolver must not run on rebase");
    assert!(!is_clean(dir.path()), "repo must be left untouched");
}

/// The commit guard — the deeper net under the front door — gives the same
/// honest advice for rebase states: the manual continuation, not `aic resolve`.
#[test]
fn commit_guard_names_manual_continuation_for_rebase() {
    let dir = tempfile::tempdir().unwrap();
    rebase_conflict(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let err = git
        .assert_commit_safe()
        .expect_err("guard must refuse to commit mid-rebase");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("mid-rebase") && msg.contains("git rebase --continue"),
        "expected manual-continuation hint, got: {msg}"
    );
    assert!(
        !msg.contains("aic resolve"),
        "must not point at a command that refuses rebase: {msg}"
    );
}

/// Default `aic` run: user accepts `resolve now?`, resolver returns clean
/// content, user approves — the conflicted repo is resolved and finalized
/// through the commit-workflow entry point.
#[tokio::test]
async fn commit_run_auto_detect_yes_routes_to_full_resolve() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let resolver = resolver_returning("merged\n");
    // [0] = "resolve now?" yes, [1] = "apply tracked.txt?" yes
    let git = Git::at(dir.path()).unwrap();

    let result = default_run(
        &git,
        ResolveDeps {
            resolve: resolver,
            prompt: prompt_queue(vec![true, true]),
            display: sink(),
        },
        RunDeps {
            display: sink(),
            planner: unreachable_planner(),
            messenger: unreachable_messenger(),
            confirm: Confirm::Disabled,
        },
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

/// The third auto-detect combination (issue #32): the user accepts `resolve
/// now?` — so the resolve workflow runs and the LLM proposes a resolution —
/// but then rejects every proposed file. The sibling tests cover the two other
/// exits from the auto-detect prompt: [`commit_run_auto_detect_aborts_when_user_declines`]
/// (decline the offer outright, never reaching the resolver) and
/// [`commit_run_auto_detect_yes_routes_to_full_resolve`] (accept then approve).
/// The all-rejected hand-off is asserted piecewise by the direct-resolve
/// partial-approval test, but never through the auto-detect entry point, so a
/// regression that, say, finalized on an all-rejected repo or mis-counted the
/// approved/rejected split *via the commit run* would ship green.
///
/// Contract pinned here: nothing is staged, the merge is NOT finalized (the
/// rejected file stays unmerged with its markers), and the emitted hand-off
/// reports zero approved plus one rejected.
#[tokio::test]
async fn commit_run_auto_detect_yes_then_rejects_every_resolution() {
    let dir = tempfile::tempdir().unwrap();
    merge_conflict(dir.path());

    let resolver = resolver_returning("merged\n");
    // [0] = "resolve now?" yes (route into the resolve workflow),
    // [1] = "apply tracked.txt?" no (reject the only proposed resolution).
    let git = Git::at(dir.path()).unwrap();

    let buf = BufferWrite::default();
    let display = Display::with(buf.clone());
    let result = default_run(
        &git,
        ResolveDeps {
            resolve: resolver,
            prompt: prompt_queue(vec![true, false]),
            display,
        },
        RunDeps {
            display: sink(),
            planner: unreachable_planner(),
            messenger: unreachable_messenger(),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "all-rejected hand-off should not error: {:?}",
        result
    );

    // Approved count is zero → nothing was written or staged. The rejected
    // file is untouched: still unmerged, still carrying its conflict markers,
    // and NOT holding the resolver's proposed content.
    assert!(
        !is_clean(dir.path()),
        "an all-rejected run must NOT finalize the merge"
    );
    assert!(
        file_has_markers(dir.path(), "tracked.txt"),
        "rejected file must keep its conflict markers"
    );
    assert!(
        is_unmerged(dir.path(), "tracked.txt"),
        "rejected file must stay unmerged in the index"
    );
    assert_ne!(
        read_file(dir.path(), "tracked.txt"),
        "merged\n",
        "the rejected resolution must NOT be written to the worktree"
    );

    // The whole point: the hand-off reports the all-rejected outcome —
    // zero approved, exactly one rejected, and the merge is not finalized.
    assert_not_finalized_handoff(&buf, 0, &["1 rejected"]);
}

/// Headline hunk-split behavior: one file with two unrelated changes lands as
/// TWO atomic commits, each carrying only its assigned hunk. Drives the full
/// batch loop (capture diff → validate → per-batch stage + commit) against a
/// real repo with a stub plan and stub commit messages, so no LLM is contacted.
#[tokio::test]
async fn commit_splits_one_file_across_two_batches() {
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
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("chore: stub"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "two-batch split should succeed: {:?}",
        result
    );

    // One commit per batch — two new commits over the base.
    assert_eq!(
        commit_count(dir.path()),
        before + 2,
        "two batches must land two commits, one per batch"
    );
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

/// Inter-file batching, two-batch shape (issue #31): the marquee split test
/// [`commit_splits_one_file_across_two_batches`] proves aic can split *one*
/// file's hunks across commits; this pins the other axis — a plan that
/// assigns *different files* to different batches. Batch 1 stages and commits
/// only `alpha.txt`; batch 2 stages and commits only `beta.txt`. The
/// per-batch staging must not leak the other file's unstaged change into the
/// wrong commit, so each commit's tree carries only its assigned file's
/// change. A regression that staged every file up front (or dropped the
/// per-file scoping in `stage_batch_hunks`) would land both changes in batch
/// 1's commit and trip the HEAD~1 assertions.
#[tokio::test]
async fn commit_splits_two_files_across_two_batches() {
    let dir = tempfile::tempdir().unwrap();
    two_file_unstaged_repo(dir.path());

    // Stub plan: one file per batch — batch 1 = alpha, batch 2 = beta.
    let plan = generator::BatchPlanOutput {
        batches: vec![
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change alpha".into()),
            },
            generator::BatchPlanBatch {
                changes: vec![generator::BatchChange {
                    file: "beta.txt".to_string(),
                    hunks: vec![1],
                }],
                reason: Some("change beta".into()),
            },
        ],
    };
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("chore: stub"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "two-file two-batch split should succeed: {:?}",
        result
    );

    // One commit per batch — two new commits, neither file leaked into the other's.
    assert_eq!(
        commit_count(dir.path()),
        before + 2,
        "two batches must land two commits, one per batch"
    );
    // Batch 1 (HEAD~1): only alpha changed; beta still at its base value.
    assert_eq!(
        file_at_ref(dir.path(), "HEAD~1", "alpha.txt"),
        "a1\n",
        "batch 1 must commit alpha's change"
    );
    assert_eq!(
        file_at_ref(dir.path(), "HEAD~1", "beta.txt"),
        "b0\n",
        "batch 1 must NOT leak beta's unstaged change into its commit"
    );
    // Batch 2 (HEAD): both files changed, working tree clean.
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "alpha.txt"),
        "a1\n",
        "alpha's change must persist into batch 2's commit"
    );
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "beta.txt"),
        "b1\n",
        "batch 2 must commit beta's change"
    );
    assert!(
        is_clean(dir.path()),
        "working tree must be clean at the end"
    );
    assert!(
        worktree_is_empty(dir.path()),
        "nothing must be left staged or unstaged"
    );
}

/// Inter-file batching, single-commit shape (issue #31): the optional
/// counterpart to [`commit_splits_two_files_across_two_batches`]. A plan that
/// assigns two distinct files to one batch must stage both and commit them
/// together as a single commit — exercising `unique_batch_files` collecting
/// two paths and the messenger drafting one message for the pair. A
/// regression that committed each file separately would land two commits
/// instead of one and trip the count assertion; one that dropped a file would
/// trip the tree assertion.
#[tokio::test]
async fn commit_batches_two_files_into_one_commit() {
    let dir = tempfile::tempdir().unwrap();
    two_file_unstaged_repo(dir.path());

    // Stub plan: one batch carrying both files — one commit for the pair.
    let plan = generator::BatchPlanOutput {
        batches: vec![generator::BatchPlanBatch {
            changes: vec![
                generator::BatchChange {
                    file: "alpha.txt".to_string(),
                    hunks: vec![1],
                },
                generator::BatchChange {
                    file: "beta.txt".to_string(),
                    hunks: vec![1],
                },
            ],
            reason: Some("change both".into()),
        }],
    };
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("chore: both files"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "two-file single-batch Run should succeed: {:?}",
        result
    );

    // Exactly one new commit — not one per file.
    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "a batch carrying two files must land a single commit, not two"
    );
    // That one commit's tree carries both files' changes.
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "alpha.txt"),
        "a1\n",
        "the single commit must contain alpha's change"
    );
    assert_eq!(
        file_at_ref(dir.path(), "HEAD", "beta.txt"),
        "b1\n",
        "the single commit must contain beta's change"
    );
    assert!(
        is_clean(dir.path()),
        "working tree must be clean at the end"
    );
    assert!(
        worktree_is_empty(dir.path()),
        "nothing must be left staged or unstaged"
    );
}

/// Regression: a binary file in the unstaged area must be committed by the
/// batch-plan Run, not silently dropped. Binary deltas carry no `@@` hunks, so
/// `parse_file_patch` reports zero hunks and the whole-file entry (`hunks: []`)
/// is the only way to carry one. Staging must treat a zero-hunk, non-empty
/// workdir diff as an atomic whole-file stage (`git add`), not as "nothing to
/// do" — otherwise the binary change is left unstaged forever and the Run
/// reports success. Reported: "binary files remain in the unstaged area, they
/// will not be included in the batch plan."
#[tokio::test]
async fn commit_includes_binary_file_in_batch_plan() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // A tracked binary file, committed at a base value, then rewritten in the
    // workdir and left unstaged — the entry condition for the batch-plan path.
    std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    git_in(dir.path(), &["add", "blob.bin"]);
    git_in(dir.path(), &["commit", "-m", "add binary"]);
    std::fs::write(dir.path().join("blob.bin"), [9u8, 9, 9, 9]).unwrap();

    // Stub plan carrying the whole binary file (no hunks to index). This is the
    // plan a correct model would emit — the bug is downstream of the planner.
    let plan = generator::BatchPlanOutput {
        batches: vec![generator::BatchPlanBatch {
            changes: vec![generator::BatchChange {
                file: "blob.bin".to_string(),
                hunks: vec![],
            }],
            reason: Some("update binary".into()),
        }],
    };
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("chore: update blob"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(result.is_ok(), "binary batch should succeed: {:?}", result);
    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "the binary change must land one commit"
    );
    assert!(
        worktree_is_empty(dir.path()),
        "binary file must be committed, not left in the unstaged area"
    );
}

/// Layer-3 coverage: the batch-plan path must send the model an explicit
/// binary marker (not an empty diff string) for a zero-hunk file, so the model
/// includes the file with an empty `hunks` array. `commit_includes_binary_file_
/// in_batch_plan` stubs the planner with `planner_fixed`, which discards its
/// input — so the marker string (the actual fix for the model-facing layer) was
/// computed then thrown away, never asserted. This captures what was sent.
#[tokio::test]
async fn batch_plan_sends_binary_marker_for_zero_hunk_file() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    std::fs::write(dir.path().join("blob.bin"), [0u8, 1, 2, 3]).unwrap();
    git_in(dir.path(), &["add", "blob.bin"]);
    git_in(dir.path(), &["commit", "-m", "add binary"]);
    std::fs::write(dir.path().join("blob.bin"), [9u8, 9, 9, 9]).unwrap();

    let plan = generator::BatchPlanOutput {
        batches: vec![generator::BatchPlanBatch {
            changes: vec![generator::BatchChange {
                file: "blob.bin".to_string(),
                hunks: vec![],
            }],
            reason: Some("update binary".into()),
        }],
    };
    let (planner, sent) = planner_capture(plan);
    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner,
            messenger: messenger_fixed("chore: update blob"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(result.is_ok(), "batch should succeed: {:?}", result);

    let sent = sent.lock();
    assert_eq!(sent.len(), 1, "planner must be called exactly once");
    let payload = &sent[0];
    assert!(
        payload.contains(crate::llm::prompt::BINARY_MARKER),
        "a zero-hunk file must reach the model as the binary marker, not an empty diff"
    );
    assert!(
        payload.contains("blob.bin"),
        "the binary file's path must appear in the model payload"
    );
}

/// Coverage for the mode-only case the fix's root-cause scopes ("binary /
/// mode-only / pure-rename") but which only a rewritten binary blob exercised.
/// A pure executable-bit flip yields a non-empty, zero-hunk workdir diff — the
/// same `hunk_count == 0 && !diff.trim().is_empty()` shape — and must be
/// committed whole via `git add`, not silently dropped.
#[cfg(unix)]
#[tokio::test]
async fn commit_includes_mode_only_change_in_batch_plan() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let script = dir.path().join("script.sh");
    std::fs::write(&script, "#!/bin/sh\necho hi\n").unwrap();
    git_in(dir.path(), &["add", "script.sh"]);
    git_in(dir.path(), &["commit", "-m", "add script"]);
    // Flip only the executable bit — content is byte-identical, so the workdir
    // diff is mode-only (`old mode 100644` / `new mode 100755`), zero hunks.
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

    let plan = generator::BatchPlanOutput {
        batches: vec![generator::BatchPlanBatch {
            changes: vec![generator::BatchChange {
                file: "script.sh".to_string(),
                hunks: vec![],
            }],
            reason: Some("make script executable".into()),
        }],
    };
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();
    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("chmod: make script executable"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "mode-only batch should succeed: {:?}",
        result
    );
    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "the mode-only change must land one commit"
    );
    assert!(
        worktree_is_empty(dir.path()),
        "mode-only change must be committed, not left unstaged"
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
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // Modify a tracked file and stage it — the entry condition for the staged
    // single-commit path. A non-Rust file keeps this test focused on the
    // commit shape.
    std::fs::write(dir.path().join("tracked.txt"), "staged change\n").unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: unreachable_planner(), // staged path must NOT plan,
            messenger: messenger_fixed("feat: staged change"),
            confirm: Confirm::Disabled,
        },
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
    let status = status_porcelain(dir.path());
    assert!(
        status.trim().is_empty(),
        "working tree must be clean after the staged commit, got: {status:?}"
    );
}

/// A mid-loop failure (here: the 2nd batch's message step errors after batch 1
/// already committed) must abort with the unified message naming how many
/// batches committed — and those earlier commits must persist in the repo.
/// Guards the [important] partial-failure UX contract.
#[tokio::test]
async fn commit_batch_loop_aborts_after_partial_commit() {
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
    let git = Git::at(dir.path()).unwrap();
    let err = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger,
            confirm: Confirm::Disabled,
        },
    )
    .await
    .expect_err("must abort when a later batch fails");

    let msg = format!("{err:#}");
    assert!(
        msg.contains("aborted on batch 2"),
        "expected batch-2 abort, got: {msg}"
    );
    assert!(
        msg.contains("1 batch committed"),
        "expected 1 committed, got: {msg}"
    );
    assert_eq!(*calls.lock(), 2, "messenger called once per batch");
    // Batch 1 DID commit — its hunk is in HEAD despite the later failure; the
    // failed batch 2's hunk is staged but NOT committed.
    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(head.contains("a1"), "batch 1 must be committed");
    assert!(!head.contains("c1"), "batch 2 must NOT be committed");
}

/// Issue #34, under the deterministic-fallback contract: an empty Batch plan
/// (the planner returns zero batches over real unstaged work) is an LLM
/// malfunction, not a user problem. The Run warns, regroups with the
/// deterministic engine, and *completes* — the silent no-op #34 feared cannot
/// recur, because the engine's output is a partition over the real work, so
/// real changes always land in ≥1 batch. Pinned end-to-end: the warn notice
/// renders, the fallback plan drives the normal loop (messenger reached per
/// batch), and the work is committed.
#[tokio::test]
async fn commit_invalid_plan_falls_back_to_deterministic_grouping() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // Two single-hunk edits in different directories — conservative defaults
    // keep them as two separate batches, proving the fallback plan (not a
    // fluke single-batch regroup) drives the loop.
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::create_dir_all(dir.path().join("docs")).unwrap();
    std::fs::write(dir.path().join("src/a.rs"), "base a\n").unwrap();
    std::fs::write(dir.path().join("docs/b.md"), "base b\n").unwrap();
    git_in(dir.path(), &["add", "."]);
    git_in(dir.path(), &["commit", "-m", "base tree"]);
    std::fs::write(dir.path().join("src/a.rs"), "new a\n").unwrap();
    std::fs::write(dir.path().join("docs/b.md"), "new b\n").unwrap();

    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();
    let buf = BufferWrite::default();
    let (messenger, calls) =
        messenger_sequence(&["feat(a): fallback draft a", "docs(b): fallback draft b"]);

    let result = commit_run(
        &git,
        RunDeps {
            display: Display::with(buf.clone()),
            // Zero batches over real work — the most degenerate invalid plan.
            planner: planner_fixed(generator::BatchPlanOutput { batches: vec![] }),
            messenger,
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(result.is_ok(), "fallback must complete the run: {result:?}");

    // The notice rendered: the user knows the LLM's plan was discarded.
    let lines = buf.lines();
    let notice = lines
        .iter()
        .find(|l| l.contains("regrouping deterministically"))
        .unwrap_or_else(|| panic!("expected fallback notice, got: {lines:?}"))
        .clone();

    // The fallback plan drove the normal loop: two batches, two drafts, two
    // commits, clean tree.
    assert_eq!(*calls.lock(), 2, "one draft per fallback batch");
    assert_eq!(commit_count(dir.path()), before + 2);
    assert!(is_clean(dir.path()), "all work must be committed");
    assert!(notice.contains("no batches"), "the LLM's failure is named");
}

/// A pre-commit hook that re-stages whole files (the lint-staged/prettier
/// pattern used by the maintainer's own aic-web repo: `prettier --write` then
/// `git add`) silently broadens a batch commit: staging hunk 1 and committing
/// lands the *entire* file, because the hook `git add`s the full worktree
/// content back into the index. The plan's later batch for that file then has
/// nothing left to stage, and replaying the plan-time snapshot used to die with
/// `git apply`'s "patch does not apply". The workflow must skip the swallowed
/// batch and finish the Run instead of aborting.
#[tokio::test]
async fn commit_batch_loop_survives_pre_commit_hook_that_re_stages_whole_files() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // A base with two change sites far enough apart that git emits two
    // separate hunks (>= ~6 unchanged lines between them).
    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}c0\n");
    let changed = format!("a1\n{pad}c1\n");
    std::fs::write(dir.path().join("tracked.txt"), &base).unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);
    git_in(dir.path(), &["commit", "-m", "base"]);
    std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

    // Simulate lint-staged: the pre-commit hook re-stages the full file, so a
    // commit of hunk 1 actually lands both hunks.
    let hooks = dir.path().join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    let hook = hooks.join("pre-commit");
    std::fs::write(&hook, "#!/bin/sh\ngit add tracked.txt\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

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
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("feat: hook swallows the rest"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "the Run must complete, not abort, when a hook commits more than the batch: {:?}",
        result
    );

    // Only batch 1 lands (it carries the whole file); the swallowed batch 2 is
    // skipped rather than failing on a stale snapshot.
    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "exactly one commit must land — the second batch has nothing left to stage"
    );
    // The hook's re-stage means both hunks are in that one commit.
    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(head.contains("a1"), "hunk 1 must be committed");
    assert!(
        head.contains("c1"),
        "hunk 2 must be committed too (hook re-staged it)"
    );
    assert!(
        is_clean(dir.path()),
        "working tree must be clean after the Run"
    );
}

/// Regression for the 3+-hunks-one-file case. The two-batch split test above
/// (`commit_splits_one_file_across_two_batches`) hides it: with no pre-commit
/// hook at all, splitting a single file's three hunks across three batches
/// must still land one commit per hunk. An earlier hook-survival fix once
/// stored *current* (remapped) hunk positions back into the per-file
/// `committed` set instead of *original* indices — so batch 2 recorded
/// position 1 instead of original hunk 2, batch 3 then addressed "hunk 2 of a
/// 1-hunk diff" and the Run aborted. This test pins the original-index
/// bookkeeping end-to-end.
#[tokio::test]
async fn commit_splits_one_file_across_three_batches() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    // Three change sites, each padded far enough apart that git emits three
    // separate hunks.
    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}b0\n{pad}c0\n");
    let changed = format!("a1\n{pad}b1\n{pad}c1\n");
    std::fs::write(dir.path().join("tracked.txt"), &base).unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);
    git_in(dir.path(), &["commit", "-m", "base"]);
    std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

    // Stub plan: split the single file's three hunks across three batches.
    let mk = |h: usize| generator::BatchPlanBatch {
        changes: vec![generator::BatchChange {
            file: "tracked.txt".to_string(),
            hunks: vec![h],
        }],
        reason: Some(format!("change {h}")),
    };
    let plan = generator::BatchPlanOutput {
        batches: vec![mk(1), mk(2), mk(3)],
    };
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("chore: stub"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "three-batch split should succeed: {:?}",
        result
    );

    // One commit per batch — three new commits over the base.
    assert_eq!(
        commit_count(dir.path()),
        before + 3,
        "three batches must land three commits, one per batch"
    );
    // Each commit carries exactly its prefix of hunks — no leak forward, no
    // skip. HEAD~2 is batch 1 (a1 only), HEAD~1 adds batch 2 (b1), HEAD adds
    // batch 3 (c1).
    let head2 = file_at_ref(dir.path(), "HEAD~2", "tracked.txt");
    assert!(head2.contains("a1"), "batch 1 must include hunk 1");
    assert!(
        !head2.contains("b1") && !head2.contains("c1"),
        "batch 1 must be hunk 1 only"
    );
    let head1 = file_at_ref(dir.path(), "HEAD~1", "tracked.txt");
    assert!(
        head1.contains("a1") && head1.contains("b1"),
        "batches 1 and 2 must be applied by HEAD~1"
    );
    assert!(!head1.contains("c1"), "batch 2 must not include hunk 3");
    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(
        head.contains("a1") && head.contains("b1") && head.contains("c1"),
        "all three hunks must be committed by HEAD"
    );
    assert!(
        is_clean(dir.path()),
        "working tree must be clean at the end"
    );
}

/// Two `changes` entries for the SAME file in one batch (disjoint hunks) must
/// land exactly one commit carrying both hunks — not two commits, and not a
/// commit-message prompt that lists the file twice. This is the contract the
/// removed `unique_batch_files` helper enforced; `stage_batch_hunks` now
/// restores it structurally by grouping a file's hunks across its entries
/// before staging.
#[tokio::test]
async fn commit_batch_merges_same_file_changes_into_one_commit() {
    let dir = tempfile::tempdir().unwrap();
    gh::init_test_repo(dir.path());

    let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    let base = format!("a0\n{pad}b0\n");
    let changed = format!("a1\n{pad}b1\n");
    std::fs::write(dir.path().join("tracked.txt"), &base).unwrap();
    git_in(dir.path(), &["add", "tracked.txt"]);
    git_in(dir.path(), &["commit", "-m", "base"]);
    std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

    // One batch, TWO changes entries for the same file with disjoint hunks.
    // `validate_batch_plan` accepts this (no duplicate hunk); the workflow
    // must treat it as one logical unit.
    let plan = generator::BatchPlanOutput {
        batches: vec![generator::BatchPlanBatch {
            changes: vec![
                generator::BatchChange {
                    file: "tracked.txt".to_string(),
                    hunks: vec![1],
                },
                generator::BatchChange {
                    file: "tracked.txt".to_string(),
                    hunks: vec![2],
                },
            ],
            reason: Some("both edits".into()),
        }],
    };
    let before = commit_count(dir.path());
    let git = Git::at(dir.path()).unwrap();

    let result = commit_run(
        &git,
        RunDeps {
            display: sink(),
            planner: planner_fixed(plan),
            messenger: messenger_fixed("feat: same-file disjoint hunks"),
            confirm: Confirm::Disabled,
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "one-batch same-file split should succeed: {:?}",
        result
    );

    // Exactly one commit — the two entries merge into one logical unit, never
    // two, and the file is staged once rather than listed twice in the prompt.
    assert_eq!(
        commit_count(dir.path()),
        before + 1,
        "two changes entries for one file must merge into one commit"
    );
    let head = file_at_ref(dir.path(), "HEAD", "tracked.txt");
    assert!(
        head.contains("a1") && head.contains("b1"),
        "both hunks must be in the single commit"
    );
    assert!(
        is_clean(dir.path()),
        "working tree must be clean after the Run"
    );
}
