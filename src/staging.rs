//! Batch staging — the phase of a Run that maps a Batch's planned hunks onto
//! the current index→workdir diff and stages them (CONTEXT.md "Batch
//! staging").
//!
//! This module owns the bookkeeping a Run needs to stage one Batch after
//! another: which **original** (plan-time) hunk indices have already landed.
//! That state lives inside [`Staging`], so callers never see the remap
//! arithmetic or the two index spaces it bridges — the class of bug that
//! aborted 3+-hunks-one-file Runs when a remapped position was written back
//! as an original index.
//!
//! Staging reaches into the git adapter through the `&Git` handle the
//! workflow passes in (`diff_workdir`, `stage_hunks`) and the pure diff
//! parser (`parse_file_patch`) directly; its behavior is exercised
//! end-to-end by the e2e suite against real repos, including the
//! pre-commit-hook restaging path that makes the fresh-diff strategy
//! load-bearing.

use anyhow::{Context, Result};
use std::collections::{HashMap, HashSet};

use crate::diff::parse_file_patch;
use crate::display::Display;
use crate::generator::BatchPlanBatch;
use crate::git::Git;

/// Stages one Batch at a time, tracking per-file which original hunk indices
/// have already landed in earlier batches of the same Run.
pub struct Staging {
    /// Original (plan-time) hunk indices already committed per file. Storing
    /// original indices — never remapped positions — keeps the numbering the
    /// model saw stable for later batches.
    committed_hunks: HashMap<String, HashSet<usize>>,
}

impl Staging {
    pub fn new() -> Self {
        Self {
            committed_hunks: HashMap::new(),
        }
    }

    /// Stage one batch's hunks from the *current* index→workdir diff and
    /// return the paths actually staged — deduplicated by file, in first-seen
    /// order.
    ///
    /// Staging from a fresh diff (rather than the plan-time snapshot) is what
    /// keeps a Run alive when a pre-commit hook commits more than the batch
    /// staged: lint-staged/prettier re-add whole files, so the first batch to
    /// touch a file can land *all* of its hunks, and the plan's later batches
    /// for that file have nothing left to stage. Replaying the stale snapshot
    /// then dies with `git apply`'s "patch does not apply". Files whose
    /// changes already landed are skipped (with a notice) and the run
    /// continues with the rest.
    ///
    /// A file split across several `changes` entries of one batch (disjoint
    /// hunks) is merged into a single staging pass and a single entry of the
    /// returned list, so it produces one commit message — the contract the old
    /// `unique_batch_files` enforced. Grouping also stages all of a file's
    /// hunks in one `git apply` (cheaper than one call per entry) and keeps
    /// `committed_hunks` free of within-batch cross-entry remapping: each
    /// file's current-diff fetch happens once, before any of its hunks are
    /// staged.
    pub fn stage_batch(
        &mut self,
        git: &Git,
        batch: &BatchPlanBatch,
        display: &Display,
    ) -> Result<Vec<String>> {
        // Gather each file's planned hunks across its (possibly several)
        // `changes` entries, preserving first-seen file order. An empty `hunks`
        // slice means "the whole file" for that entry; `validate_batch_plan`
        // rejects mixing whole-file and per-hunk entries for the same file, so
        // concatenating the slices yields the right `planned` for every
        // reachable plan (a lone whole-file entry stays empty →
        // `map_planned_hunks` expands it).
        let mut files: Vec<String> = Vec::new();
        let mut hunks_by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for change in &batch.changes {
            if !hunks_by_file.contains_key(&change.file) {
                files.push(change.file.clone());
            }
            hunks_by_file
                .entry(change.file.clone())
                .or_default()
                .extend_from_slice(&change.hunks);
        }

        let mut staged_paths: Vec<String> = Vec::new();
        for file in &files {
            let planned = hunks_by_file
                .get(file)
                .expect("every file in `files` has an entry in `hunks_by_file`");
            let current = git.diff_workdir(Some(file.as_str()))?;
            if current.trim().is_empty() {
                display.warn(&format!(
                    "{}: all its changes were already committed (a pre-commit hook may have \
                     staged the whole file) — nothing left in this batch",
                    file
                ));
                continue;
            }
            let patch = parse_file_patch(&current);
            let committed = self.committed_hunks.entry(file.clone()).or_default();
            let mapping = map_planned_hunks(planned, committed, patch.hunk_count());
            if mapping.current.is_empty() {
                continue;
            }
            git.stage_hunks(&current, &mapping.current)
                .with_context(|| format!("staging hunks for {}", file))?;
            // Record ORIGINAL indices (not the remapped current positions) so
            // the plan-time numbering the model saw stays stable for later
            // batches.
            committed.extend(mapping.original);
            staged_paths.push(file.clone());
        }
        Ok(staged_paths)
    }
}

/// One file's planned hunks for a batch, split into two views that must
/// never be confused:
///
/// - `original`: 1-based indices in the file's **plan-time** diff — the
///   numbering the model saw and the plan refers to. Recorded into `committed`
///   so it stays stable for the rest of the Run.
/// - `current`: matching 1-based positions in the file's **current**
///   index→workdir diff — what `Git::stage_hunks` must index into, because an
///   earlier batch (or a pre-commit hook) may have already landed some hunks
///   and shrunk the diff.
///
/// Hunks already committed are dropped from both. Surviving hunks keep their
/// original relative order, so an uncommitted original hunk `h` lands at
/// current position `h - (#committed original indices < h)`. Splitting the two
/// spaces into named fields (rather than returning one ambiguous `Vec`) is
/// what stops callers from writing a remapped position back as an original
/// index — the class of bug that aborted 3+-hunks-one-file Runs.
struct HunkMapping {
    original: Vec<usize>,
    current: Vec<usize>,
}

/// Resolve a batch's planned hunks for one file against what's already
/// committed.
///
/// `planned` is the plan's `hunks` array for the file (empty = every hunk of
/// the file); `committed` is the set of **original** hunk indices already
/// landed by earlier batches; `current_count` is how many hunks the current
/// diff still has. When `planned` is empty the original count is reconstructed
/// as `current_count + committed.len()` (each committed hunk removed one from
/// the current diff).
fn map_planned_hunks(
    planned: &[usize],
    committed: &HashSet<usize>,
    current_count: usize,
) -> HunkMapping {
    // Empty `planned` means "the whole file"; rebuild the original hunk count
    // from what the current diff still shows plus what's already gone.
    let wanted: Vec<usize> = if planned.is_empty() {
        (1..=(current_count + committed.len())).collect()
    } else {
        planned.to_vec()
    };

    let mut original = Vec::with_capacity(wanted.len());
    let mut current = Vec::with_capacity(wanted.len());
    for h in wanted {
        if committed.contains(&h) {
            continue;
        }
        // Every committed original hunk before `h` shifts it down by one slot
        // in the current (shrunk) diff.
        let shift = committed.iter().filter(|&&c| c < h).count();
        original.push(h);
        current.push(h - shift);
    }
    HunkMapping { original, current }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_planned_hunks_splits_original_and_current_indices() {
        // Hunks 1-2 already committed → plan's 3-7 survive as original 3-7 and
        // map onto the current diff's 1-5. The two views differ precisely when
        // something is committed; conflating them is the bug this struct exists
        // to prevent, so assert both.
        let committed: HashSet<usize> = [1usize, 2].into_iter().collect();
        let m = map_planned_hunks(&[3, 4, 5, 6, 7], &committed, 5);
        assert_eq!(m.original, vec![3, 4, 5, 6, 7]);
        assert_eq!(m.current, vec![1, 2, 3, 4, 5]);

        // Empty planned hunks = every hunk of the file; the original count is
        // reconstructed as current + already-committed.
        let m = map_planned_hunks(&[], &committed, 5);
        assert_eq!(m.original, vec![3, 4, 5, 6, 7]);
        assert_eq!(m.current, vec![1, 2, 3, 4, 5]);

        // Nothing committed yet → both views pass through unchanged.
        let m = map_planned_hunks(&[1, 2], &HashSet::new(), 2);
        assert_eq!(m.original, vec![1, 2]);
        assert_eq!(m.current, vec![1, 2]);

        // Interleaved committed hunks are dropped and current positions
        // recomputed.
        let committed: HashSet<usize> = [1usize, 3, 5].into_iter().collect();
        let m = map_planned_hunks(&[2, 4, 6], &committed, 3);
        assert_eq!(m.original, vec![2, 4, 6]);
        assert_eq!(m.current, vec![1, 2, 3]);

        // The regression: three hunks of one file, split across three batches
        // with NO hook. After batch 1 commits original hunk 1, batch 2's hunk
        // 2 must map to current position 1; after batch 2, hunk 3 must map to
        // current position 1 of a 1-hunk diff. Storing the remapped position
        // (1) instead of the original (2 / 3) here used to corrupt `committed`
        // and make batch 3 address a non-existent hunk 2.
        let mut committed: HashSet<usize> = [1usize].into_iter().collect();
        let m = map_planned_hunks(&[2], &committed, 2);
        assert_eq!(m.original, vec![2]);
        assert_eq!(m.current, vec![1]);
        committed.extend(m.original);
        let m = map_planned_hunks(&[3], &committed, 1);
        assert_eq!(m.original, vec![3]);
        assert_eq!(m.current, vec![1]);
    }
}
