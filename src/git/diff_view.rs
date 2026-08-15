//! Diff computation and formatting: staged/worktree diffs, per-file commit
//! stats, and the free-function formatting layer over libgit2's
//! `Diff::print` callbacks.

use super::*;
use git2::{DiffFormat, DiffLineType};
use std::collections::HashMap;

impl Git {
    pub fn diff(&self, path: Option<&str>) -> anyhow::Result<String> {
        let repo = &self.repo;
        let head_tree = match repo.head() {
            Ok(r) => Some(r.peel_to_tree().context("failed to peel HEAD to tree")?),
            Err(_) => None,
        };

        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true);
        if let Some(p) = path {
            opts.pathspec(p);
        }

        let index = self.index()?;
        let diff = repo
            .diff_tree_to_index(head_tree.as_ref(), Some(&index), Some(&mut opts))
            .context("failed to compute diff")?;

        format_diff(&diff)
    }

    /// Working-tree diff for one file (`path`) or every changed path, including
    /// untracked files with their full content.
    ///
    /// The result is captured once per Run and consumed two ways: a numbered
    /// view goes to the model, and the raw hunks are replayed per-batch by
    /// [`Git::stage_hunks`] via `git apply --cached`. For that replay to land,
    /// every path [`Git::status`] reports — including a file *inside* an
    /// untracked directory — must resolve to a real patch here, or staging
    /// silently no-ops and the batch aborts.
    ///
    /// That contract is why `recurse_untracked_dirs` is mandatory below.
    /// `status()` expands untracked directories file-by-file, but
    /// `diff_index_to_workdir` does not unless asked: it collapses a whole
    /// untracked directory into one directory-level delta (path `src/foo/`),
    /// so a per-file query like `src/foo/mod.rs` matches no delta and returns
    /// an empty string. `stage_hunks` then feeds `git apply --cached` an empty
    /// patch and the batch dies with "No valid patches in input". This is the
    /// splitting-a-file-into-a-module case (e.g. `src/e2e.rs` → `src/e2e/`).
    pub fn diff_workdir(&self, path: Option<&str>) -> anyhow::Result<String> {
        let repo = &self.repo;
        let index = self.index()?;

        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true);
        opts.show_untracked_content(true);
        // Expand untracked dirs one file at a time — see the doc comment above;
        // without this, files inside a new directory get empty patches.
        opts.recurse_untracked_dirs(true);

        let diff = repo
            .diff_index_to_workdir(Some(&index), Some(&mut opts))
            .context("failed to compute workdir diff")?;

        match path {
            Some(p) => format_diff_for_path(&diff, p),
            None => format_diff(&diff),
        }
    }

    /// Per-file stats for `paths` in the staged diff (HEAD → index) — exactly
    /// what the next commit would land. Mirrors [`Git::diff`]'s head-less
    /// handling: with no HEAD, every staged path counts as a new file.
    pub fn staged_stats(&self, paths: &[String]) -> anyhow::Result<Vec<FileStats>> {
        let head_tree = match self.repo.head() {
            Ok(r) => Some(r.peel_to_tree().context("failed to peel HEAD to tree")?),
            Err(_) => None,
        };
        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true);
        let index = self.index()?;
        let diff = self
            .repo
            .diff_tree_to_index(head_tree.as_ref(), Some(&index), Some(&mut opts))
            .context("failed to compute staged diff")?;
        Self::stats_from_diff(&diff, paths)
    }

    /// Per-file stats for `paths` in the commit `HEAD` just became — the
    /// landed counterpart of [`Git::staged_stats`]. Diffed against the parent
    /// commit (the empty tree for a root commit). Call after [`Git::commit`]:
    /// this reads the same HEAD whose short hash that call displays, so the
    /// stats always match the shown commit even if a hook moved HEAD.
    pub fn committed_stats(&self, paths: &[String]) -> anyhow::Result<Vec<FileStats>> {
        let head = self
            .repo
            .head()
            .context("failed to resolve HEAD after commit")?
            .peel_to_commit()
            .context("failed to peel HEAD to commit")?;
        let parent_tree = if head.parent_count() == 0 {
            None
        } else {
            Some(
                head.parent(0)
                    .context("failed to read parent commit")?
                    .tree()
                    .context("failed to read parent tree")?,
            )
        };
        let tree = head.tree().context("failed to read HEAD tree")?;
        let diff = self
            .repo
            .diff_tree_to_tree(parent_tree.as_ref(), Some(&tree), None)
            .context("failed to compute commit diff")?;
        Self::stats_from_diff(&diff, paths)
    }

    /// Count per-file added/deleted lines and new/removed/binary status from
    /// a libgit2 diff, restricted to `paths` (results keep the caller's
    /// order). One `foreach` walk attributes lines by delta path; deltas
    /// outside `paths` are skipped without stopping the walk. A path in
    /// `paths` that has no delta (e.g. a pre-commit hook that cancelled its
    /// changes) is kept at zero counts rather than dropped, so the footer
    /// always lists every planned file. Binary deltas fire no line events, so
    /// they are flagged from the file callback and keep zero counts.
    fn stats_from_diff(diff: &git2::Diff, paths: &[String]) -> anyhow::Result<Vec<FileStats>> {
        let mut stats: Vec<FileStats> = paths
            .iter()
            .map(|p| FileStats {
                path: p.clone(),
                added: 0,
                deleted: 0,
                new: false,
                removed: false,
                binary: false,
            })
            .collect();
        let mut new_flags = vec![false; paths.len()];
        let mut removed_flags = vec![false; paths.len()];
        let mut binary_flags = vec![false; paths.len()];
        let mut index: HashMap<&str, usize> = HashMap::with_capacity(paths.len());
        for (i, p) in paths.iter().enumerate() {
            index.insert(p.as_str(), i);
        }

        // The file and line callbacks run interleaved in one `foreach` walk,
        // so each captures disjoint data (flags vs counts) — two closures
        // borrowing the same Vec would not compile.
        let mut file_cb = |delta: git2::DiffDelta<'_>, _: f32| -> bool {
            let Some(i) = delta_path(&delta).and_then(|p| index.get(p).copied()) else {
                return true;
            };
            new_flags[i] = delta.status() == git2::Delta::Added;
            removed_flags[i] = delta.status() == git2::Delta::Deleted;
            binary_flags[i] = delta.flags().contains(git2::DiffFlags::BINARY);
            true
        };
        let mut line_cb = |delta: git2::DiffDelta<'_>,
                           _: Option<git2::DiffHunk<'_>>,
                           line: git2::DiffLine<'_>|
         -> bool {
            let Some(i) = delta_path(&delta).and_then(|p| index.get(p).copied()) else {
                return true;
            };
            match line.origin_value() {
                DiffLineType::Addition => stats[i].added += 1,
                DiffLineType::Deletion => stats[i].deleted += 1,
                _ => {}
            }
            true
        };
        diff.foreach(&mut file_cb, None, None, Some(&mut line_cb))
            .context("failed to walk diff")?;

        let mut out = Vec::with_capacity(paths.len());
        for (i, s) in stats.into_iter().enumerate() {
            out.push(FileStats {
                new: new_flags[i],
                removed: removed_flags[i],
                binary: binary_flags[i],
                ..s
            });
        }
        Ok(out)
    }
}

/// The path identifying a diff delta: the new-side path, falling back to the
/// old side (deletions keep both, but the fallback costs nothing). Non-UTF-8
/// paths yield `None` — the caller skips them, matching `status()`'s
/// UTF-8-only handling.
fn delta_path<'a>(delta: &git2::DiffDelta<'a>) -> Option<&'a str> {
    delta
        .new_file()
        .path()
        .or_else(|| delta.old_file().path())
        .and_then(|p| p.to_str())
}

fn format_line(line: &git2::DiffLine, output: &mut String) {
    match line.origin_value() {
        DiffLineType::Context | DiffLineType::Addition | DiffLineType::Deletion => {
            let origin = match line.origin() {
                '+' => "+",
                '-' => "-",
                ' ' => " ",
                _ => "",
            };
            output.push_str(origin);
            output.push_str(&String::from_utf8_lossy(line.content()));
        }
        _ => {
            output.push_str(&String::from_utf8_lossy(line.content()));
        }
    }
}

fn format_diff(diff: &git2::Diff) -> anyhow::Result<String> {
    if diff.deltas().len() == 0 {
        return Ok(String::new());
    }

    let mut output = String::new();
    diff.print(DiffFormat::Patch, |_delta, _hunk, line| {
        format_line(&line, &mut output);
        true
    })
    .context("failed to format diff")?;

    Ok(output)
}

fn format_diff_for_path(diff: &git2::Diff, path: &str) -> anyhow::Result<String> {
    let matched = diff
        .deltas()
        .any(|d| d.new_file().path().is_some_and(|p| p == Path::new(path)));
    if !matched {
        return Ok(String::new());
    }

    let mut output = String::new();
    diff.print(DiffFormat::Patch, |delta, _hunk, line| {
        if delta.new_file().path().is_none_or(|p| p != Path::new(path)) {
            return true;
        }
        format_line(&line, &mut output);
        true
    })
    .context("failed to format diff")?;

    Ok(output)
}
