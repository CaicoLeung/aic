//! Deterministic block grouping — the v1 heuristic that turns a workdir diff
//! (one or more files, each with hunks) into atomic commit blocks **without an
//! LLM**.
//!
//! This is the block-level diff engine called out in the charter: parse diffs,
//! split into coherent atomic blocks across hunks/files, group related changes,
//! detect logical commit boundaries. The LLM planner ([`crate::generator`])
//! remains the production splitter; this module is the deterministic
//! foundation underneath it — a predictable, testable baseline that can later
//! serve as a fallback (no API key) or a pre-grouper. v1 delivers the engine,
//! its heuristics, and tests; the live Run falls back to it when the LLM
//! planner fails (error, timeout, or no key).
//!
//! # The two v1 heuristics
//!
//! Grouping runs in two stages, each a small, independently testable rule:
//!
//! 1. **Adjacency** (within one file). Consecutive hunks whose regions sit
//!    close together — fewer than [`GroupingConfig::adjacency_gap`] unchanged
//!    lines between them — are one edit git happened to split, and are merged
//!    into one block. [`GroupingConfig::join_same_context`] additionally
//!    merges hunks that share git's attached context header (a function or
//!    section name), the strongest "same edit" signal there is.
//! 2. **Same-scope** (across files). After within-file grouping, blocks from
//!    *different* files that share a directory scope are merged into one block.
//!    Scope is the file's parent directory (e.g. `src/auth` for
//!    `src/auth/login.rs`). Gated by [`GroupingConfig::cross_file_same_scope`].
//!
//! [`group`] orchestrates the two stages; [`group_adjacent`] and
//! [`group_same_scope`] expose them individually.
//!
//! # Where v1 is conservative
//!
//! v1 prefers correctness over clever splitting. The conservatism is explicit:
//!
//! - **Cross-file grouping is off by default.** A shared directory is a weak
//!   proxy for "same logical change" — two files in `src/auth/` may carry
//!   unrelated work — so v1 ships it implemented and tested but *not* on. A
//!   Run that turns it on opts into the risk.
//! - **Adjacency is tight.** git already folds edits within its context window
//!   into a single hunk, so the engine only ever sees separate hunks for edits
//!   ≥7 source lines apart. `adjacency_gap = 3` extends git's own grouping by
//!   only a few lines: hunks with a handful of unchanged lines between them
//!   merge; anything farther apart stays split. Set it to `0` to merge only
//!   touching/overlapping hunks.
//! - **Root-scope files never auto-merge.** `Cargo.toml`, `README.md`, and
//!   other top-level files carry unrelated concerns (a dep bump vs a doc
//!   tweak); same-scope leaves every root file in its own block.
//! - **A file split into several blocks by adjacency is left alone.** If
//!   adjacency decided one file's changes belong in two commits, same-scope
//!   does not pull them back together via a sibling file — it only groups
//!   files whose own change is already a single coherent block.
//! - **No reordering.** Blocks preserve file order, then hunk order, so the
//!   output is a stable, reviewable sequence.
//!
//! The output is always a valid partition: every hunk of every file lands in
//! exactly one block, so [`blocks_to_plan`] feeds straight into
//! [`crate::generator::validate_batch_plan`] and [`crate::staging::Staging`].

use std::collections::{HashMap, HashSet};
use std::path::Path;

use crate::diff;
use crate::generator::{BatchChange, BatchPlanBatch, BatchPlanOutput};

/// Scope key for root-level files (no parent directory). Same-scope never
/// merges blocks whose scope is this — see "Where v1 is conservative".
const ROOT_SCOPE: &str = ".";

/// One hunk of one file, as the grouping engine sees it.
///
/// `index` is the 1-based position within the file — the same numbering the
/// LLM plan and [`crate::staging::Staging`] stage against — so a grouped
/// block's hunk indices are staged without remapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileHunk {
    pub index: usize,
    /// Git's attached context header (a function/section name when it knows
    /// one), or `""`. The same-context adjacency signal.
    pub context: String,
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
}

/// One file's hunks, as the grouping engine sees them. Built from a raw diff
/// via [`GroupFile::from_diff`]; the engine never touches git.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupFile {
    pub path: String,
    pub hunks: Vec<FileHunk>,
}

impl GroupFile {
    /// Parse a single file's unified diff into the engine's hunk view.
    /// Reuses [`diff::parse_diff_blocks`] so the line ranges stay consistent
    /// with the numbered view sent to the model.
    pub fn from_diff(path: &str, raw_diff: &str) -> GroupFile {
        let hunks = diff::parse_diff_blocks(raw_diff)
            .into_iter()
            .enumerate()
            .map(|(i, b)| FileHunk {
                index: i + 1,
                context: b.header,
                old_start: b.old_start,
                old_count: b.old_count,
                new_start: b.new_start,
                new_count: b.new_count,
            })
            .collect();
        GroupFile {
            path: path.to_string(),
            hunks,
        }
    }
}

/// Why the engine joined a block's hunks. Carried on [`Block::heuristic`] and
/// surfaced as the batch `reason`, so a grouped plan is self-documenting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockHeuristic {
    /// A lone hunk with nothing to group against.
    Single,
    /// Adjacent hunks within one file (small gap).
    Adjacency,
    /// Hunks sharing git's context header (same function/section).
    SameContext,
    /// Blocks from different files sharing a directory scope.
    SameScope,
}

impl BlockHeuristic {
    /// Human-readable reason text for a batch `reason` field.
    fn reason(self) -> &'static str {
        match self {
            BlockHeuristic::Single => "single hunk",
            BlockHeuristic::Adjacency => "adjacent hunks (within file)",
            BlockHeuristic::SameContext => "same function/section context",
            BlockHeuristic::SameScope => "same directory scope (cross-file)",
        }
    }
}

/// One atomic commit block: a coherent group of hunks (possibly across files)
/// the engine believes belong in one commit, plus the heuristic that joined
/// them.
#[derive(Debug, Clone)]
pub struct Block {
    /// One [`BatchChange`] per file that contributes hunks to this block, in
    /// first-seen file order; each entry's `hunks` are 1-based and sorted.
    pub changes: Vec<BatchChange>,
    pub heuristic: BlockHeuristic,
}

impl Block {
    /// Render this block as one batch of an LLM-style plan. The hunk partition
    /// is identical to the engine's; only the `reason` is added, so the result
    /// drops straight into [`crate::generator::validate_batch_plan`] and
    /// [`crate::staging::Staging::stage_batch`].
    pub fn to_batch(&self) -> BatchPlanBatch {
        BatchPlanBatch {
            changes: self.changes.clone(),
            reason: Some(self.heuristic.reason().to_string()),
        }
    }
}

/// Knobs for v1 grouping. Defaults are conservative — see the module docs for
/// exactly where v1 refuses to be clever.
#[derive(Debug, Clone)]
pub struct GroupingConfig {
    /// Max unchanged lines between two consecutive same-file hunks for them to
    /// count as one edit (adjacency). `0` merges only touching/overlapping
    /// hunks; larger = more aggressive. git already folds edits within its
    /// context window into one hunk, so the engine only sees separate hunks
    /// for edits ≥7 source lines apart.
    pub adjacency_gap: u32,
    /// Also merge consecutive same-file hunks that share a non-empty git
    /// context header (same function/section), regardless of gap. The
    /// strongest within-file "same edit" signal.
    pub join_same_context: bool,
    /// After within-file grouping, merge per-file blocks from *different*
    /// files that share a directory scope. Off by default — a directory is a
    /// weak proxy for "same logical change".
    pub cross_file_same_scope: bool,
}

impl Default for GroupingConfig {
    fn default() -> Self {
        Self {
            adjacency_gap: 3,
            join_same_context: true,
            cross_file_same_scope: false,
        }
    }
}

/// Group every hunk of every file into atomic blocks under `cfg`.
///
/// Always returns a partition: each hunk lands in exactly one block. Blocks
/// are in first-seen file order; within a block, changes are in file order and
/// each file's hunks are sorted.
pub fn group(files: &[GroupFile], cfg: &GroupingConfig) -> Vec<Block> {
    let mut blocks: Vec<Block> = group_adjacent(files, cfg);
    if cfg.cross_file_same_scope {
        blocks = group_same_scope(blocks);
    }
    blocks
}

/// Stage 1 — within-file adjacency (and same-context). Produces one block per
/// maximal run of joinable consecutive hunks, per file. Exposed so callers can
/// apply only the safe within-file heuristic.
pub fn group_adjacent(files: &[GroupFile], cfg: &GroupingConfig) -> Vec<Block> {
    let mut blocks: Vec<Block> = Vec::new();
    for file in files {
        if file.hunks.is_empty() {
            continue;
        }
        let mut run: Vec<usize> = vec![file.hunks[0].index];
        let mut run_used_context = false;
        for win in file.hunks.windows(2) {
            let (prev, next) = (&win[0], &win[1]);
            let gap = unchanged_gap(prev, next);
            let same_context =
                cfg.join_same_context && !prev.context.is_empty() && prev.context == next.context;
            if gap <= cfg.adjacency_gap || same_context {
                run.push(next.index);
                if same_context {
                    run_used_context = true;
                }
            } else {
                blocks.push(single_file_block(
                    &file.path,
                    std::mem::take(&mut run),
                    run_used_context,
                ));
                run = vec![next.index];
                run_used_context = false;
            }
        }
        blocks.push(single_file_block(&file.path, run, run_used_context));
    }
    blocks
}

/// Stage 2 — cross-file same-scope. Merges blocks from *different* files that
/// share a non-root parent directory, leaving multi-block files and root files
/// untouched. Exposed so callers can apply only the cross-file heuristic to an
/// already adjacency-grouped list.
pub fn group_same_scope(blocks: Vec<Block>) -> Vec<Block> {
    // A file is eligible to join a cross-file group only when ALL of its
    // changes already sit in one block (adjacency did not split it) and that
    // block is single-file. Compute the set of split files up front so the
    // borrow is free before we consume `blocks`.
    let mut split_files: HashSet<String> = HashSet::new();
    let mut seen: HashMap<String, u32> = HashMap::new();
    for b in &blocks {
        for c in &b.changes {
            let n = seen.entry(c.file.clone()).or_default();
            *n += 1;
            if *n > 1 {
                split_files.insert(c.file.clone());
            }
        }
    }

    let mut out: Vec<Block> = Vec::new();
    let mut scope_first_idx: HashMap<String, usize> = HashMap::new();
    for b in blocks {
        // Eligible: a single-file block whose only file is unsplit and lives
        // under a real (non-root) directory.
        let eligible = b.changes.len() == 1
            && !split_files.contains(&b.changes[0].file)
            && scope_of(&b.changes[0].file) != ROOT_SCOPE;
        if !eligible {
            out.push(b);
            continue;
        }
        let scope = scope_of(&b.changes[0].file);
        match scope_first_idx.get(&scope) {
            Some(&idx) => {
                // A second file joins this scope → it is now a cross-file group.
                out[idx].changes.push(b.changes.into_iter().next().unwrap());
                out[idx].heuristic = BlockHeuristic::SameScope;
            }
            None => {
                scope_first_idx.insert(scope, out.len());
                out.push(b);
            }
        }
    }
    // A block that ended up with several changes is, by construction, a
    // cross-file merge — tag it so its reason is honest even though the first
    // member kept its original heuristic.
    for b in &mut out {
        if b.changes.len() > 1 {
            b.heuristic = BlockHeuristic::SameScope;
        }
    }
    out
}

/// Turn a grouped block list into an LLM-style plan. The partition is
/// unchanged; each block becomes one batch carrying its heuristic as the
/// `reason`. Feeds straight into [`crate::generator::validate_batch_plan`].
pub fn blocks_to_plan(blocks: &[Block]) -> BatchPlanOutput {
    BatchPlanOutput {
        batches: blocks.iter().map(Block::to_batch).collect(),
    }
}

/// Unchanged old-side lines strictly between two consecutive hunks' regions.
/// `next` must follow `prev` in file order. Overlap (regions touch or cross)
/// yields `0`.
fn unchanged_gap(prev: &FileHunk, next: &FileHunk) -> u32 {
    // prev covers old lines [old_start, old_start + old_count - 1]; one past
    // its last line is old_start + old_count. next starts at next.old_start.
    let prev_end_plus_one = prev.old_start.saturating_add(prev.old_count);
    next.old_start.saturating_sub(prev_end_plus_one)
}

/// Parent-directory scope key for a path, or [`ROOT_SCOPE`] for top-level
/// files. `src/auth/login.rs` → `src/auth`; `Cargo.toml` → `.`.
fn scope_of(path: &str) -> String {
    match Path::new(path).parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_string_lossy().into_owned(),
        _ => ROOT_SCOPE.to_string(),
    }
}

/// Build a single-file block from a run of hunk indices, tagging it with the
/// heuristic that actually joined the run.
fn single_file_block(file: &str, hunks: Vec<usize>, used_context: bool) -> Block {
    let heuristic = if hunks.len() == 1 {
        BlockHeuristic::Single
    } else if used_context {
        BlockHeuristic::SameContext
    } else {
        BlockHeuristic::Adjacency
    };
    Block {
        changes: vec![BatchChange {
            file: file.to_string(),
            hunks,
        }],
        heuristic,
    }
}

// Keep `DiffBlock` construction ergonomic in tests below without reaching into
// private fields each time.
#[cfg(test)]
fn db(
    header: &str,
    old_start: u32,
    old_count: u32,
    new_start: u32,
    new_count: u32,
) -> crate::diff::DiffBlock {
    crate::diff::DiffBlock {
        header: header.to_string(),
        old_start,
        old_count,
        new_start,
        new_count,
        lines: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::validate_batch_plan;

    fn hunk(index: usize, context: &str, old_start: u32, old_count: u32) -> FileHunk {
        FileHunk {
            index,
            context: context.to_string(),
            old_start,
            old_count,
            new_start: old_start,
            new_count: old_count,
        }
    }

    fn file(path: &str, hunks: &[FileHunk]) -> GroupFile {
        GroupFile {
            path: path.to_string(),
            hunks: hunks.to_vec(),
        }
    }

    fn default_cfg() -> GroupingConfig {
        GroupingConfig::default()
    }

    /// Single hunk, single file → one `Single` block carrying hunk 1.
    #[test]
    fn single_hunk_is_one_single_block() {
        let files = [file("a.rs", &[hunk(1, "", 1, 3)])];
        let blocks = group(&files, &default_cfg());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].heuristic, BlockHeuristic::Single);
        assert_eq!(blocks[0].changes[0].file, "a.rs");
        assert_eq!(blocks[0].changes[0].hunks, vec![1]);
    }

    /// Two hunks with a small gap (1 unchanged line) merge into one
    /// `Adjacency` block: prev ends at line 5, next starts at line 7 → gap 1.
    #[test]
    fn adjacent_hunks_merge_into_one_block() {
        let files = [file(
            "a.rs",
            &[hunk(1, "", 1, 4), hunk(2, "", 7, 3)], // gap = 7 - (1+4) = 1
        )];
        let blocks = group(&files, &default_cfg());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].heuristic, BlockHeuristic::Adjacency);
        assert_eq!(blocks[0].changes[0].hunks, vec![1, 2]);
    }

    /// Two hunks far apart (gap 14) stay in two `Single` blocks.
    #[test]
    fn distant_hunks_stay_separate() {
        let files = [file(
            "a.rs",
            &[hunk(1, "", 1, 5), hunk(2, "", 20, 3)], // gap = 20 - 6 = 14
        )];
        let blocks = group(&files, &default_cfg());
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].heuristic, BlockHeuristic::Single);
        assert_eq!(blocks[1].heuristic, BlockHeuristic::Single);
    }

    /// The boundary is inclusive: a gap equal to `adjacency_gap` merges; one
    /// more line does not.
    #[test]
    fn adjacency_gap_boundary_is_inclusive() {
        let cfg = GroupingConfig {
            adjacency_gap: 3,
            join_same_context: false,
            cross_file_same_scope: false,
        };
        // gap exactly 3 → merge.
        let files = [file(
            "a.rs",
            &[hunk(1, "", 1, 2), hunk(2, "", 6, 2)], // gap = 6 - 3 = 3
        )];
        assert_eq!(
            group(&files, &cfg).len(),
            1,
            "gap==adjacency_gap must merge"
        );
        // gap 4 → separate.
        let files = [file(
            "a.rs",
            &[hunk(1, "", 1, 2), hunk(2, "", 7, 2)], // gap = 7 - 3 = 4
        )];
        assert_eq!(group(&files, &cfg).len(), 2, "gap>adjacency_gap must split");
    }

    /// Two hunks with a huge gap but the same non-empty context header merge
    /// into one `SameContext` block — the function-name signal overrides
    /// distance.
    #[test]
    fn same_context_header_merges_across_large_gap() {
        let files = [file(
            "a.rs",
            &[hunk(1, "fn login", 1, 3), hunk(2, "fn login", 500, 3)],
        )];
        let blocks = group(&files, &default_cfg());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].heuristic, BlockHeuristic::SameContext);
        assert_eq!(blocks[0].changes[0].hunks, vec![1, 2]);
    }

    /// With `join_same_context` off, the same-context hunks stay split by gap.
    #[test]
    fn same_context_can_be_disabled() {
        let cfg = GroupingConfig {
            adjacency_gap: 3,
            join_same_context: false,
            cross_file_same_scope: false,
        };
        let files = [file(
            "a.rs",
            &[hunk(1, "fn login", 1, 3), hunk(2, "fn login", 500, 3)],
        )];
        let blocks = group(&files, &cfg);
        assert_eq!(blocks.len(), 2, "disabled context must not override gap");
    }

    /// An empty context header is not a real scope and never triggers the
    /// same-context merge.
    #[test]
    fn empty_context_does_not_merge() {
        let files = [file("a.rs", &[hunk(1, "", 1, 3), hunk(2, "", 500, 3)])];
        let blocks = group(&files, &default_cfg());
        assert_eq!(blocks.len(), 2);
    }

    /// A three-hunk file: h1–h2 adjacent, h3 far → one `Adjacency` block
    /// `[1,2]` plus one `Single` block `[3]`.
    #[test]
    fn mixed_run_splits_into_adjacency_and_single() {
        let files = [file(
            "a.rs",
            &[
                hunk(1, "", 1, 3),
                hunk(2, "", 6, 3),  // gap 2 from h1 → adjacent
                hunk(3, "", 90, 3), // gap large → split
            ],
        )];
        let blocks = group(&files, &default_cfg());
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].heuristic, BlockHeuristic::Adjacency);
        assert_eq!(blocks[0].changes[0].hunks, vec![1, 2]);
        assert_eq!(blocks[1].heuristic, BlockHeuristic::Single);
        assert_eq!(blocks[1].changes[0].hunks, vec![3]);
    }

    /// Two files in different directories, cross-file off → two blocks.
    #[test]
    fn different_scopes_stay_separate_with_cross_file_off() {
        let files = [
            file("src/auth/a.rs", &[hunk(1, "", 1, 1)]),
            file("src/core/b.rs", &[hunk(1, "", 1, 1)]),
        ];
        let blocks = group(&files, &default_cfg());
        assert_eq!(blocks.len(), 2);
    }

    /// Cross-file on: two single-block files in the same directory merge into
    /// one `SameScope` block with two changes.
    #[test]
    fn cross_file_same_scope_merges() {
        let cfg = GroupingConfig {
            cross_file_same_scope: true,
            ..default_cfg()
        };
        let files = [
            file("src/auth/login.rs", &[hunk(1, "", 1, 1)]),
            file("src/auth/session.rs", &[hunk(1, "", 1, 1)]),
        ];
        let blocks = group(&files, &cfg);
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].heuristic, BlockHeuristic::SameScope);
        assert_eq!(blocks[0].changes.len(), 2);
        assert_eq!(blocks[0].changes[0].file, "src/auth/login.rs");
        assert_eq!(blocks[0].changes[1].file, "src/auth/session.rs");
    }

    /// Root-level files (`Cargo.toml`, `README.md`) never auto-merge even with
    /// cross-file on — top-level files carry unrelated concerns.
    #[test]
    fn root_scope_files_never_merge() {
        let cfg = GroupingConfig {
            cross_file_same_scope: true,
            ..default_cfg()
        };
        let files = [
            file("Cargo.toml", &[hunk(1, "", 1, 1)]),
            file("README.md", &[hunk(1, "", 1, 1)]),
        ];
        let blocks = group(&files, &cfg);
        assert_eq!(blocks.len(), 2, "root files must stay separate");
    }

    /// A file split into two blocks by adjacency is left alone by same-scope:
    /// its blocks stay single, and a sibling single-block file does not pull
    /// them back together.
    #[test]
    fn same_scope_leaves_split_files_alone() {
        let cfg = GroupingConfig {
            adjacency_gap: 3,
            join_same_context: false,
            cross_file_same_scope: true,
        };
        let files = [
            // a.rs: two distant hunks → two blocks (split file).
            file("src/auth/a.rs", &[hunk(1, "", 1, 2), hunk(2, "", 80, 2)]),
            // b.rs: one hunk, same dir, single block → eligible but alone.
            file("src/auth/b.rs", &[hunk(1, "", 1, 2)]),
        ];
        let blocks = group(&files, &cfg);
        // a.rs → 2 blocks, b.rs → 1 block; b.rs has no eligible sibling, so no
        // cross-file merge occurs.
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].changes[0].file, "src/auth/a.rs");
        assert_eq!(blocks[1].changes[0].file, "src/auth/a.rs");
        assert_eq!(blocks[2].changes[0].file, "src/auth/b.rs");
    }

    /// The output is always a valid partition: every hunk of every file lands
    /// in exactly one batch, so `validate_batch_plan` accepts it.
    #[test]
    fn grouped_output_is_a_valid_partition() {
        let files = [
            file(
                "src/auth/a.rs",
                &[hunk(1, "", 1, 2), hunk(2, "", 6, 2), hunk(3, "", 80, 2)],
            ),
            file("src/core/b.rs", &[hunk(1, "", 1, 2)]),
            file("Cargo.toml", &[hunk(1, "", 1, 2)]),
        ];
        let blocks = group(&files, &default_cfg());
        let plan = blocks_to_plan(&blocks);
        let counts = vec![
            ("src/auth/a.rs".to_string(), 3),
            ("src/core/b.rs".to_string(), 1),
            ("Cargo.toml".to_string(), 1),
        ];
        validate_batch_plan(&plan, &counts).expect("grouped output must partition every hunk");
        assert!(!plan.batches.is_empty());
        // Each batch carries its heuristic as the reason.
        for b in &plan.batches {
            assert!(b.reason.is_some(), "every batch must carry a reason");
        }
    }

    /// Hunk indices are sorted within a change and blocks never repeat a hunk.
    #[test]
    fn hunk_indices_are_sorted_and_disjoint() {
        let files = [file(
            "a.rs",
            &[
                hunk(1, "", 1, 2),
                hunk(2, "", 4, 2),
                hunk(3, "", 6, 2),
                hunk(4, "", 90, 2),
            ],
        )];
        let blocks = group(&files, &default_cfg());
        let mut seen = Vec::new();
        for b in &blocks {
            for c in &b.changes {
                assert!(
                    c.hunks.windows(2).all(|w| w[0] < w[1]),
                    "hunks must be sorted: {:?}",
                    c.hunks
                );
                seen.extend_from_slice(&c.hunks);
            }
        }
        seen.sort_unstable();
        assert_eq!(seen, vec![1, 2, 3, 4], "every hunk appears exactly once");
    }

    /// No files → no blocks (no panic).
    #[test]
    fn empty_input_yields_no_blocks() {
        let blocks: Vec<Block> = group(&[], &default_cfg());
        assert!(blocks.is_empty());
    }

    /// `GroupFile::from_diff` parses old_count and context so adjacency has the
    /// ranges it needs.
    #[test]
    fn from_diff_captures_ranges_and_context() {
        let raw = "\
diff --git a/a.rs b/a.rs\n\
--- a/a.rs\n\
+++ b/a.rs\n\
@@ -1,4 +1,4 @@ fn login\n\
 ctx\n\
-old\n\
+new\n\
 ctx\n\
@@ -20,4 +20,4 @@ fn login\n\
 ctx\n\
-old2\n\
+new2\n\
 ctx\n";
        let gf = GroupFile::from_diff("a.rs", raw);
        assert_eq!(gf.path, "a.rs");
        assert_eq!(gf.hunks.len(), 2);
        assert_eq!(gf.hunks[0].index, 1);
        assert_eq!(gf.hunks[0].context, "fn login");
        assert_eq!((gf.hunks[0].old_start, gf.hunks[0].old_count), (1, 4));
        assert_eq!(gf.hunks[1].index, 2);
        assert_eq!((gf.hunks[1].old_start, gf.hunks[1].old_count), (20, 4));
        // Same context header → one SameContext block despite the gap.
        let blocks = group(&[gf], &default_cfg());
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].heuristic, BlockHeuristic::SameContext);
    }

    /// `unchanged_gap` is saturating: overlapping hunks yield 0, not underflow.
    #[test]
    fn unchanged_gap_is_saturating_on_overlap() {
        // prev covers [1,5] (start1,count5); next starts at 3 → overlap.
        let prev = hunk(1, "", 1, 5);
        let next = hunk(2, "", 3, 2);
        assert_eq!(unchanged_gap(&prev, &next), 0);
        // Touching: next starts right after prev's region.
        let next_touch = hunk(2, "", 6, 2); // prev_end_plus_one = 6 → gap 0
        assert_eq!(unchanged_gap(&prev, &next_touch), 0);
        // One line between.
        let next_one = hunk(2, "", 7, 2);
        assert_eq!(unchanged_gap(&prev, &next_one), 1);
    }

    /// `scope_of` maps paths to their parent directory and root files to `.`.
    #[test]
    fn scope_of_extracts_parent_directory() {
        assert_eq!(scope_of("src/auth/login.rs"), "src/auth");
        assert_eq!(scope_of("a/b/c.rs"), "a/b");
        assert_eq!(scope_of("Cargo.toml"), ROOT_SCOPE);
        assert_eq!(scope_of("top.rs"), ROOT_SCOPE);
    }

    // `db` is exercised here to prove DiffBlock now carries old_count through
    // parse_diff_blocks end-to-end (the field the engine reads).
    #[test]
    fn diff_block_carries_old_count() {
        let blocks = diff::parse_diff_blocks("@@ -1,4 +1,4 @@ fn x\n ctx\n-old\n+new\n ctx\n");
        assert_eq!(blocks.len(), 1);
        assert_eq!((blocks[0].old_start, blocks[0].old_count), (1, 4));
        // Also via the test helper, which sets every field.
        let b = db("fn x", 1, 4, 1, 4);
        assert_eq!(b.old_count, 4);
    }
}
