//! The git surface: repository discovery, the CLI/libgit2 reconciliation
//! (`run_git`), staging, and commit authoring. Status listing lives in
//! [`status`]; diff computation and formatting in [`diff_view`].

use crate::git::diff::parse_file_patch;
use anyhow::Context;
use git2::{ObjectType, Repository, Status, Tree};
use std::path::Path;
use std::process::{Command, ExitStatus, Stdio};

mod diff_view;
mod status;

pub mod conflict;
pub mod diff;
pub mod diff_json;
pub mod staging;

#[cfg(test)]
pub(crate) mod tests;

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

/// Per-file stats for a commit entry's footer: added/deleted line counts,
/// new/removed status, and binary-ness for one path in one diff. Produced by
/// [`Git::staged_stats`] (what a pending commit would land) and
/// [`Git::committed_stats`] (what a landed commit did land); rendered by
/// `display::Display::emit_file_stats`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStats {
    pub path: String,
    pub added: usize,
    pub deleted: usize,
    /// File is new in this diff (git `A`) — renders `[new]`.
    pub new: bool,
    /// File is deleted in this diff (git `D`) — renders `[del]`.
    pub removed: bool,
    /// Binary delta: no line counts exist; renders `(binary)`.
    pub binary: bool,
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

pub struct Git {
    repo: Repository,
}
impl Git {
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
    /// The `git2::Repository` handle this `Git` owns — the single source of
    /// truth the libgit2/CLI duality reconciles (see `index`'s force-refresh).
    /// `pub(crate)` so the conflict module (reached via [`Self::conflict`]) can
    /// read state and the workdir without rediscovering the repo. ADR-0006.
    pub(crate) fn repo(&self) -> &Repository {
        &self.repo
    }

    /// The conflict-resolution surface over this handle — see `crate::git::conflict`.
    pub fn conflict(&self) -> conflict::Conflict<'_> {
        conflict::Conflict::new(self)
    }

    pub(crate) fn run_git(
        &self,
        args: &[&str],
        stdin: Option<&str>,
        envs: &[(&str, &str)],
    ) -> anyhow::Result<String> {
        let command_line = format!("git {}", args.join(" "));
        let mut cmd = Command::new("git");
        // Operate on the repo this handle discovered — never the process CWD.
        if let Some(workdir) = self.repo.workdir() {
            cmd.current_dir(workdir);
        }
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

    /// Discover the repository containing `path` once; every method operates
    /// on this handle, so repo identity never depends on the process CWD
    /// (which used to force tests to chdir and serialize on a global lock).
    pub fn at(path: &Path) -> anyhow::Result<Self> {
        Ok(Self {
            repo: Repository::discover(path).with_context(|| {
                format!("failed to discover git repository at {}", path.display())
            })?,
        })
    }

    /// The repo's index, force-refreshed from disk.
    ///
    /// The git CLI (`git apply --cached`, `git add`, hook re-staging) rewrites
    /// the index file behind libgit2's back, and libgit2 caches the index on
    /// the `Repository` handle — so without a forced re-read, the second half
    /// of a Run would diff against a stale index and `git apply --cached`
    /// would reject the remapped hunks ("patch does not apply"). The old
    /// per-call `discover(".")` got a fresh index every time; the shared
    /// handle needs this instead.
    ///
    /// `pub(crate)` so the conflict module's `conflicted_files` shares the one
    /// refresh strategy — see ADR-0006.
    pub(crate) fn index(&self) -> anyhow::Result<git2::Index> {
        let mut index = self
            .repo
            .index()
            .context("failed to get repository index")?;
        index
            .read(true)
            .context("failed to refresh repository index")?;
        Ok(index)
    }

    pub fn add(&self, paths: &[&str]) -> anyhow::Result<()> {
        let repo = &self.repo;
        let mut index = self.index()?;

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
    pub fn stage_hunks(&self, raw_diff: &str, hunk_indices: &[usize]) -> anyhow::Result<()> {
        let patch = parse_file_patch(raw_diff);
        let body = patch.slice(hunk_indices)?;

        // Name the aic operation the user can act on; the helper's cause layer
        // carries the command and git's own stderr.
        self.run_git(&["apply", "--cached", "-"], Some(&body), &[])
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
    /// Returns git's abbreviated hash (`git rev-parse --short`) for the new
    /// HEAD — honoring `core.abbrev` and matching what `git log --oneline`
    /// shows, rather than slicing a fixed width ourselves. The value is the
    /// commit git just created: commit hooks run before
    /// the commit object is written, so HEAD is that commit (only a hook that
    /// itself commits could move HEAD further — and then the displayed hash is
    /// the state the user actually sees). If resolving HEAD fails after the
    /// commit landed, the error says so instead of implying the commit failed.
    pub fn commit(&self, message: String, body: Option<String>) -> anyhow::Result<String> {
        self.assert_commit_safe()?;
        let full_message = match body {
            Some(b) => format!("{message}\n\n{b}"),
            None => message,
        };
        self.run_git(
            &["commit", "-F", "-", "--cleanup=verbatim"],
            Some(&full_message),
            &[],
        )?;
        // git's abbreviated hash for the new HEAD (`git rev-parse --short`),
        // honoring `core.abbrev` and matching what `git log --oneline` shows.
        // Default abbreviation is 7 hex chars but git may extend it for repo
        // size or to guarantee uniqueness, so we defer to git rather than
        // slicing a fixed width ourselves.
        let short = self
            .run_git(&["rev-parse", "--short", "HEAD"], None, &[])
            .with_context(|| "commit landed, but the new HEAD could not be resolved")?;
        let short = short.trim();
        if short.is_empty() {
            anyhow::bail!("commit landed, but `git rev-parse --short HEAD` returned no output");
        }
        // Verify what actually shipped: the pre-commit guard scanned the index
        // *before* hooks ran; a hook that re-staged content is not in that
        // scan. Check the landed tree so marker-bearing hook output surfaces
        // instead of silently shipping.
        self.verify_commit_clean()?;
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
    fn verify_commit_clean(&self) -> anyhow::Result<()> {
        let repo = &self.repo;
        let head = repo.head().context("failed to resolve HEAD after commit")?;
        let tree = head.peel_to_tree().context("failed to read HEAD tree")?;
        let mut marked = Vec::new();
        self.collect_marked_paths(&tree, "", &mut marked)?;
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
        &self,
        tree: &Tree,
        prefix: &str,
        out: &mut Vec<String>,
    ) -> anyhow::Result<()> {
        for entry in tree.iter() {
            let path = format!("{prefix}{}", entry.name().unwrap_or("?"));
            match entry.kind() {
                Some(ObjectType::Tree) => {
                    let sub = entry
                        .to_object(&self.repo)
                        .with_context(|| format!("failed to read subtree {path}"))?
                        .peel_to_tree()
                        .context("failed to peel subtree to tree")?;
                    self.collect_marked_paths(&sub, &format!("{path}/"), out)?;
                }
                Some(ObjectType::Blob) => {
                    let blob = entry
                        .to_object(&self.repo)
                        .with_context(|| format!("failed to read blob {path}"))?
                        .peel_to_blob()
                        .context("failed to peel blob object")?;
                    let content = String::from_utf8_lossy(blob.content());
                    if conflict::has_conflict_markers(&content) {
                        out.push(path);
                    }
                }
                _ => {}
            }
        }
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
    pub fn assert_commit_safe(&self) -> anyhow::Result<()> {
        let repo = &self.repo;
        // Conflict detection lives behind the conflict seam; this guard is
        // commit-time *policy* that acts on it (ADR-0006).
        let state = self.conflict().state()?;
        if state.is_conflicted() {
            // rebase/am: `aic resolve` refuses these (ADR 0005) — name the
            // manual continuation instead of redirecting to a closed door.
            let hint = state.manual_finalize_command().map_or_else(
                || "run `aic resolve` or finalize manually".to_string(),
                |args| format!("finish it with `git {}`", args.join(" ")),
            );
            anyhow::bail!(
                "repo is mid-{} (unresolved conflict state); {hint}",
                state.label()
            );
        }

        // Scan the staged *blobs*, not the working-tree files. The tree `git
        // commit` writes is built from the index, so a marker in a staged blob
        // is what would ship — even if the worktree was since cleaned without
        // re-staging. Reading the blob matches the real commit payload and the
        // documented contract
        // ("scans staged file contents for markers").
        let index = self.index()?;
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
            if conflict::has_conflict_markers(&content) {
                anyhow::bail!(
                    "cannot commit: staged {path} still contains conflict markers; \
                     run `aic resolve` or re-stage after editing"
                );
            }
        }
        Ok(())
    }
}
