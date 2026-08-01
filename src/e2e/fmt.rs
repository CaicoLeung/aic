//! cargo fmt runs before the unstaged diff is captured (hunk-stability guard).

use super::common::*;

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
