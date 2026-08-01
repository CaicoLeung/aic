use anyhow::Context;
use git2::{DiffFormat, DiffLineType, ObjectType, Repository, Status, Tree};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};

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
/// of `git add -p`. Hunk order matches the diff, so a 1-based index into
/// `hunks` is a stable handle the LLM can reference.
#[derive(Debug, Clone)]
pub struct FilePatch {
    /// Lines before the first `@@` hunk header: `diff --git`, `index`,
    /// `---`, `+++` (and any leading noise).
    pub header: String,
    /// Each hunk's full text, starting at its `@@ … @@` line and including
    /// every body line, in file order.
    pub hunks: Vec<String>,
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

    /// The git args that finalize this state, or `None` when aic won't
    /// finalize it (rebase / am). The program is always `git` — `run_git`
    /// spawns it — so it is not part of the contract.
    pub fn finalize_invocation(&self) -> Option<&'static [&'static str]> {
        match self {
            Self::Merge => Some(&["commit", "--no-edit"][..]),
            Self::CherryPick | Self::CherryPickSequence => Some(&["cherry-pick", "--continue"][..]),
            Self::Revert | Self::RevertSequence => Some(&["revert", "--continue"][..]),
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

/// Run the real `git` CLI inside the repo: spawns `git` with `args`, writes
/// `stdin` to its stdin when `Some`, applies `envs` when non-empty, and returns
/// captured **stdout**.
///
/// stderr is captured too, but only to surface git's own diagnostic on a
/// non-zero exit — the real reason (a hook veto, a missing merge message, a
/// refused patch), never a bare exit code. Some refusals (`git commit`'s
/// "nothing to commit") write to stdout instead, so stdout is the fallback
/// detail when stderr is empty. No success-path caller needs stderr, so it
/// stays out of the return type; the authored-commit migration consumes
/// the returned stdout.
///
/// The command line appears in the error exactly once (here). Callers
/// propagate with a bare `?`, except where they name the user-facing operation
/// (hunk staging) or correct the state the failure implies (a commit that
/// landed) — a context layer must never mislabel a clean non-zero exit as a
/// spawn failure. If git exits before consuming stdin, the broken pipe is
/// discarded in favor of git's own stderr: the refusal that closed the pipe is
/// the reason the caller needs.
fn run_git(args: &[&str], stdin: Option<&str>, envs: &[(&str, &str)]) -> anyhow::Result<String> {
    let command_line = format!("git {}", args.join(" "));
    let mut cmd = Command::new("git");
    cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
    if stdin.is_some() {
        cmd.stdin(Stdio::piped());
    } else {
        cmd.stdin(Stdio::null());
    }
    for (key, value) in envs {
        cmd.env(key, value);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("failed to spawn {command_line}"))?;
    if let Some(body) = stdin {
        use std::io::Write as _;
        let mut child_stdin = child.stdin.take().context("failed to open stdin for git")?;
        if let Err(write_err) = child_stdin.write_all(body.as_bytes()) {
            // git exited before consuming stdin (e.g. it already knew it would
            // refuse). Drop the handle so git sees EOF, wait for its verdict,
            // and surface its stderr — the write error is just the symptom.
            drop(child_stdin);
            let output = child
                .wait_with_output()
                .with_context(|| format!("failed to wait on {command_line}"))?;
            if !output.status.success() {
                return Err(nonzero_exit(
                    &command_line,
                    output.status,
                    &output.stderr,
                    &output.stdout,
                ));
            }
            // Bizarre: git exited 0 without reading stdin. The write error is
            // the only explanation left.
            return Err(write_err)
                .with_context(|| format!("failed to write stdin to {command_line}"));
        }
        // child_stdin drops here → git sees EOF and processes the input.
    }
    let output = child
        .wait_with_output()
        .with_context(|| format!("failed to wait on {command_line}"))?;
    if !output.status.success() {
        return Err(nonzero_exit(
            &command_line,
            output.status,
            &output.stderr,
            &output.stdout,
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// The canonical non-zero-exit diagnostic: the command line (once), the exit
/// status, and git's own diagnostic when it has one — the real reason (a hook
/// veto, a missing merge message, a refused patch), never a bare exit code.
/// stderr is the convention for errors; some refusals (e.g. `git commit`'s
/// "nothing to commit") go to stdout instead, so it is the fallback.
fn nonzero_exit(
    command_line: &str,
    status: ExitStatus,
    stderr: &[u8],
    stdout: &[u8],
) -> anyhow::Error {
    let stderr_trim = String::from_utf8_lossy(stderr).trim().to_owned();
    let stdout_trim = String::from_utf8_lossy(stdout).trim().to_owned();
    let reason = if !stderr_trim.is_empty() {
        stderr_trim
    } else if !stdout_trim.is_empty() {
        stdout_trim
    } else {
        String::new()
    };
    let detail = if reason.is_empty() {
        format!("exited with {status}")
    } else {
        format!("exited with {status} — {reason}")
    };
    anyhow::anyhow!("{command_line}: {detail}")
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
    pub fn diff_workdir(path: Option<&str>) -> anyhow::Result<String> {
        let repo = Self::repo()?;
        let index = repo.index().context("failed to get repository index")?;

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
    /// Stage a subset of one file's hunks into the index — the non-interactive
    /// analogue of `git add -p`. `raw_diff` is the file's original workdir-vs-
    /// HEAD diff (captured once so hunk numbering stays stable across commits
    /// to the same file); `hunk_indices` are 1-based positions in that diff's
    /// hunk order. The selected hunks are rebuilt into a patch and applied with
    /// `git apply --cached`, which relocates each hunk by its context lines —
    /// so it still lands correctly after an earlier batch's commit shifted line
    /// numbers.
    pub fn stage_hunks(raw_diff: &str, hunk_indices: &[usize]) -> anyhow::Result<()> {
        let patch = parse_file_patch(raw_diff);
        let mut body = patch.header;
        for &idx in hunk_indices {
            let i = idx
                .checked_sub(1)
                .context("hunk indices are 1-based and must be >= 1")?;
            let hunk = patch.hunks.get(i).with_context(|| {
                format!(
                    "hunk {idx} is out of range (file has {} hunk(s))",
                    patch.hunks.len()
                )
            })?;
            body.push_str(hunk);
        }

        // Name the aic operation the user can act on; the helper's cause layer
        // carries the command and git's own stderr.
        run_git(&["apply", "--cached", "-"], Some(&body), &[])
            .with_context(|| "git apply --cached rejected the selected hunks")?;
        Ok(())
    }

    /// Author a commit by shelling out to the real `git commit` (issue #19).
    ///
    /// libgit2 never runs git hooks, so a Run landed commits that bypassed
    /// husky / prettier / lint-staged and `commit-msg` enforcement. Shelling
    /// out delegates everything libgit2 skipped: `pre-commit`,
    /// `prepare-commit-msg`, and `commit-msg` hooks run in order, `commit.gpgsign`
    /// is honored, and the commit is signed with the repo's own
    /// `user.name` / `user.email` signature — the same reason the
    /// conflict-finalize path already shells out (ADR 0005: "the native CLI
    /// does the right thing"). Staging is untouched: `Git::add` and hunk
    /// staging already wrote a git-compatible index that `git commit` consumes.
    ///
    /// The message (and optional body) travel over stdin via `-F -`, so quotes,
    /// `%` substitutions, and newlines survive with no shell escaping;
    /// `--cleanup=verbatim` keeps git from cleaning the message up (the default
    /// `strip` collapses consecutive blank lines and strips trailing
    /// whitespace) — the LLM's body lands byte-for-byte, as libgit2 committed
    /// it. `-F` already suppresses the editor, so no `GIT_EDITOR=true` is
    /// needed (unlike the finalize `--continue` path).
    ///
    /// The always-on commit guard (`assert_commit_safe`: refuse while the repo
    /// is mid-operation, or a staged blob still holds conflict markers) runs
    /// first, before any shell-out. With nothing staged, `git commit` refuses
    /// ("nothing to commit") instead of silently landing an empty commit as
    /// libgit2 did — aic always stages before committing, so this only fires
    /// on a state we would not want to commit anyway. An empty message is
    /// refused the same way ("empty commit message") where libgit2 created
    /// the commit. The guard scans the index *before* hooks run, and a hook
    /// can re-stage files the scan never saw — so the landed tree is verified
    /// again after the commit (`verify_commit_clean`): marker-bearing hook
    /// output is reported as the landed commit it is, with the recovery path.
    ///
    /// Returns the first 7 hex chars of the new HEAD — the same format the
    /// libgit2 path returned (`oid.to_string()[..7]`), the conventional short
    /// hash. The value is the commit git just created: commit hooks run before
    /// the commit object is written, so HEAD is that commit (only a hook that
    /// itself commits could move HEAD further — and then the displayed hash is
    /// the state the user actually sees). If resolving HEAD fails after the
    /// commit landed, the error says so instead of implying the commit failed.
    pub fn commit(message: String, body: Option<String>) -> anyhow::Result<String> {
        Self::assert_commit_safe()?;
        let full_message = match body {
            Some(b) => format!("{message}\n\n{b}"),
            None => message,
        };
        run_git(
            &["commit", "-F", "-", "--cleanup=verbatim"],
            Some(&full_message),
            &[],
        )?;
        // First 7 hex chars of the new HEAD — the same format the libgit2 path
        // returned (`oid.to_string()[..7]`), the conventional short hash.
        let head = run_git(&["rev-parse", "HEAD"], None, &[])
            .with_context(|| "commit landed, but the new HEAD could not be resolved")?;
        let short = head
            .trim()
            .get(..7)
            .ok_or_else(|| anyhow::anyhow!("unexpected HEAD output from rev-parse: {head:?}"))?;
        // Verify what actually shipped: the pre-commit guard scanned the index
        // *before* hooks ran; a hook that re-staged content is not in that
        // scan. Check the landed tree so marker-bearing hook output surfaces
        // instead of silently shipping.
        Self::verify_commit_clean()?;
        Ok(short.to_string())
    }

    /// Post-commit verification — the enforcement half of the hook window.
    ///
    /// [`Git::commit`]'s guard (`assert_commit_safe`) scans the index *before*
    /// the shell-out, but a `pre-commit` hook (husky / lint-staged / prettier)
    /// runs after it and can re-stage files the scan never saw. Under libgit2
    /// the guard-time index was always the committed tree; with hooks it is
    /// not. This scans the tree the commit actually landed and reports any
    /// marker-bearing blob — as the landed commit it is, naming the file and
    /// the recovery path, never as a "commit failed" lie.
    fn verify_commit_clean() -> anyhow::Result<()> {
        let repo = Self::repo()?;
        let head = repo.head().context("failed to resolve HEAD after commit")?;
        let tree = head.peel_to_tree().context("failed to read HEAD tree")?;
        let mut marked = Vec::new();
        Self::collect_marked_paths(&repo, &tree, "", &mut marked)?;
        if let Some(first) = marked.first() {
            let list = if marked.len() == 1 {
                first.clone()
            } else {
                format!("{first} (and {} more)", marked.len() - 1)
            };
            anyhow::bail!(
                "commit landed, but its tree still contains conflict markers in {list}; \
                 fix the file(s) and run `git commit --amend --no-edit`, \
                 or `git reset --soft HEAD~1` and recommit"
            );
        }
        Ok(())
    }

    /// Depth-first walk of a tree, collecting paths whose blob content trips
    /// [`has_conflict_markers`] — the same detector the pre-commit guard uses,
    /// over the tree git actually committed. Submodules (commits) and any
    /// other non-blob entries cannot hold marker text and are skipped.
    fn collect_marked_paths(
        repo: &Repository,
        tree: &Tree,
        prefix: &str,
        out: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        for entry in tree.iter() {
            let path = format!("{prefix}{}", entry.name().unwrap_or("?"));
            match entry.kind() {
                Some(ObjectType::Tree) => {
                    let sub = entry
                        .to_object(repo)
                        .with_context(|| format!("failed to read subtree {path}"))?
                        .peel_to_tree()
                        .context("failed to peel subtree to tree")?;
                    Self::collect_marked_paths(repo, &sub, &format!("{path}/"), out)?;
                }
                Some(ObjectType::Blob) => {
                    let blob = entry
                        .to_object(repo)
                        .with_context(|| format!("failed to read blob {path}"))?
                        .peel_to_blob()
                        .context("failed to peel blob object")?;
                    let content = String::from_utf8_lossy(blob.content());
                    if has_conflict_markers(&content) {
                        out.push(path);
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The repository's working directory (the checkout root), resolved from
    /// the current directory. Used by the run-state layer to place `.aic/`.
    pub fn workdir() -> anyhow::Result<PathBuf> {
        let repo = Self::repo()?;
        repo.workdir()
            .map(|p| p.to_path_buf())
            .context("repository has no working directory (bare repo)")
    }

    /// Short (7-char) HEAD oid, for stamping the plan's origin commit.
    pub fn head_short() -> anyhow::Result<String> {
        let head = run_git(&["rev-parse", "--short", "HEAD"], None, &[])
            .context("failed to resolve HEAD")?;
        Ok(head.trim().to_string())
    }

    /// Mixed-reset the index to HEAD (unstage everything, leave the workdir
    /// untouched). Resume replay calls this before staging each pending batch
    /// so a batch that staged its hunks then failed before committing does not
    /// leave a dirty index that makes re-staging fail on a context mismatch.
    pub fn reset_index_to_head() -> anyhow::Result<()> {
        run_git(&["reset", "--mixed", "--quiet", "HEAD"], None, &[])
            .context("failed to reset index to HEAD")?;
        Ok(())
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
        // A bare `?` lets the helper's message stand — no "failed to run ..."
        // wrapper that would mislabel a clean non-zero exit (git ran fine, it
        // just refused).
        let args = state.finalize_invocation().ok_or_else(|| {
            anyhow::anyhow!(
                "aic cannot finalize a {} state in v1; resolve manually \
                 (e.g. `git rebase --continue`)",
                state.label()
            )
        })?;
        run_git(args, None, &[("GIT_EDITOR", "true")])?;
        Ok(())
    }

    /// Feature #2 commit guard. Aborts before any commit when the repo is
    /// mid-operation or a staged file still contains conflict markers. Called
    /// at the top of [`Git::commit`] so every commit path inherits it.
    ///
    /// The scan covers the index at call time. Hooks run *after* this guard
    /// (that is the point of #19) and can re-stage files the scan never saw —
    /// `Git::commit` re-verifies the landed tree (`verify_commit_clean`) to
    /// close that window.
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

        // Scan the staged *blobs*, not the working-tree files. The tree `git
        // commit` writes is built from the index, so a marker in a staged blob
        // is what would ship — even if the worktree was since cleaned without
        // re-staging. Reading the blob matches the real commit payload and the
        // documented contract
        // ("scans staged file contents for markers").
        let index = repo.index().context("failed to get repository index")?;
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
            // Stage-0 entry = the resolved/staged version. A conflicted repo is
            // already caught above, so any staged path here has a stage-0 entry.
            let Some(idx_entry) = index.get_path(Path::new(&path), 0) else {
                continue;
            };
            let blob = repo
                .find_blob(idx_entry.id)
                .with_context(|| format!("failed to read staged blob for {path}"))?;
            let content = String::from_utf8_lossy(blob.content());
            if has_conflict_markers(&content) {
                anyhow::bail!(
                    "cannot commit: staged {path} still contains conflict markers; \
                     run `aic resolve` or re-stage after editing"
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
pub(crate) mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::path::PathBuf;

    pub(crate) struct CwdGuard {
        original: PathBuf,
    }

    impl CwdGuard {
        pub(crate) fn new(dir: &Path) -> Self {
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

    pub(crate) fn init_test_repo(dir: &Path) {
        let repo = Repository::init(dir).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "test").unwrap();
        config.set_str("user.email", "test@test.com").unwrap();
        // Pin line-ending handling so test repos are deterministic across
        // platforms: Windows defaults to core.autocrlf=true, which rewrites
        // working-tree files to CRLF on checkout and breaks LF assertions
        // (e.g. e2e::resolve::resolve_finalizes_cherry_pick_sequence).
        config.set_str("core.autocrlf", "false").unwrap();

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
    pub(crate) static GIT_CWD_MUTEX: Mutex<()> = Mutex::new(());
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
        assert_eq!(patch.hunks.len(), 2);
        assert!(patch.hunks[0].starts_with("@@ -1,3 +1,3 @@"));
        assert!(patch.hunks[1].starts_with("@@ -10,3 +10,3 @@ fn b"));
    }

    /// Core of the hunk-split feature: stage only some of a file's hunks via
    /// `git apply --cached`, then confirm the index holds exactly those hunks.
    #[test]
    fn stage_hunks_stages_only_selected_hunk() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        // A base with two change sites far enough apart that git emits two
        // separate hunks (>= ~6 unchanged lines between them).
        let pad: String = (0..8).map(|i| format!("pad{i}\n")).collect();
        let base = format!("a0\n{pad}c0\n");
        let changed = format!("a1\n{pad}c1\n");
        std::fs::write(dir.path().join("tracked.txt"), &base).unwrap();
        {
            let _g = CwdGuard::new(dir.path());
            Git::add(&["tracked.txt"]).unwrap();
            Git::commit("base".into(), None).unwrap();
        }
        std::fs::write(dir.path().join("tracked.txt"), &changed).unwrap();

        let _g = CwdGuard::new(dir.path());
        let raw = Git::diff_workdir(Some("tracked.txt")).unwrap();
        let patch = parse_file_patch(&raw);
        assert_eq!(patch.hunks.len(), 2, "expected two separate hunks");

        // Stage ONLY hunk 1; hunk 2 must stay unstaged.
        Git::stage_hunks(&raw, &[1]).unwrap();
        let staged = Git::diff(Some("tracked.txt")).unwrap();
        assert!(
            staged.contains("a1"),
            "staged index must include hunk 1 (a1)"
        );
        assert!(
            !staged.contains("c1"),
            "staged index must NOT include hunk 2 (c1)"
        );

        // Now stage hunk 2 as well and confirm both are present.
        Git::stage_hunks(&raw, &[2]).unwrap();
        let staged = Git::diff(Some("tracked.txt")).unwrap();
        assert!(staged.contains("a1") && staged.contains("c1"));
    }

    #[test]
    fn run_git_delivers_stdin_and_captures_stdout() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        let _guard = CwdGuard::new(dir.path());

        // `git hash-object --stdin` hashes stdin and prints the object id to
        // stdout — proves stdin delivery and stdout capture in one shot. The
        // expected value is the independent blob hash of `hello\n`.
        let out = run_git(&["hash-object", "--stdin"], Some("hello\n"), &[]).unwrap();
        assert_eq!(out.trim(), "ce013625030ba8dba906f756967f9e9ca394464a");
    }

    #[test]
    fn run_git_applies_env() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        let _guard = CwdGuard::new(dir.path());

        // `git var GIT_AUTHOR_IDENT` honors GIT_AUTHOR_NAME from the
        // environment, proving the env passthrough reaches the child.
        let out = run_git(
            &["var", "GIT_AUTHOR_IDENT"],
            None,
            &[("GIT_AUTHOR_NAME", "Ada")],
        )
        .unwrap();
        assert!(out.starts_with("Ada <"), "got: {}", out);
    }

    #[test]
    fn run_git_surfaces_git_stderr_on_failure() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        let _guard = CwdGuard::new(dir.path());

        // A well-formed patch for a file the index doesn't have: git refuses
        // with its own diagnostic. The error must carry that stderr plus the
        // exit status — never a bare exit code.
        let patch = "diff --git a/nope.txt b/nope.txt\n\
                     index 0000000..3b18e51 100644\n\
                     --- a/nope.txt\n\
                     +++ b/nope.txt\n\
                     @@ -0,0 +1 @@\n\
                     +hello\n";
        let err = run_git(&["apply", "--cached", "-"], Some(patch), &[])
            .expect_err("git apply must reject a patch for a missing file");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("does not exist in index"),
            "git's real stderr must be surfaced: {msg}"
        );
        // ExitStatus renders as "exit status: N" on Unix and "exit code: N"
        // on Windows (a std::os Display difference), so accept either — the
        // point is that the non-zero exit info is surfaced, never a bare
        // message.
        assert!(
            msg.contains("exit status") || msg.contains("exit code"),
            "exit status must be surfaced: {msg}"
        );
    }

    #[test]
    fn diff_workdir_returns_untracked_content() {
        let _lock = GIT_CWD_MUTEX.lock();
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
        let _lock = GIT_CWD_MUTEX.lock();
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
        let _lock = GIT_CWD_MUTEX.lock();
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
        let _lock = GIT_CWD_MUTEX.lock();
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
        let _lock = GIT_CWD_MUTEX.lock();
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
        let _lock = GIT_CWD_MUTEX.lock();
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
            Some(&["commit", "--no-edit"][..])
        );
        assert_eq!(
            RepoState::CherryPick.finalize_invocation(),
            Some(&["cherry-pick", "--continue"][..])
        );
        assert_eq!(
            RepoState::Revert.finalize_invocation(),
            Some(&["revert", "--continue"][..])
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
    pub(crate) fn make_content_conflict(dir: &Path) {
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
        let _lock = GIT_CWD_MUTEX.lock();
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
        let _lock = GIT_CWD_MUTEX.lock();
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
        let _lock = GIT_CWD_MUTEX.lock();
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

    /// Regression: the guard must scan the staged *blob*, not the worktree
    /// file. A user can `git add` a marker-laden file then clean the worktree
    /// without re-staging — the index (what gets committed) still has markers,
    /// but the worktree file is clean. Reading the worktree would let the
    /// marker-laden blob ship; reading the index blob catches it.
    #[test]
    fn assert_commit_safe_scans_index_blob_not_worktree() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        // Stage a marker-laden version of a tracked file.
        std::fs::write(
            dir.path().join("tracked.txt"),
            "<<<<<<< HEAD\nmine\n=======\nyours\n>>>>>>> branch\n",
        )
        .unwrap();

        let _guard = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).unwrap();

        // Clean the worktree WITHOUT re-staging: index still holds the markers.
        std::fs::write(dir.path().join("tracked.txt"), "clean\n").unwrap();

        let err = Git::assert_commit_safe()
            .expect_err("guard must read the staged blob, not the worktree");
        assert!(
            format!("{err:#}").contains("conflict markers"),
            "expected marker message from staged blob, got: {err:#}"
        );
    }

    /// Make a hook file executable so git will actually invoke it. git skips a
    /// non-executable hook silently, which would mask a regression in the
    /// shell-out path. No-op off Unix — Windows runs hooks through `sh`
    /// without checking the exec bit.
    #[cfg(unix)]
    fn make_hook_executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt as _;
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }
    #[cfg(not(unix))]
    fn make_hook_executable(_path: &Path) {}

    /// Install an executable hook script at `.git/hooks/<name>`. Shared by the
    /// git.rs unit tests and the e2e suite (issue #20), which both need a real
    /// hook the `git commit` shell-out will execute.
    pub(crate) fn install_hook(dir: &Path, name: &str, body: &str) {
        let hooks_dir = dir.join(".git/hooks");
        fs::create_dir_all(&hooks_dir).unwrap();
        let hook = hooks_dir.join(name);
        fs::write(&hook, format!("#!/bin/sh\n{body}\n")).unwrap();
        make_hook_executable(&hook);
    }

    /// The core claim of issue #19 (AC#2): a normal Run commits through the
    /// real `git commit`, so all three commit hooks — `pre-commit`,
    /// `prepare-commit-msg`, and `commit-msg` — run. libgit2 never runs hooks,
    /// so this test is red under the old implementation and green only once
    /// `Git::commit` shells out.
    #[test]
    fn commit_runs_pre_commit_and_commit_msg_hooks() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        // pre-commit drops a sentinel file — proves the hook executed.
        install_hook(dir.path(), "pre-commit", "echo ran > sentinel.txt");
        // prepare-commit-msg drops its own sentinel — proves it ran too. It
        // receives `$1` (message file) and `$2` (source = "message" for `-F`);
        // we only need the side effect.
        install_hook(
            dir.path(),
            "prepare-commit-msg",
            "echo ran > prepare-ran.txt",
        );
        // commit-msg appends a trailer to the message file ($1) — proves the
        // hook executed AND its edit survived into the landed commit.
        install_hook(
            dir.path(),
            "commit-msg",
            "echo 'Signed-off-by: aic-test' >> \"$1\"",
        );

        std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
        let _g = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).unwrap();

        let hash = Git::commit("chore: hook test".into(), None).unwrap();

        // pre-commit ran → sentinel exists.
        assert!(
            dir.path().join("sentinel.txt").exists(),
            "pre-commit hook must run during Git::commit"
        );
        // prepare-commit-msg ran → its sentinel exists.
        assert!(
            dir.path().join("prepare-ran.txt").exists(),
            "prepare-commit-msg hook must run during Git::commit"
        );
        // commit-msg ran → its trailer is in the committed message.
        let msg = run_git(&["log", "-1", "--pretty=%B"], None, &[]).unwrap();
        assert!(
            msg.contains("Signed-off-by: aic-test"),
            "commit-msg hook must run during Git::commit; got message:\n{msg}"
        );
        // The authored subject survives alongside the hook's trailer.
        assert!(msg.contains("chore: hook test"));
        // Hash still the 7-char HEAD prefix.
        assert_eq!(hash.len(), 7);
    }

    /// The returned short hash must be exactly the first 7 hex chars of the new
    /// HEAD — the format the libgit2 path returned (`oid.to_string()[..7]`).
    #[test]
    fn commit_returns_seven_char_head_prefix() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
        let _g = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).unwrap();

        let hash = Git::commit("chore: hash format".into(), None).unwrap();

        let full = run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();
        assert_eq!(hash, &full.trim()[..7]);
        assert!(hash.chars().all(|c| c.is_ascii_hexdigit()));
    }

    /// A drafted body must land verbatim in the committed message — the
    /// `--cleanup=verbatim` flag keeps git from collapsing consecutive blank
    /// lines or stripping trailing whitespace (the default `strip` cleanup
    /// does both). The body also carries a `#`-prefixed line, which git treats
    /// as commentary in interactive messages.
    #[test]
    fn commit_preserves_authored_body() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
        let _g = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).unwrap();

        let hash = Git::commit(
            "fix: subject".into(),
            Some(
                "explanation line\n\n\nsecond paragraph\n\n# a comment-looking line\nline with trailing spaces  "
                    .into(),
            ),
        )
        .unwrap();
        assert_eq!(hash.len(), 7);

        let msg = run_git(&["log", "-1", "--pretty=%B"], None, &[]).unwrap();
        // Byte-for-byte: the blank run, the `#` line, and the trailing
        // whitespace all survive; git appends the closing newline.
        assert_eq!(
            msg,
            "fix: subject\n\nexplanation line\n\n\nsecond paragraph\n\n# a comment-looking line\nline with trailing spaces  \n"
        );
    }

    /// The enforcement half of #19's AC2: a hook that vetoes the commit
    /// aborts `Git::commit`, surfaces the hook's own stderr, and lands
    /// nothing. (The happy-path test above proves hooks run; this proves a
    /// refusal is reported, not swallowed.)
    #[test]
    fn commit_vetoed_by_hook_surfaces_stderr_and_lands_nothing() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        install_hook(
            dir.path(),
            "pre-commit",
            "echo 'blocked by policy' >&2; exit 1",
        );

        std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
        let _g = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).unwrap();
        let before = run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();

        let err = Git::commit("chore: vetoed".into(), None)
            .expect_err("a vetoing pre-commit hook must abort Git::commit");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("blocked by policy"),
            "the hook's stderr must surface, got: {msg}"
        );

        let after = run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();
        assert_eq!(before, after, "a vetoed commit must not move HEAD");
    }

    /// With nothing staged, `git commit` refuses instead of silently landing
    /// an empty commit (libgit2 created one). aic always stages before
    /// committing, so this only fires on a state that must not be committed —
    /// and the refusal carries git's own message.
    #[test]
    fn commit_refuses_when_nothing_staged() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());
        let _g = CwdGuard::new(dir.path());

        let err = Git::commit("chore: nothing".into(), None)
            .expect_err("git must refuse a commit with nothing staged");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("nothing to commit"),
            "git's refusal must surface, got: {msg}"
        );
        let count = run_git(&["rev-list", "--count", "HEAD"], None, &[]).unwrap();
        assert_eq!(count.trim(), "1", "no commit may land");
    }

    /// The husky / lint-staged flow this PR exists for: a `pre-commit` hook
    /// edits a file and re-stages it; the landed commit must contain the
    /// hook's change. (The other hook tests prove hooks *run*; this proves
    /// hook-staged content *ships*.)
    #[test]
    fn commit_includes_hook_staged_changes() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        // lint-staged-style: rewrite a file and stage it from the hook.
        install_hook(
            dir.path(),
            "pre-commit",
            "echo 'auto-fixed by hook' > hook-fixed.txt\ngit add hook-fixed.txt",
        );

        std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
        let _g = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).unwrap();

        let hash = Git::commit("chore: hook staged".into(), None).unwrap();
        assert_eq!(hash.len(), 7);

        let content = run_git(&["show", "HEAD:hook-fixed.txt"], None, &[]).unwrap();
        assert_eq!(content.trim(), "auto-fixed by hook");
    }

    /// The enforcement half of the hook window: a `pre-commit` hook re-stages
    /// a file that holds conflict markers — content the pre-commit guard never
    /// scanned (it runs before hooks). The post-commit tree scan catches it:
    /// the commit landed, but `Git::commit` reports the violation, names the
    /// file, and offers the recovery path instead of shipping silently.
    #[test]
    fn commit_reports_markers_staged_by_hook() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        install_hook(
            dir.path(),
            "pre-commit",
            "printf '<<<<<<< ours\\nbad\\n>>>>>>> theirs\\n' > sneaky.txt\ngit add sneaky.txt",
        );

        std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
        let _g = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).unwrap();
        let before = run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();

        let err = Git::commit("chore: sneaky".into(), None)
            .expect_err("hook-staged conflict markers must be reported");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("sneaky.txt"),
            "the marked file must be named, got: {msg}"
        );
        assert!(
            msg.contains("commit landed"),
            "the error must say the commit exists, got: {msg}"
        );
        assert!(
            msg.contains("amend"),
            "the error must offer the recovery path, got: {msg}"
        );

        let after = run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();
        assert_ne!(before, after, "the commit landed despite the markers");
    }

    /// `git commit -F -` refuses an empty message ("empty commit message")
    /// where libgit2 created the commit — a silent empty-subject commit is
    /// worse than a loud refusal. aic's message comes from the LLM, so an
    /// empty string is a realistic input.
    #[test]
    fn commit_refuses_empty_message() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        std::fs::write(dir.path().join("tracked.txt"), "changed\n").unwrap();
        let _g = CwdGuard::new(dir.path());
        Git::add(&["tracked.txt"]).unwrap();
        let before = run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();

        let err =
            Git::commit(String::new(), None).expect_err("an empty message must abort the commit");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("empty commit message"),
            "git's refusal must surface, got: {msg}"
        );

        let after = run_git(&["rev-parse", "HEAD"], None, &[]).unwrap();
        assert_eq!(before, after, "no commit may land with an empty message");
    }

    /// Regression: `diff_workdir` must return a real new-file patch for a file
    /// that lives inside an *untracked directory* (e.g. a new `src/foo/` module
    /// created by splitting a single file into a module dir). Without
    /// `recurse_untracked_dirs`, libgit2 collapses the whole untracked directory
    /// into one directory-level delta, `format_diff_for_path` finds no delta for
    /// the inner file, returns an empty patch, and `stage_hunks` later feeds
    /// `git apply --cached` an empty input → "No valid patches in input" → the
    /// whole batch aborts. This is the splitting-a-file-into-a-module case.
    #[test]
    fn diff_workdir_returns_content_for_file_in_untracked_dir() {
        let _lock = GIT_CWD_MUTEX.lock();
        let dir = tempfile::tempdir().unwrap();
        init_test_repo(dir.path());

        // An untracked directory with a file inside — mirrors `src/e2e/`.
        let new_dir = dir.path().join("mymod");
        std::fs::create_dir(&new_dir).unwrap();
        std::fs::write(
            new_dir.join("mod.rs"),
            "pub fn x() {}
",
        )
        .unwrap();

        let _guard = CwdGuard::new(dir.path());
        let result = Git::diff_workdir(Some("mymod/mod.rs")).unwrap();
        assert!(
            !result.is_empty(),
            "must produce a patch for a file in an untracked dir"
        );
        assert!(
            result.contains("--- /dev/null"),
            "must be a new-file patch; got:\n{result}"
        );
        // The rebuilt patch must actually apply to the index.
        let patch = parse_file_patch(&result);
        assert!(!patch.hunks.is_empty(), "must have at least one hunk");
        run_git(&["apply", "--cached", "-"], Some(&result), &[])
            .expect("rebuilt new-file patch must apply to the index");
    }
}
