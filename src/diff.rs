//! Pure text parsing of unified diffs — no git, no libgit2, testable without
//! a repository.
//!
//! Two views over the same raw diff text:
//!
//! - [`FilePatch`]: one file's diff split into a stable header and raw `@@`
//!   hunks, with intent-bearing methods ([`FilePatch::hunk_count`],
//!   [`FilePatch::slice`]) so consumers never touch the raw slices — the
//!   `git apply --cached` replay contract lives behind `slice`, not in the
//!   type's shape.
//! - [`DiffBlock`] / [`parse_diff_blocks`]: the numbered, context-labelled
//!   block view fed to the LLM via [`format_diff_scoped`].

use anyhow::{Context, Result};

#[derive(Debug, serde::Serialize)]
pub struct DiffBlock {
    pub header: String,
    pub old_start: u32,
    pub new_start: u32,
    pub new_count: u32,
    pub lines: Vec<String>,
}

fn parse_hunk_header(header: &str) -> Option<(u32, u32, u32, u32, Option<&str>)> {
    // Parse: @@ -old_start,old_count +new_start,new_count @@ optional_context
    let rest = header.strip_prefix("@@ ")?;
    let closing = rest.find("@@")?;
    let range_part = &rest[..closing];
    let context = rest
        .get(closing + 2..)
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());

    let mut parts = range_part.split(' ');
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;

    let (old_start, old_count) = parse_range(old)?;
    let (new_start, new_count) = parse_range(new)?;

    Some((old_start, old_count, new_start, new_count, context))
}

fn parse_range(range: &str) -> Option<(u32, u32)> {
    match range.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((range.parse().ok()?, 1)),
    }
}

pub fn parse_diff_blocks(diff: &str) -> Vec<DiffBlock> {
    let mut blocks: Vec<DiffBlock> = Vec::new();

    for line in diff.lines() {
        if let Some((old_start, _old_count, new_start, new_count, context)) =
            parse_hunk_header(line)
        {
            let header = context.unwrap_or("(top-level)").to_string();
            blocks.push(DiffBlock {
                header,
                old_start,
                new_start,
                new_count,
                lines: Vec::new(),
            });
        } else if let Some(block) = blocks.last_mut() {
            block.lines.push(line.to_string());
        }
    }

    blocks
}

/// A single-file unified diff split into its stable header and raw `@@` hunks.
///
/// Preserves enough of the original patch text to reconstruct a partial patch
/// (a subset of hunks) for `git apply --cached` — the non-interactive analogue
/// of `git add -p`. Hunk order matches the diff, so a 1-based index into the
/// hunks is a stable handle the LLM can reference.
#[derive(Debug, Clone)]
pub struct FilePatch {
    /// Lines before the first `@@` hunk header: `diff --git`, `index`,
    /// `---`, `+++` (and any leading noise).
    header: String,
    /// Each hunk's full text, starting at its `@@ … @@` line and including
    /// every body line, in file order.
    hunks: Vec<String>,
}

impl FilePatch {
    /// How many hunks the diff has — the numbering the LLM's plan refers to.
    pub fn hunk_count(&self) -> usize {
        self.hunks.len()
    }

    /// Rebuild a partial patch from selected hunks: the stable header plus
    /// each chosen hunk verbatim, in file order. `hunk_indices` are 1-based
    /// positions in this diff's hunk order — the numbering the plan (and the
    /// model) saw. Out-of-range indices are rejected rather than silently
    /// dropped, so a hallucinated plan surfaces instead of staging a partial
    /// commit.
    pub fn slice(&self, hunk_indices: &[usize]) -> Result<String> {
        let mut body = self.header.clone();
        for &idx in hunk_indices {
            let i = idx
                .checked_sub(1)
                .context("hunk indices are 1-based and must be >= 1")?;
            let hunk = self.hunks.get(i).with_context(|| {
                format!(
                    "hunk {idx} is out of range (file has {} hunk(s))",
                    self.hunks.len()
                )
            })?;
            body.push_str(hunk);
        }
        Ok(body)
    }
}

/// Parse a single-file unified diff into its header and raw hunks.
pub fn parse_file_patch(raw: &str) -> FilePatch {
    let mut header = String::new();
    let mut hunks: Vec<String> = Vec::new();
    let mut current: Option<String> = None;

    for line in raw.lines() {
        if line.starts_with("@@") {
            if let Some(prev) = current.take() {
                hunks.push(prev);
            }
            current = Some(String::new());
        }
        match &mut current {
            Some(hunk) => {
                hunk.push_str(line);
                hunk.push('\n');
            }
            None => {
                header.push_str(line);
                header.push('\n');
            }
        }
    }
    if let Some(prev) = current {
        hunks.push(prev);
    }

    FilePatch { header, hunks }
}

/// The LLM's numbered view of one file's diff: each hunk labelled with its
/// 1-based position, its context header (function name when git knows one),
/// and the new-side line range.
pub fn format_diff_scoped(diff: &str, file_path: &str) -> String {
    let blocks = parse_diff_blocks(diff);
    if blocks.is_empty() {
        return String::new();
    }

    let mut out = format!("--- {file_path} ---\n");
    let mut i = 0;
    while i < blocks.len() {
        let block = &blocks[i];
        let end = new_end(block.new_start, block.new_count);
        out.push_str(&format!(
            "[{}] hunk {}, lines {}-{}\n",
            block.header,
            i + 1,
            block.new_start,
            end
        ));
        for line in &block.lines {
            out.push_str(line);
            out.push('\n');
        }
        out.push('\n');
        i += 1;
    }
    out
}

fn new_end(start: u32, count: u32) -> u32 {
    if count == 0 { start } else { start + count - 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_file_patch_splits_header_and_hunks() {
        let raw = "diff --git a/f.rs b/f.rs\n\
index 1..2 100644\n\
--- a/f.rs\n\
+++ b/f.rs\n\
@@ -1,3 +1,3 @@\n\
 ctx\n\
-old\n\
+new\n\
 ctx\n\
@@ -10,3 +10,3 @@ fn b\n\
 ctx\n\
-old2\n\
+new2\n\
 ctx\n";
        let patch = parse_file_patch(raw);
        assert!(patch.header.starts_with("diff --git"));
        assert!(
            !patch.header.contains("@@"),
            "header must not include hunks"
        );
        assert_eq!(patch.hunk_count(), 2);
        assert!(patch.hunks[0].starts_with("@@ -1,3 +1,3 @@"));
        assert!(patch.hunks[1].starts_with("@@ -10,3 +10,3 @@ fn b"));
    }

    /// The replay contract behind `slice`: header plus selected hunks, in
    /// order — the exact text `git apply --cached` consumes.
    #[test]
    fn slice_rebuilds_header_plus_selected_hunks() {
        let raw = "diff --git a/f.rs b/f.rs\n\
index 1..2 100644\n\
--- a/f.rs\n\
+++ b/f.rs\n\
@@ -1,3 +1,3 @@\n\
 ctx\n\
-old\n\
+new\n\
 ctx\n\
@@ -10,3 +10,3 @@ fn b\n\
 ctx\n\
-old2\n\
+new2\n\
 ctx\n";
        let patch = parse_file_patch(raw);
        let body = patch.slice(&[2]).unwrap();
        assert!(body.starts_with("diff --git"), "header must lead");
        assert!(!body.contains("-old\n"), "hunk 1 must be absent");
        assert!(body.contains("old2"), "hunk 2 must be present");

        let both = patch.slice(&[1, 2]).unwrap();
        assert!(both.contains("-old\n") && both.contains("old2"));
    }

    /// 1-based numbering is the plan's contract — 0 and out-of-range indices
    /// must be rejected, not silently dropped.
    #[test]
    fn slice_rejects_zero_and_out_of_range_indices() {
        let patch = parse_file_patch(
            "diff --git a/f.rs b/f.rs\n\
index 1..2 100644\n\
--- a/f.rs\n\
+++ b/f.rs\n\
@@ -1,3 +1,3 @@\n\
 ctx\n\
-old\n\
+new\n\
 ctx\n",
        );
        assert_eq!(patch.hunk_count(), 1);
        let err = patch.slice(&[0]).unwrap_err().to_string();
        assert!(err.contains("1-based"), "zero index error: {err}");
        let err = patch.slice(&[2]).unwrap_err().to_string();
        assert!(err.contains("out of range"), "OOB error: {err}");
    }
}
