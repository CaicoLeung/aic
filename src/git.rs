use anyhow::Context;
use git2::{DiffFormat, DiffLineType, Repository, Status};
use std::fs;
use std::path::Path;
use std::process::Command;

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

#[derive(Debug, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub struct FileStatus {
    pub path: String,
    pub staged: bool,
    pub kind: StatusKind,
}

#[derive(Debug, PartialEq, Eq, serde::Deserialize, serde::Serialize, schemars::JsonSchema)]
pub enum StatusKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Untracked,
}

// ----------------------------------------------------------------------
// Conflict resolution primitives (ADR 0005)
// ----------------------------------------------------------------------

/// Maximum worktree-file size we will hand to the LLM for resolution. Files at
/// or above this are skipped as oversized.
pub const MAX_CONFLICT_BYTES: usize = 50 * 1024; // 50 KB
pub const MAX_CONFLICT_LINES: usize = 2000;

/// Normalized git operation state, mapped from `git2::RepositoryState`. Drives
/// whether `aic resolve` can resolve + finalize (ADR 0005).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepoState {
    Clean,
    Merge,
    CherryPick,
    CherryPickSequence,
    Revert,
    RevertSequence,
    Rebase,
    RebaseInteractive,
    RebaseMerge,
    ApplyMailbox,
    ApplyMailboxOrRebase,
}

impl RepoState {
    pub fn from_git2(state: git2::RepositoryState) -> Self {
        use git2::RepositoryState as G;
        match state {
            G::Clean => Self::Clean,
            G::Merge => Self::Merge,
            G::Revert => Self::Revert,
            G::RevertSequence => Self::RevertSequence,
            G::CherryPick => Self::CherryPick,
            G::CherryPickSequence => Self::CherryPickSequence,
            // Bisect is not a conflict-bearing state — treat like Clean so it
            // never trips the guard or the auto-detect prompt.
            G::Bisect => Self::Clean,
            G::Rebase => Self::Rebase,
            G::RebaseInteractive => Self::RebaseInteractive,
            G::RebaseMerge => Self::RebaseMerge,
            G::ApplyMailbox => Self::ApplyMailbox,
            G::ApplyMailboxOrRebase => Self::ApplyMailboxOrRebase,
        }
    }

    /// v1 resolves and finalizes these states end-to-end (ADR 0005).
    pub fn resolvable(&self) -> bool {
        matches!(
            self,
            Self::Merge
                | Self::CherryPick
                | Self::CherryPickSequence
                | Self::Revert
                | Self::RevertSequence
        )
    }

    /// Any non-clean conflict-bearing state. Drives the default-run auto-detect
    /// prompt and the commit guard.
    pub fn is_conflicted(&self) -> bool {
        !matches!(self, Self::Clean)
    }

    /// Short human label for messages, e.g. `merge`, `cherry-pick`, `rebase`.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Merge => "merge",
            Self::CherryPick => "cherry-pick",
            Self::CherryPickSequence => "cherry-pick",
            Self::Revert => "revert",
            Self::RevertSequence => "revert",
            Self::Rebase | Self::RebaseInteractive | Self::RebaseMerge => "rebase",
            Self::ApplyMailbox | Self::ApplyMailboxOrRebase => "am",
        }
    }

    /// The git invocation that finalizes this state (`(program, args)`), or
    /// `None` when aic won't finalize it (rebase / am).
    pub fn finalize_invocation(&self) -> Option<(&'static str, &'static [&'static str])> {
        match self {
            Self::Merge => Some(("git", &["commit", "--no-edit"][..])),
            Self::CherryPick | Self::CherryPickSequence => {
                Some(("git", &["cherry-pick", "--continue"][..]))
            }
            Self::Revert | Self::RevertSequence => Some(("git", &["revert", "--continue"][..])),
            _ => None,
        }
    }
}

/// Why a conflicted file is or isn't eligible for AI resolution (ADR 0005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictKind {
    /// Modify/modify content conflict in a UTF-8 text file under the size cap —
    /// eligible for the LLM resolver.
    Content,
    /// Non-UTF-8 or contains NUL bytes — cannot be fed to the LLM.
    Binary,
    /// One side deleted the file (missing `our` or `their` stage) — structural,
    /// not a textual merge the LLM can resolve.
    DeleteModify,
    /// Text file at or above the size cap.
    Oversized { bytes: usize, lines: usize },
}

impl ConflictKind {
    pub fn resolvable(&self) -> bool {
        matches!(self, Self::Content)
    }

    /// One-word reason for skip messages.
    pub fn reason(&self) -> &'static str {
        match self {
            Self::Content => "content",
            Self::Binary => "binary",
            Self::DeleteModify => "delete/modify",
            Self::Oversized { .. } => "oversized",
        }
    }
}

#[derive(Debug, Clone)]
pub struct ConflictedFile {
    pub path: String,
    pub kind: ConflictKind,
}

/// Detect leftover git conflict markers in file content. Flags the
/// unambiguous `<<<<<<<` and `>>>>>>>` line prefixes (vanishingly rare in real
/// code); `=======` alone is too common (markdown rulers, rules files) to flag
/// on its own.
pub fn has_conflict_markers(content: &str) -> bool {
    content
        .lines()
        .any(|line| line.starts_with("<<<<<<<") || line.starts_with(">>>>>>>"))
}

pub struct Git;

impl Git {
    fn repo() -> anyhow::Result<Repository> {
        Repository::discover(".").with_context(|| "failed to discover git repository")
    }

    pub fn status() -> anyhow::Result<Vec<FileStatus>> {
        let repo = Self::repo()?;
        let statuses = repo
            .statuses(None)
            .context("failed to get repository status")?;

        let mut result = Vec::new();

        for entry in statuses.iter() {
            let path = match entry.path() {
                Ok(p) => p.to_string(),
                Err(_) => continue,
            };
            let flags = entry.status();

            if flags.intersects(
                Status::INDEX_NEW
                    | Status::INDEX_MODIFIED
                    | Status::INDEX_DELETED
                    | Status::INDEX_RENAMED,
            ) {
                result.push(FileStatus {
                    path: path.clone(),
                    staged: true,
                    kind: index_status_kind(flags),
                });
            }

            if flags.intersects(
                Status::WT_NEW | Status::WT_MODIFIED | Status::WT_DELETED | Status::WT_RENAMED,
            ) {
                result.push(FileStatus {
                    path,
                    staged: false,
                    kind: wt_status_kind(flags),
                });
            }
        }

        Ok(result)
    }

    pub fn diff(path: Option<&str>) -> anyhow::Result<String> {
        let repo = Self::repo()?;
        let head_tree = match repo.head() {
            Ok(r) => Some(r.peel_to_tree().context("failed to peel HEAD to tree")?),
            Err(_) => None,
        };

        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true);
        if let Some(p) = path {
            opts.pathspec(p);
        }

        let diff = repo
            .diff_tree_to_index(head_tree.as_ref(), None, Some(&mut opts))
            .context("failed to compute diff")?;

        format_diff(&diff)
    }

    pub fn diff_workdir(path: Option<&str>) -> anyhow::Result<String> {
        let repo = Self::repo()?;
        let index = repo.index().context("failed to get repository index")?;

        let mut opts = git2::DiffOptions::new();
        opts.include_untracked(true);
        opts.show_untracked_content(true);

        let diff = repo
            .diff_index_to_workdir(Some(&index), Some(&mut opts))
            .context("failed to compute workdir diff")?;

        match path {
            Some(p) => format_diff_for_path(&diff, p),
            None => format_diff(&diff),
        }
    }

    pub fn add(paths: &[&str]) -> anyhow::Result<()> {
        let repo = Self::repo()?;
        let mut index = repo.index().context("failed to get repository index")?;

        if paths.is_empty() {
            index
                .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
                .context("failed to add all files to index")?;
        } else {
            let workdir = repo.workdir();
            for path in paths {
                // Mirror `git add <path>`: a present file is added/updated, a
                // tracked-but-missing file is staged as a deletion, and a path
                // that is neither on disk nor tracked is rejected. `index.add_path`
                // only handles the first case (it stats the file and errors with
                // NotFound when it's gone), so tracked-but-absent paths route
                // through `remove_path`. Bailing on absent-and-untracked paths
                // matters: the batch plan comes from the LLM, and silently
                // no-oping a hallucinated pathspec would commit without it and
                // still report success — real `git add` errors with "pathspec ...
                // did not match any files", and so do we.
                let on_disk = workdir.is_some_and(|w| w.join(path).exists());
                let tracked = index.get_path(Path::new(path), 0).is_some();
                if on_disk {
                    index
                        .add_path(Path::new(path))
                        .with_context(|| format!("failed to add {path} to index"))?;
                } else if tracked {
                    index
                        .remove_path(Path::new(path))
                        .with_context(|| format!("failed to stage removal of {path}"))?;
                } else {
                    anyhow::bail!(
                        "pathspec '{path}' did not match any tracked or working-tree file"
                    );
                }
            }
        }

        index.write().context("failed to write index")?;
        Ok(())
    }

    pub fn commit(message: String, body: Option<String>) -> anyhow::Result<String> {
        Self::assert_commit_safe()?;
        let repo = Self::repo()?;
        let mut index = repo.index().context("failed to get repository index")?;
        let tree_id = index.write_tree_to(&repo).context("failed to write tree")?;
        let tree = repo.find_tree(tree_id).context("failed to find tree")?;
        let sig = repo.signature().context("failed to get git signature")?;

        let parents: Vec<_> = match repo.head() {
            Ok(r) => vec![
                r.peel_to_commit()
                    .context("failed to peel HEAD to commit")?,
            ],
            Err(_) => vec![],
        };
        let parent_refs: Vec<_> = parents.iter().collect();

        let full_message = match body {
            Some(b) => format!("{message}\n\n{b}"),
            None => message.to_string(),
        };
        let oid = repo
            .commit(Some("HEAD"), &sig, &sig, &full_message, &tree, &parent_refs)
            .context("failed to create commit")?;

        // First 7 hex chars — the conventional short hash.
        Ok(oid.to_string()[..7].to_string())
    }

    // ------------------------------------------------------------------
    // Conflict resolution surface (ADR 0005)
    // ------------------------------------------------------------------

    pub fn state() -> anyhow::Result<RepoState> {
        let repo = Self::repo()?;
        Ok(RepoState::from_git2(repo.state()))
    }

    /// Every unmerged path in the index, classified by whether the LLM can
    /// resolve it. Driven by `Index::conflicts()` (ancestor/our/their stages).
    pub fn conflicted_files() -> anyhow::Result<Vec<ConflictedFile>> {
        let repo = Self::repo()?;
        let index = repo.index().context("failed to get repository index")?;
        let mut out = Vec::new();
        for conflict in index.conflicts()? {
            let c = conflict.context("failed to read index conflict")?;
            let path = conflict_path(&c);
            let kind = match (c.our.as_ref(), c.their.as_ref()) {
                (None, _) | (_, None) => ConflictKind::DeleteModify,
                _ => classify_worktree(&repo, &path),
            };
            out.push(ConflictedFile { path, kind });
        }
        Ok(out)
    }

    /// Read the current working-tree bytes for a path (the file git wrote
    /// conflict markers into).
    pub fn read_worktree(path: &str) -> anyhow::Result<Vec<u8>> {
        let repo = Self::repo()?;
        let workdir = repo
            .workdir()
            .context("repository has no working directory")?;
        fs::read(workdir.join(path)).with_context(|| format!("failed to read worktree file {path}"))
    }

    /// Overwrite a working-tree file with resolved content (called only after
    /// the user approves the resolution).
    pub fn write_worktree(path: &str, content: &str) -> anyhow::Result<()> {
        let repo = Self::repo()?;
        let workdir = repo
            .workdir()
            .context("repository has no working directory")?;
        fs::write(workdir.join(path), content)
            .with_context(|| format!("failed to write resolved file {path}"))?;
        Ok(())
    }

    /// Finalize a resolved conflict state by shelling out to git. `GIT_EDITOR`
    /// is set to `true` so `--continue` / `commit --no-edit` never block on an
    /// editor and git's default message is kept verbatim (ADR 0005).
    pub fn finalize(state: RepoState) -> anyhow::Result<()> {
        let (prog, args) = state.finalize_invocation().ok_or_else(|| {
            anyhow::anyhow!(
                "aic cannot finalize a {} state in v1; resolve manually \
                 (e.g. `git rebase --continue`)",
                state.label()
            )
        })?;
        let status = Command::new(prog)
            .args(args)
            .env("GIT_EDITOR", "true")
            .status()
            .with_context(|| format!("failed to run {prog} {}", args.join(" ")))?;
        if !status.success() {
            anyhow::bail!("{prog} {} exited with {status}", args.join(" "));
        }
        Ok(())
    }

    /// Feature #2 commit guard. Aborts before any commit when the repo is
    /// mid-operation or a staged file still contains conflict markers. Called
    /// at the top of [`Git::commit`] so every commit path inherits it.
    pub fn assert_commit_safe() -> anyhow::Result<()> {
        let repo = Self::repo()?;
        let state = RepoState::from_git2(repo.state());
        if state.is_conflicted() {
            anyhow::bail!(
                "repo is mid-{} (unresolved conflict state); run `aic resolve` \
                 or finalize manually",
                state.label()
            );
        }

        let statuses = repo
            .statuses(None)
            .context("failed to get repository status")?;
        for entry in statuses.iter() {
            let flags = entry.status();
            if !flags.intersects(Status::INDEX_NEW | Status::INDEX_MODIFIED | Status::INDEX_RENAMED)
            {
                continue;
            }
            let path = match entry.path() {
                Ok(p) => p.to_string(),
                Err(_) => continue,
            };
            let Some(workdir) = repo.workdir() else {
                break;
            };
            let bytes = match fs::read(workdir.join(&path)) {
                Ok(b) => b,
                Err(_) => continue, // deleted / unreadable — nothing to scan
            };
            let content = String::from_utf8_lossy(&bytes);
            if has_conflict_markers(&content) {
                anyhow::bail!(
                    "cannot commit: {path} still contains conflict markers; \
                     run `aic resolve`"
                );
            }
        }
        Ok(())
    }
}

fn index_status_kind(flags: Status) -> StatusKind {
    if flags.contains(Status::INDEX_NEW) {
        StatusKind::Added
    } else if flags.contains(Status::INDEX_MODIFIED) {
        StatusKind::Modified
    } else if flags.contains(Status::INDEX_DELETED) {
        StatusKind::Deleted
    } else {
        StatusKind::Renamed
    }
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
            "[{}] lines {}-{}\n",
            block.header, block.new_start, end
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

fn wt_status_kind(flags: Status) -> StatusKind {
    if flags.contains(Status::WT_NEW) {
        StatusKind::Untracked
    } else if flags.contains(Status::WT_MODIFIED) {
        StatusKind::Modified
    } else if flags.contains(Status::WT_DELETED) {
        StatusKind::Deleted
    } else {
        StatusKind::Renamed
    }
}

/// Pull a path out of an `IndexConflict` — prefers `our`, then `their`, then the
/// common ancestor. `IndexEntry.path` is an `Option<CString>`; lossy-convert
/// to a Rust string (non-UTF-8 paths are exotic and not aic's target).
fn conflict_path(c: &git2::IndexConflict) -> String {
    let entry = c.our.as_ref().or(c.their.as_ref()).or(c.ancestor.as_ref());
    match entry {
        // `IndexEntry.path` is `Vec<u8>` (raw path bytes) in git2 0.21.
        Some(e) => String::from_utf8_lossy(&e.path).into_owned(),
        None => "(unknown)".to_string(),
    }
}

/// Classify a conflicted file by reading its working-tree bytes. Only
/// `Content` conflicts are eligible for the LLM resolver (ADR 0005).
fn classify_worktree(repo: &Repository, path: &str) -> ConflictKind {
    let Some(workdir) = repo.workdir() else {
        return ConflictKind::Binary;
    };
    let bytes = match fs::read(workdir.join(path)) {
        Ok(b) => b,
        Err(_) => return ConflictKind::DeleteModify,
    };
    if bytes.contains(&0u8) {
        return ConflictKind::Binary;
    }
    let Ok(text) = std::str::from_utf8(&bytes) else {
        return ConflictKind::Binary;
    };
    let lines = text.lines().count();
    if bytes.len() > MAX_CONFLICT_BYTES || lines > MAX_CONFLICT_LINES {
        return ConflictKind::Oversized {
            bytes: bytes.len(),
            lines,
        };
    }
    ConflictKind::Content
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::Mutex;

    struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        fn new(dir: &Path) -> Self {
            let original = std::env::current_dir().unwrap();
            std::env::set_current_dir(dir).unwrap();
            Self { original }
        }
    }

    impl Drop for CwdGuard {
        fn drop(&mut self) {
            let _ = std::env::set_current_dir(&self.original);
        }
    }

    fn init_test_repo(dir: &Path) {
        let repo = Repository::init(dir).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "test").unwrap();
        config.set_str("user.email", "test@test.com").unwrap();

        std::fs::write(dir.join("tracked.txt"), "original\n").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("tracked.txt")).unwrap();
        index.write().unwrap();

        let tree_id = index.write_tree_to(&repo).unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let sig = repo.signature().unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();
    }

    /// Serializes tests that mutate the process working directory via `CwdGuard`.
    /// Parallel CwdGuard tests race on the global CWD and intermittently resolve
    /// the wrong repository, so any test that chdir()s must hold this lock.
    static GIT_CWD_MUTEX: Mutex<()> = Mutex::new(());

    #[test]
    fn diff_workdir_returns_untracked_content() {
        let _lock = GIT_CWD_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        std::fs::write(dir.path().join("new_file.txt"), "new content\n").unwrap();

        let _guard = CwdGuard::new(dir.path());
        let result = Git::diff_workdir(Some("new_file.txt")).unwrap();
        assert!(
            !result.is_empty(),
            "should have diff content for untracked file"
        );
    }

    #[test]
    fn diff_workdir_returns_modified_content() {
        let _lock = GIT_CWD_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        std::fs::write(dir.path().join("tracked.txt"), "modified\n").unwrap();

        let _guard = CwdGuard::new(dir.path());
        let result = Git::diff_workdir(Some("tracked.txt")).unwrap();
        assert!(
            !result.is_empty(),
            "should have diff content for modified file"
        );
    }

    #[test]
    fn diff_workdir_returns_deleted_content() {
        let _lock = GIT_CWD_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();

        let _guard = CwdGuard::new(dir.path());
        let result = Git::diff_workdir(Some("tracked.txt")).unwrap();
        assert!(
            !result.is_empty(),
            "should have diff content for deleted file"
        );
    }

    /// Regression: `Git::add` must stage a working-tree deletion. Previously it
    /// called `index.add_path`, which stats the file on disk and failed with
    /// NotFound for deleted files — breaking the whole unstaged-deletion flow.
    #[test]
    fn add_stages_working_tree_deletion() {
        let _lock = GIT_CWD_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        std::fs::remove_file(dir.path().join("tracked.txt")).unwrap();

        let _guard = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).expect("add should stage a deleted file");

        let repo = Repository::open(dir.path()).unwrap();
        let statuses = repo.statuses(None).unwrap();
        let entry = statuses
            .iter()
            .find(|s| s.path() == Ok("tracked.txt"))
            .unwrap();
        assert!(
            entry.status().contains(Status::INDEX_DELETED),
            "deletion should be staged in the index"
        );
    }

    /// `Git::add` must reject a pathspec that is neither on disk nor tracked,
    /// matching `git add`'s "did not match any files" error. Without this, a
    /// hallucinated path from the LLM batch plan would be silently dropped
    /// and the commit would report success while missing a file.
    #[test]
    fn add_rejects_untracked_absent_path() {
        let _lock = GIT_CWD_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        let _guard = CwdGuard::new(dir.path());
        let err = Git::add(&["does-not-exist.txt"])
            .expect_err("add should reject an absent, untracked path");
        assert!(
            format!("{err:#}").contains("did not match any tracked or working-tree file"),
            "error should explain the pathspec mismatch, got: {err:#}"
        );
    }

    /// Guard: `Git::diff` (tree-to-index) must return content for a staged
    /// deletion so the commit-message LLM has something to describe. This is
    /// load-bearing for the deletion flow — `generate_and_commit` calls
    /// `Git::diff` after staging a removal.
    #[test]
    fn diff_returns_content_for_staged_deletion() {
        let _lock = GIT_CWD_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        {
            let repo = Repository::open(dir.path()).unwrap();
            let mut index = repo.index().unwrap();
            index.remove_path(Path::new("tracked.txt")).unwrap();
            index.write().unwrap();
        }

        let _guard = CwdGuard::new(dir.path());
        let result = Git::diff(Some("tracked.txt")).unwrap();
        assert!(
            !result.is_empty(),
            "should have diff content for a staged deletion"
        );
    }

    // ------------------------------------------------------------------
    // Conflict-resolution tests (ADR 0005)
    // ------------------------------------------------------------------

    #[test]
    fn conflict_markers_detected() {
        assert!(has_conflict_markers(
            "<<<<<<< HEAD\na\n=======\nb\n>>>>>>> x\n"
        ));
        assert!(has_conflict_markers("plain\n>>>>>>> branch\n"));
        assert!(has_conflict_markers("<<<<<<<\n"));
    }

    #[test]
    fn plain_content_has_no_markers() {
        // `=======` alone is NOT a marker — too common in markdown/rules.
        assert!(!has_conflict_markers("=======\nsection\n=======\n"));
        assert!(!has_conflict_markers(
            "fn main() {\n    println!(\"hi\");\n}\n"
        ));
        assert!(!has_conflict_markers(""));
    }

    #[test]
    fn repo_state_resolvable_set() {
        assert!(RepoState::Merge.resolvable());
        assert!(RepoState::CherryPick.resolvable());
        assert!(RepoState::CherryPickSequence.resolvable());
        assert!(RepoState::Revert.resolvable());
        assert!(RepoState::RevertSequence.resolvable());
        assert!(!RepoState::Clean.resolvable());
        assert!(!RepoState::Rebase.resolvable());
        assert!(!RepoState::RebaseMerge.resolvable());
        assert!(!RepoState::ApplyMailbox.resolvable());
    }

    #[test]
    fn repo_state_finalize_invocation() {
        assert_eq!(
            RepoState::Merge.finalize_invocation(),
            Some(("git", &["commit", "--no-edit"][..]))
        );
        assert_eq!(
            RepoState::CherryPick.finalize_invocation(),
            Some(("git", &["cherry-pick", "--continue"][..]))
        );
        assert_eq!(
            RepoState::Revert.finalize_invocation(),
            Some(("git", &["revert", "--continue"][..]))
        );
        assert_eq!(RepoState::Rebase.finalize_invocation(), None);
        assert_eq!(RepoState::Clean.finalize_invocation(), None);
    }

    #[test]
    fn repo_state_is_conflicted_excludes_clean_and_bisect() {
        assert!(!RepoState::Clean.is_conflicted());
        assert!(RepoState::Merge.is_conflicted());
        assert!(RepoState::Rebase.is_conflicted());
        // Bisect maps to Clean in from_git2 — not a conflict state.
        assert!(!RepoState::from_git2(git2::RepositoryState::Bisect).is_conflicted());
    }

    /// Set up a real content conflict in `dir` via the `git` CLI: master and
    /// `other` both change the same line of `tracked.txt` differently, then
    /// `git merge other` produces a conflict. Repo ends in the Merge state with
    /// conflict markers in `tracked.txt`.
    fn make_content_conflict(dir: &Path) {
        let git = |args: &[&str]| {
            Command::new("git")
                .args(["-C"])
                .arg(dir)
                .args(args)
                .status()
                .expect("git command ran")
        };
        // init_test_repo left HEAD at the initial commit on master.
        git(&["branch", "other"]);

        std::fs::write(dir.join("tracked.txt"), "master\n").unwrap();
        let _ = git(&["add", "tracked.txt"]);
        let _ = git(&["commit", "-m", "master side"]);

        let _ = git(&["checkout", "other"]);
        std::fs::write(dir.join("tracked.txt"), "other\n").unwrap();
        let _ = git(&["add", "tracked.txt"]);
        let _ = git(&["commit", "-m", "other side"]);

        let _ = git(&["checkout", "master"]);
        // Non-zero exit on conflict is expected; .status() returns Ok regardless.
        let _ = git(&["merge", "other"]);
    }

    #[test]
    fn conflicted_files_classifies_content_conflict() {
        let _lock = GIT_CWD_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        make_content_conflict(dir.path());

        let _guard = CwdGuard::new(dir.path());
        assert_eq!(Git::state().unwrap(), RepoState::Merge);

        let files = Git::conflicted_files().unwrap();
        assert_eq!(files.len(), 1, "exactly one conflicted file");
        assert_eq!(files[0].path, "tracked.txt");
        assert_eq!(files[0].kind, ConflictKind::Content);
    }

    #[test]
    fn assert_commit_safe_blocks_mid_merge() {
        let _lock = GIT_CWD_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        make_content_conflict(dir.path());

        let _guard = CwdGuard::new(dir.path());
        let err = Git::assert_commit_safe().expect_err("must abort mid-merge");
        assert!(
            format!("{err:#}").contains("mid-merge"),
            "expected mid-merge message, got: {err:#}"
        );
    }

    #[test]
    fn assert_commit_safe_blocks_staged_markers() {
        let _lock = GIT_CWD_MUTEX.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        // Repo state is Clean, but a staged file carries leftover markers.
        std::fs::write(
            dir.path().join("tracked.txt"),
            "<<<<<<< HEAD\nmine\n=======\nyours\n>>>>>>> branch\n",
        )
        .unwrap();

        let _guard = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).unwrap();

        let err = Git::assert_commit_safe().expect_err("must abort on staged markers");
        assert!(
            format!("{err:#}").contains("conflict markers"),
            "expected marker message, got: {err:#}"
        );
    }
}
