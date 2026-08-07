//! Integration tests for deterministic block grouping (`src/grouping.rs`).
//!
//! These prove the engine's output is a *usable* plan — not just well-shaped:
//! grouped blocks are a valid partition, and driving them through the real
//! [`Staging`] path on a real repo lands exactly one commit per block with the
//! working tree left clean. The exact hunk count git emits is version/codec
//! dependent, so these assert invariants that hold for any partition (every
//! hunk staged once, commit count == blocks) rather than hard-coded counts.

#![cfg(test)]

use super::common::*;
use crate::generator::validate_batch_plan;
use crate::grouping::{GroupFile, GroupingConfig, blocks_to_plan, group};
use crate::staging::Staging;

/// Initialize a repo with a base tree, then leave unstaged edits that produce
/// several hunks across several files and directories.
fn repo_with_scattered_edits(dir: &Path) {
    gh::init_test_repo(dir);
    // Base contents committed at HEAD.
    let pad8: String = (0..8).map(|i| format!("pad{i}\n")).collect();
    std::fs::write(dir.join("a.rs"), format!("a0\n{pad8}c0\n")).unwrap();
    std::fs::write(dir.join("b.rs"), "b0\n").unwrap();
    std::fs::write(dir.join("Cargo.toml"), "version = \"0.1.0\"\n").unwrap();
    git_in(dir, &["add", "a.rs", "b.rs", "Cargo.toml"]);
    git_in(dir, &["commit", "-m", "base tree"]);

    // Unstaged edits: two change sites in a.rs (git splits into ≥2 hunks), one
    // each in b.rs and Cargo.toml.
    std::fs::write(dir.join("a.rs"), format!("a1\n{pad8}c1\n")).unwrap();
    std::fs::write(dir.join("b.rs"), "b1\n").unwrap();
    std::fs::write(dir.join("Cargo.toml"), "version = \"0.2.0\"\n").unwrap();
}

/// Read each file's real workdir-vs-HEAD diff into a [`GroupFile`].
fn group_files(git: &Git, paths: &[&str]) -> Vec<GroupFile> {
    paths
        .iter()
        .map(|p| GroupFile::from_diff(p, &git.diff_workdir(Some(p)).unwrap()))
        .collect::<Vec<_>>()
}

/// The headline invariant: the engine's partition is valid, and staging +
/// committing every block lands exactly one commit per block with the tree
/// clean afterwards — the same path the LLM planner drives, fed a
/// deterministic plan instead.
#[test]
fn grouped_plan_stages_and_commits_every_block() {
    let dir = tempfile::tempdir().unwrap();
    repo_with_scattered_edits(dir.path());

    let git = Git::at(dir.path()).unwrap();
    let files = group_files(&git, &["a.rs", "b.rs", "Cargo.toml"]);
    let blocks = group(&files, &GroupingConfig::default());

    // The grouped plan must be an exact partition of every file's hunks.
    let counts: Vec<(String, usize)> = files
        .iter()
        .map(|f| (f.path.clone(), f.hunks.len()))
        .collect();
    let plan = blocks_to_plan(&blocks);
    validate_batch_plan(&plan, &counts)
        .expect("grouped output must partition every hunk exactly once");

    // Cross-file off + each file carrying ≥1 block ⇒ at least one block per
    // file, no more than the total hunk count.
    let total_hunks: usize = counts.iter().map(|(_, n)| n).sum();
    assert!(
        blocks.len() >= files.len(),
        "expected ≥1 block per file, got {} blocks for {} files",
        blocks.len(),
        files.len()
    );
    assert!(
        blocks.len() <= total_hunks,
        "expected ≤{total_hunks} blocks, got {}",
        blocks.len()
    );

    // Drive the real staging path: one commit per block.
    let start_commits = commit_count(dir.path());
    let mut staging = Staging::new();
    for (i, block) in blocks.iter().enumerate() {
        let paths = staging
            .stage_batch(&git, &block.to_batch(), &sink())
            .unwrap_or_else(|e| panic!("stage block {i} failed: {e:#}"));
        assert!(
            !paths.is_empty(),
            "block {i} staged no files — partition invariant broken"
        );
        git.commit(format!("block {i}"), None)
            .unwrap_or_else(|e| panic!("commit block {i} failed: {e:#}"));
    }

    assert_eq!(
        commit_count(dir.path()),
        start_commits + blocks.len(),
        "expected one commit per block"
    );
    assert!(
        worktree_is_empty(dir.path()),
        "working tree must be clean after staging every block — leftover:\n{}",
        status_porcelain(dir.path())
    );
}

/// Adjacency is observable on a real diff: with the gap ceiling raised to cover
/// the real inter-hunk gap, a multi-hunk file collapses to one block; with it
/// at zero (and no context signal), the hunks stay split whenever git left a
/// gap between them. Asserted from the *parsed* gap so it is robust to git's
/// own hunk-folding rather than pinning a magic pad length.
#[test]
fn adjacency_collapses_or_splits_a_multi_hunk_file() {
    let dir = tempfile::tempdir().unwrap();
    repo_with_scattered_edits(dir.path());

    let git = Git::at(dir.path()).unwrap();
    let mut files = group_files(&git, &["a.rs"]);
    let a = files.remove(0);
    let hunk_count = a.hunks.len();
    assert!(
        hunk_count >= 2,
        "test setup must yield ≥2 hunks in a.rs, got {hunk_count} — adjust the pad length",
    );

    // Real unchanged-line gap between the first two hunks (same formula as the
    // engine's private `unchanged_gap`).
    let gap = a.hunks[1]
        .old_start
        .saturating_sub(a.hunks[0].old_start + a.hunks[0].old_count);

    // Raising the ceiling to cover the real gap merges the whole file.
    let merged = group(
        std::slice::from_ref(&a),
        &GroupingConfig {
            adjacency_gap: gap,
            join_same_context: false,
            cross_file_same_scope: false,
        },
    );
    assert_eq!(merged.len(), 1, "gap-covered ceiling must merge all hunks");

    // With ceiling 0 and no context signal, the file splits whenever git left a
    // gap between its first two hunks.
    if gap > 0 {
        let split = group(
            std::slice::from_ref(&a),
            &GroupingConfig {
                adjacency_gap: 0,
                join_same_context: false,
                cross_file_same_scope: false,
            },
        );
        assert!(
            split.len() >= 2,
            "zero ceiling with a real gap must keep hunks split",
        );
    }
}
