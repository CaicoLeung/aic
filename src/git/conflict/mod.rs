//! Conflict resolution primitives (ADR 0005) — the domain aic resolves and
//! finalizes: `RepoState` classification, conflicted-file detection, worktree
//! I/O for conflicted files, Finalize, and the resolve-flow status UI (the
//! free functions over [`crate::render::display::Display`] at the bottom of this
//! file), which lives here next to the domain instead of on the commit
//! panel.
//! Reached as [`Git::conflict`] → [`Conflict`], which borrows the repo handle
//! `Git` owns (`run_git`, `index`, and the `Repository` itself). The commit
//! guards (`assert_commit_safe`, `verify_commit_clean`) stay on `Git`: they
//! cross this seam for *conflict detection* (`state()`,
//! `has_conflict_markers`) but keep their own git2 blob/tree scans on `Git`
//! via its `pub(crate) repo()` — so the seam moves detection, not every git2
//! call. They are commit-time *policy*; this module owns the *detection* they
//! act on. See CONTEXT.md "Conflict module" and ADR-0006 for why `Git` itself
//! is not split further.

use anyhow::Context;
use git2::Repository;
use std::fs;

use crate::git::Git;

// ----------------------------------------------------------------------
// Size caps
// ----------------------------------------------------------------------

/// Maximum worktree-file size we will hand to the LLM for resolution. Files at
/// or above this are skipped as oversized.
pub const MAX_CONFLICT_BYTES: usize = 50 * 1024; // 50 KB
pub const MAX_CONFLICT_LINES: usize = 2000;

// ----------------------------------------------------------------------
// RepoState
// ----------------------------------------------------------------------

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
    fn from_git2(state: git2::RepositoryState) -> Self {
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

    /// The git command a *user* runs to finalize a state aic refuses to
    /// finalize (rebase / am — v1, ADR 0005), as args for `git`. `None` for
    /// states aic finalizes itself and for non-conflict states. The single
    /// source for the "resolve manually" hints — display derives its hint
    /// text from this, never re-mirrors it.
    pub fn manual_finalize_command(&self) -> Option<&'static [&'static str]> {
        match self {
            Self::Rebase | Self::RebaseInteractive | Self::RebaseMerge => {
                Some(&["rebase", "--continue"][..])
            }
            Self::ApplyMailbox | Self::ApplyMailboxOrRebase => Some(&["am", "--continue"][..]),
            _ => None,
        }
    }
}

// ----------------------------------------------------------------------
// ConflictKind
// ----------------------------------------------------------------------

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

    /// The size detail line for an oversized file, or `None` for every other
    /// kind. Display renders it (dimmed) under the file's summary row; the
    /// content is size-cap policy and stays behind the interface.
    pub fn size_note(&self) -> Option<String> {
        match self {
            Self::Oversized { bytes, lines } => {
                Some(format!("{bytes} bytes, {lines} lines (> cap)"))
            }
            _ => None,
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

// ----------------------------------------------------------------------
// Conflict — the deep module over the repo handle Git owns
// ----------------------------------------------------------------------

/// Conflict-resolution operations over the repo handle `Git` owns. Built by
/// [`Git::conflict`]; borrows the handle, so it cannot outlive its `Git`.
/// Every method crosses the same seam the resolve workflow and the commit
/// guards do — the test surface is this interface.
pub struct Conflict<'a> {
    git: &'a Git,
}

impl<'a> Conflict<'a> {
    /// `Git::conflict` is the only constructor — `Conflict` is never built
    /// independently of the handle that owns the repo.
    pub(crate) fn new(git: &'a Git) -> Self {
        Self { git }
    }

    pub fn state(&self) -> anyhow::Result<RepoState> {
        Ok(RepoState::from_git2(self.git.repo().state()))
    }

    /// Every unmerged path in the index, classified by whether the LLM can
    /// resolve it. Driven by `Index::conflicts()` (ancestor/our/their stages).
    pub fn conflicted_files(&self) -> anyhow::Result<Vec<ConflictedFile>> {
        let repo = self.git.repo();
        let index = self.git.index()?;
        let mut out = Vec::new();
        for conflict in index.conflicts()? {
            let c = conflict.context("failed to read index conflict")?;
            let path = conflict_path(&c);
            let kind = match (c.our.as_ref(), c.their.as_ref()) {
                (None, _) | (_, None) => ConflictKind::DeleteModify,
                _ => classify_worktree(repo, &path),
            };
            out.push(ConflictedFile { path, kind });
        }
        Ok(out)
    }

    /// Read the current working-tree bytes for a path (the file git wrote
    /// conflict markers into).
    pub fn read_worktree(&self, path: &str) -> anyhow::Result<Vec<u8>> {
        let workdir = self
            .git
            .repo()
            .workdir()
            .context("repository has no working directory")?;
        fs::read(workdir.join(path)).with_context(|| format!("failed to read worktree file {path}"))
    }

    /// Overwrite a working-tree file with resolved content (called only after
    /// the user approves the resolution).
    pub fn write_worktree(&self, path: &str, content: &str) -> anyhow::Result<()> {
        let workdir = self
            .git
            .repo()
            .workdir()
            .context("repository has no working directory")?;
        fs::write(workdir.join(path), content)
            .with_context(|| format!("failed to write resolved file {path}"))?;
        Ok(())
    }

    /// Finalize a resolved conflict state by shelling out to git. `GIT_EDITOR`
    /// is set to `true` so `--continue` / `commit --no-edit` never block on an
    /// editor and git's default message is kept verbatim (ADR 0005).
    pub fn finalize(&self, state: RepoState) -> anyhow::Result<()> {
        // A bare `?` lets the helper's message stand — no "failed to run ..."
        // wrapper that would mislabel a clean non-zero exit (git ran fine, it
        // just refused).
        let args = state.finalize_invocation().ok_or_else(|| {
            let hint = state
                .manual_finalize_command()
                .map(|args| format!(" (e.g. `git {}`)", args.join(" ")))
                .unwrap_or_default();
            anyhow::anyhow!(
                "aic cannot finalize a {} state in v1; resolve manually{hint}",
                state.label()
            )
        })?;
        self.git.run_git(args, None, &[("GIT_EDITOR", "true")])?;
        Ok(())
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

// ----------------------------------------------------------------------
// Resolve-flow UI (ADR 0005)
//
// The status lines the resolve workflow paints as it walks the conflict
// domain this module already owns: detection header, conflicted-file
// summary, review diff, per-file outcomes, and the finalize hand-off.
// These are free functions over the shared [`Display`] render seam (not
// methods) so the conflict domain — not the commit panel — owns its
// vocabulary; `run.rs` and `resolve.rs` call them as `conflict::…`.
// ----------------------------------------------------------------------

use crate::render::display::Display;
use crate::render::palette;

/// Header shown when a conflicted repo state is detected.
pub(crate) fn conflict_detected(display: &Display, state: RepoState, count: usize) {
    let word = if count == 1 { "file" } else { "files" };
    display.emit(&format!(
        "{} conflicts detected — repo is mid-{} ({} {})",
        display.styled("\u{26A0}", palette::pending()),
        state.label(),
        count,
        word,
    ));
}

/// One-line prompt for the default-run auto-detect: `Resolve now? [y/n]`.
pub(crate) fn resolve_prompt(display: &Display, state: RepoState) {
    display.emit(&display.styled(
        &format!("repo is mid-{}; resolve with aic now?", state.label()),
        palette::caution(),
    ));
}

/// List conflicted files with their kind. Resolvable files are unmarked;
/// unresolvable ones carry a `(reason)` tag.
pub(crate) fn conflicted_summary(display: &Display, files: &[ConflictedFile]) {
    for f in files {
        let tag = if f.kind.resolvable() {
            String::new()
        } else {
            format!(
                " {}",
                display.styled(&format!("({})", f.kind.reason()), palette::caution())
            )
        };
        display.emit(&format!("  {}{}", f.path, tag));
        if let Some(note) = f.kind.size_note() {
            display.emit(&format!("    {}", display.styled(&note, palette::muted())));
        }
    }
    display.emit_blank();
}

/// Render one file's review diff (original worktree -> LLM resolution) with
/// its path as the header — called immediately before that file's `apply`
/// prompt, so heading, diff, and question never drift apart.
/// Coloring by leading sign so a glance distinguishes additions and context:
///   `+` addition → green, `-` deletion → red, ` ` context → dim.
pub(crate) fn review_section(display: &Display, path: &str, diff: &str) {
    display.emit(&display.styled("proposed resolution:", palette::muted()));
    display.emit(&display.styled(path, palette::header()));
    for line in diff.lines() {
        // Color is computed on the original diff line (leading +,-, or
        // space), then the shared margin is prepended by `emit` — so the
        // sign-based coloring stays correct while the whole diff block
        // sits at the uniform inset.
        let styled = match line.chars().next() {
            Some('+') => display.styled(line, palette::added()),
            Some('-') => display.styled(line, palette::removed()),
            Some(' ') => display.styled(line, palette::muted()),
            None => String::new(),
            _ => display.styled(line, palette::header()),
        };
        display.emit(&styled);
    }
    display.emit_blank();
}

/// Per-file outcome lines.
pub(crate) fn resolved(display: &Display, path: &str) {
    display.emit(&format!(
        "{} resolved + staged: {}",
        display.styled("\u{2713}", palette::success()),
        path,
    ));
}

pub(crate) fn skipped(display: &Display, path: &str, reason: &str) {
    display.emit(&format!(
        "{} skipped: {} ({})",
        display.styled("\u{26A0}", palette::caution()),
        path,
        reason,
    ));
}

pub(crate) fn rejected(display: &Display, path: &str) {
    display.emit(&format!(
        "{} rejected: {}",
        display.styled("\u{2717}", palette::removed()),
        display.styled(path, palette::muted()),
    ));
}

/// Finalize succeeded.
pub(crate) fn finalize_done(display: &Display, state: RepoState) {
    let green = palette::success();
    display.emit_blank();
    display.emit(&format!(
        "{} {} finalized",
        display.styled("\u{2713}", green.clone()),
        display.styled(state.label(), green),
    ));
}

/// Partial: approved files staged, but some files still block finalize.
/// Breaks the blockers down by kind so the user knows whether to re-run
/// `aic resolve` (rejected/failed), retry a flaky LLM call (failed), or
/// resolve a binary/oversized file by hand (unresolvable). The old single
/// "unresolved" count conflated all three.
pub(crate) fn handoff(
    display: &Display,
    approved: usize,
    rejected: usize,
    failed: usize,
    unresolvable: usize,
    state: RepoState,
) {
    display.emit_blank();
    display.emit(&format!(
        "{} {approved} resolved + staged",
        display.styled("\u{2713}", palette::success()),
    ));

    // Only list blocker categories that actually occurred.
    let mut blockers: Vec<String> = Vec::new();
    if rejected > 0 {
        blockers.push(format!("{rejected} rejected"));
    }
    if failed > 0 {
        blockers.push(format!("{failed} failed to resolve"));
    }
    if unresolvable > 0 {
        blockers.push(format!("{unresolvable} need manual resolution"));
    }
    let blocker_text = if blockers.is_empty() {
        String::from("nothing left")
    } else {
        blockers.join(", ")
    };
    display.emit(&format!(
        "{} not finalized — {blocker_text}",
        display.styled("\u{26A0}", palette::caution()),
    ));
    display.emit(&format!(
        "  {}",
        display.styled(
            "resolve the remaining files (or re-run `aic resolve`), then:",
            palette::muted(),
        ),
    ));
    display.emit(&format!(
        "    {}",
        display.styled(&finalize_hint(state), palette::hint())
    ));
}

/// `aic resolve` on a clean repo.
pub(crate) fn no_conflicts(display: &Display) {
    display.emit(&display.styled("no conflicts — nothing to resolve", palette::muted()));
}

/// `aic resolve` on a rebase/am state — detected but refused in v1.
pub(crate) fn refused(display: &Display, state: RepoState) {
    display.emit(&format!(
        "{} cannot resolve a {} state in v1",
        display.styled("\u{2717}", palette::failure()),
        state.label(),
    ));
    display.emit(&format!(
        "  resolve manually, then run {}",
        display.styled(&finalize_hint(state), palette::hint()),
    ));
}

/// State is conflicted but the index has no unmerged entries — the user
/// resolved everything by hand and just needs the finalize step.
pub(crate) fn all_resolved_offer_finalize(display: &Display, state: RepoState) {
    display.emit(&display.styled(
        "no unmerged files remain — conflicts already resolved manually",
        palette::muted(),
    ));
    display.emit(&format!(
        "  finalize with {}",
        display.styled(&finalize_hint(state), palette::hint()),
    ));
}

/// The git command a user runs to finalize a state by hand, for hand-off /
/// refuse messages. Derived from [`RepoState::finalize_invocation`] (what aic
/// runs) and [`RepoState::manual_finalize_command`] (what the user runs for
/// states aic refuses) — one mapping, no hand-mirroring.
fn finalize_hint(state: RepoState) -> String {
    if state == RepoState::Clean {
        // Unreachable in practice: hints render only for conflict states.
        return "git commit".to_string();
    }
    let args = state
        .finalize_invocation()
        .or_else(|| state.manual_finalize_command())
        .expect("every conflict state has a finalize or manual command");
    format!("git {}", args.join(" "))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    /// The contract that replaced the hand-mirrored hint: every conflict state
    /// resolves to the exact command a user runs, derived from the single
    /// `RepoState` mapping (`finalize_invocation` for what aic runs,
    /// `manual_finalize_command` for what the user runs on refused states).
    #[test]
    fn finalize_hint_covers_every_state_with_the_right_command() {
        for (state, expected) in [
            (RepoState::Clean, "git commit"),
            (RepoState::Merge, "git commit --no-edit"),
            (RepoState::CherryPick, "git cherry-pick --continue"),
            (RepoState::CherryPickSequence, "git cherry-pick --continue"),
            (RepoState::Revert, "git revert --continue"),
            (RepoState::RevertSequence, "git revert --continue"),
            (RepoState::Rebase, "git rebase --continue"),
            (RepoState::RebaseInteractive, "git rebase --continue"),
            (RepoState::RebaseMerge, "git rebase --continue"),
            (RepoState::ApplyMailbox, "git am --continue"),
            (RepoState::ApplyMailboxOrRebase, "git am --continue"),
        ] {
            assert_eq!(finalize_hint(state), expected, "state {state:?}");
        }
    }

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
    ///
    /// `init_test_repo` (in `git::tests`) must have run first — this helper
    /// assumes HEAD is at the initial commit on master.
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
        let dir = tempfile::tempdir().unwrap();
        crate::git::tests::init_test_repo(dir.path());
        make_content_conflict(dir.path());

        let git = Git::at(dir.path()).unwrap();
        assert_eq!(git.conflict().state().unwrap(), RepoState::Merge);

        let files = git.conflict().conflicted_files().unwrap();
        assert_eq!(files.len(), 1, "exactly one conflicted file");
        assert_eq!(files[0].path, "tracked.txt");
        assert_eq!(files[0].kind, ConflictKind::Content);
    }
}
