//! The Undo module — `aic undo`, one command that reverts the last commit
//! Run (CONTEXT.md "Undo"). The Run flow records the HEAD OID captured
//! before a Run's first commit ([`record`]); this module reads it back,
//! refuses anything it cannot prove safe, confirms with the user, and runs
//! `git reset --mixed` so every change the Run committed returns to the
//! working tree unstaged — the exact pre-Run state, one command away.
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
/// and is per-repo by construction.
const STATE_FILE: &str = "aic/undo-run";

fn state_path(git: &Git) -> PathBuf {
    git.git_dir().join(STATE_FILE)
}

/// Record a Run start for a later `aic undo`. Called before each of the
/// Run's commits with the same start OID — the rewrite is idempotent, and
/// recording *before* the first commit (not after the Run) means a Run that
/// aborts mid-loop is still fully undoable for the batches that landed.
pub(crate) fn record(git: &Git, start_sha: &str) -> anyhow::Result<()> {
    let path = state_path(git);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(&path, start_sha).with_context(|| format!("failed to write {}", path.display()))
}

/// Read the recorded Run start, or `None` when no Run has been recorded in
/// this repo. A malformed file (truncated write, hand edit) is an error, not
/// a silent `None` — the user should know their undo anchor is unreadable.
fn read_state(git: &Git) -> anyhow::Result<Option<String>> {
    let path = state_path(git);
    match std::fs::read_to_string(&path) {
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

/// The undo core: read the recorded Run start, prove it still safe (HEAD
/// descends from it; at least one commit to undo), confirm, then
/// `git reset --mixed` back and clear the state. `prompt` answers the
/// one y/n gate so tests can script it.
pub(crate) fn undo_run(
    git: &Git,
    prompt: &dyn Fn(&str) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let Some(start) = read_state(git)? else {
        anyhow::bail!(
            "no aic run to undo — either no run has committed in this repo yet, \
             or the run predates undo support"
        );
    };

    // Ancestry proof: if HEAD no longer descends from the recorded start
    // (a rebase, filter-branch, or reset rewrote the baseline itself),
    // resetting would orphan commits the user never meant to hand back.
    // Refuse and point at the reflog — the manual recovery path.
    if !git.is_ancestor_of_head(&start)? {
        anyhow::bail!(
            "history has been rewritten past the last aic run; refusing to undo. \
             recover manually with `git reflog` if needed"
        );
    }

    let count = git
        .commit_count(&start, "HEAD")
        .with_context(|| "failed to count the last aic run's commits")?;
    if count == 0 {
        // The Run recorded its start but never landed a commit (a staging
        // or hook failure between record and commit). Clear the stale
        // record so the next `aic undo` isn't stuck on it.
        let _ = std::fs::remove_file(state_path(git));
        anyhow::bail!("the last aic run made no commits — nothing to undo (stale state cleared)");
    }

    let approved = prompt(
        "undo this aic run? its commits are reset and all their changes return to \
         the working tree unstaged (anything currently staged also becomes unstaged)",
    )?;
    if !approved {
        anyhow::bail!("aborted — nothing was changed");
    }

    git.reset_mixed(&start)
        .with_context(|| "failed to reset back to the last aic run's start")?;
    std::fs::remove_file(state_path(git))
        .with_context(|| "undone, but the undo state file could not be removed")?;
    println!("undone — run reset; all its changes are back in the working tree, unstaged");
    Ok(())
}

/// Production entry point for `aic undo` — discover the repo around the
/// process CWD and run the undo core with the real y/n prompt.
pub fn run_undo() -> anyhow::Result<()> {
    let git = Git::at(Path::new("."))?;
    undo_run(&git, &|label| input::prompt_yes_no(label))
}
