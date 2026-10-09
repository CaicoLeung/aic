//! The Undo module — `aic undo`, one command that reverts the last commit
//! Run (CONTEXT.md "Undo"). The Run flow records two anchors ([`record`],
//! [`record_tip`]): the HEAD OID captured before a Run's first commit, and
//! the last commit the Run landed. This module reads them back, refuses
//! anything it cannot prove safe, discloses *exactly* what a reset would
//! strip — including commits made after the Run — confirms with the user,
//! and runs `git reset --mixed` so every change the Run committed returns to
//! the working tree unstaged — the exact pre-Run state, one command away.
//!
//! Split like the other workflows: [`undo_run`] is the seam-driven core (a
//! real [`Git`] plus a y/n prompt closure — tests script the prompt),
//! [`run_undo`] the production shell that discovers the repo and wires
//! [`crate::workflow::input::prompt_yes_no`].

use std::path::{Path, PathBuf};

use anyhow::Context;

use crate::git::Git;
use crate::workflow::input;

/// The undo state file, relative to the repo's git dir: the HEAD OID captured
/// at the start of the last Run that reached a commit. Lives under `.git/`
/// (never the worktree) so it can never be committed, diffed, or staged —
/// and is per-worktree by construction (see [`Git::git_dir`]).
const STATE_FILE: &str = "aic/undo-run";

/// The companion tip file: the OID of the last commit the recorded Run
/// landed. `start..tip` is exactly the Run's own commits; `tip..HEAD` is
/// everything made after it — the split that lets the undo confirmation
/// disclose post-Run commits instead of silently resetting them too.
/// Absent when the Run aborted before its first commit landed, or when it
/// predates tip tracking; [`undo_run`] then falls back to total-only counts.
const TIP_FILE: &str = "aic/undo-run-tip";

fn state_path(git: &Git) -> PathBuf {
    git.git_dir().join(STATE_FILE)
}

fn tip_path(git: &Git) -> PathBuf {
    git.git_dir().join(TIP_FILE)
}

/// Record a Run start for a later `aic undo`. Called before each of the
/// Run's commits with the same start OID — the rewrite is idempotent, and
/// recording *before* the first commit (not after the Run) means a Run that
/// aborts mid-loop is still fully undoable for the batches that landed.
/// Any tip file from an earlier Run is dropped first: a stale tip must
/// never outlive its start anchor (crash windows between this and the
/// Run's first commit merely degrade undo to total-only counts).
pub(crate) fn record(git: &Git, start_sha: &str) -> anyhow::Result<()> {
    let path = state_path(git);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let _ = std::fs::remove_file(tip_path(git));
    std::fs::write(&path, start_sha).with_context(|| format!("failed to write {}", path.display()))
}

/// Record the Run's tip: the commit that just landed. Called right after
/// each of the Run's commits, so the file ends up holding the last one —
/// the boundary between the Run's commits and the user's own.
pub(crate) fn record_tip(git: &Git) -> anyhow::Result<()> {
    let sha = git.head_sha()?;
    let path = tip_path(git);
    std::fs::write(&path, &sha).with_context(|| format!("failed to write {}", path.display()))
}

/// Read one of the two undo state files, or `None` when absent. A malformed
/// file (truncated write, hand edit) is an error, not a silent `None` — the
/// user should know their undo anchor is unreadable.
fn read_anchor(path: &Path) -> anyhow::Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(content) => {
            let sha = content.trim().to_string();
            if sha.is_empty() {
                anyhow::bail!("undo state file {} is empty", path.display());
            }
            Ok(Some(sha))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("failed to read {}", path.display())),
    }
}

/// The undo core: read the recorded anchors, prove them still safe (HEAD
/// descends from both; at least one commit to undo), confirm with a prompt
/// that names every commit the reset would strip, then `git reset --mixed`
/// back and clear the state. `prompt` answers the one y/n gate so tests can
/// script it.
pub(crate) fn undo_run(
    git: &Git,
    prompt: &dyn Fn(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let Some(start) = read_anchor(&state_path(git))? else {
        anyhow::bail!(
            "no aic run to undo — either no run has committed in this repo yet, \
             or the run predates undo support"
        );
    };
    let tip = read_anchor(&tip_path(git))?;

    // Ancestry proof: if HEAD no longer descends from the recorded start
    // (a rebase, filter-branch, or reset rewrote the baseline itself), or
    // from the recorded tip (the Run's own commits were rewritten),
    // resetting would orphan commits the user never meant to hand back.
    // Refuse and point at the reflog — the manual recovery path.
    for anchor in [Some(start.as_str()), tip.as_deref()].into_iter().flatten() {
        if !git.is_ancestor_of_head(anchor)? {
            anyhow::bail!(
                "history has been rewritten past the last aic run; refusing to undo. \
                 recover manually with `git reflog` if needed"
            );
        }
    }

    // Split what the reset would strip: the Run's own commits (start..tip)
    // versus everything made after it (tip..HEAD). Without a tip the split
    // is unavailable — count the total and say so plainly.
    let (run, after) = match &tip {
        Some(tip) => {
            let run = git
                .commit_count(&start, tip)
                .with_context(|| "failed to count the last aic run's commits")?;
            let after = git
                .commit_count(tip, "HEAD")
                .with_context(|| "failed to count commits made after the last aic run")?;
            (run, after)
        }
        None => {
            let total = git
                .commit_count(&start, "HEAD")
                .with_context(|| "failed to count the last aic run's commits")?;
            (total, 0)
        }
    };
    if run + after == 0 {
        // The Run recorded its start but never landed a commit (a staging
        // or hook failure between record and commit). Clear the stale
        // records so the next `aic undo` isn't stuck on them.
        clear_state(git)
            .with_context(|| "failed to clear the stale undo state after a zero-commit run")?;
        anyhow::bail!("the last aic run made no commits — nothing to undo (stale state cleared)");
    }

    let scope = if after > 0 {
        format!(
            "{} {} will be reset — {} {} from the run, {} {} made after it —",
            run + after,
            commits(run + after),
            run,
            commits(run),
            after,
            commits(after),
        )
    } else {
        format!("{} {} from the run are reset", run, commits(run))
    };
    let approved = prompt(&format!(
        "undo this aic run? {scope} and all their changes return to the working tree \
         unstaged (anything currently staged also becomes unstaged)",
    ))?;
    if !approved {
        anyhow::bail!("aborted — nothing was changed");
    }

    git.reset_mixed(&start)
        .with_context(|| "failed to reset back to the last aic run's start")?;
    clear_state(git).with_context(|| "undone, but the undo state files could not be removed")?;
    println!("undone — run reset; all its changes are back in the working tree, unstaged");
    Ok(())
}

/// Remove both undo state files after a successful undo (or a stale,
/// zero-commit record).
fn clear_state(git: &Git) -> anyhow::Result<()> {
    for path in [state_path(git), tip_path(git)] {
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            // Already absent is fine — only the start file is guaranteed.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => Err(e).with_context(|| format!("failed to remove {}", path.display()))?,
        }
    }
    Ok(())
}

/// "commit" / "commits" for a count — the confirmation reads better with
/// real plurals than a permanent "(s)".
fn commits(n: usize) -> &'static str {
    if n == 1 { "commit" } else { "commits" }
}

/// `aic undo` mutates the repo (`git reset --mixed`) behind a single y/n
/// gate, and that gate reads raw stdin where empty input (EOF) counts as
/// approval — so a non-TTY stdin (script, CI, `< /dev/null`) would
/// auto-approve the reset, including commits the disclosure prompt never
/// got to name. Refuse up front, before any state is read: the same shape
/// as the Run's `ensure_confirm_terminal`, but unconditional — undo has no
/// configuration that could turn its gate off, so there is no second
/// flag to honor. The escape hatch for scripted callers is plain git:
/// `git reset --mixed $(cat .git/aic/undo-run)`.
fn ensure_undo_terminal(stdin_tty: bool) -> anyhow::Result<()> {
    if !stdin_tty {
        anyhow::bail!(
            "aic undo needs an interactive terminal to confirm the reset — \n\
             for a scripted equivalent, run \n\
             `git reset --mixed $(cat .git/aic/undo-run)` manually"
        );
    }
    Ok(())
}

/// Production entry point for `aic undo` — discover the repo around the
/// process CWD and run the undo core with the real y/n prompt. Refuses a
/// non-TTY stdin first ([`ensure_undo_terminal`]): the y/n gate would
/// otherwise read EOF as approval.
pub fn run_undo() -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    ensure_undo_terminal(std::io::stdin().is_terminal())?;
    let git = Git::at(Path::new("."))?;
    undo_run(&git, &|label| input::prompt_yes_no(label))
}

#[cfg(test)]
mod tests {
    use super::ensure_undo_terminal;

    /// The undo gate must refuse non-TTY stdin: the y/n prompt counts EOF
    /// as approval, so a scripted/CI invocation would silently reset the
    /// Run's commits (and any made after it). Mirrors
    /// `ensure_confirm_terminal`'s guard test.
    #[test]
    fn ensure_undo_terminal_guards_non_tty_stdin() {
        assert!(ensure_undo_terminal(true).is_ok());
        let err = ensure_undo_terminal(false).expect_err("must refuse non-TTY stdin");
        assert!(
            format!("{err:#}").contains("interactive terminal"),
            "got: {err:#}"
        );
    }
}
