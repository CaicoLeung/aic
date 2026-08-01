//! Durable run state for the batch-plan workflow.
//!
//! An interrupted Run (LLM outage, network drop, accidental Ctrl-C, a bug) used
//! to lose the planner's batch split: re-running `aic` re-analyzed everything
//! from scratch and re-split the remaining files. This module persists the
//! plan, the captured per-file diffs, a content fingerprint of every planned
//! file, and a per-batch status — so the remaining batches can be *replayed*
//! from the frozen snapshot without re-planning.
//!
//! Two artifacts live under `.aic/` in the worktree (gitignored, auto-ensured):
//!   - `active.json` — in-flight machine state. Exists only while a Run is
//!     incomplete; its presence is the resume-available signal. Deleted on
//!     clean completion (every batch committed or deferred).
//!   - `run.log`     — permanent append-only human timeline across all Runs,
//!     for post-mortem ("翻阅 log 排查").
//!
//! A `.aic/lock` advisory file (created exclusively, removed on drop) prevents
//! two concurrent `aic` Runs in one worktree from corrupting `active.json`.
//!
//! Resume is pure replay: the captured diffs are staged per-batch exactly as
//! the live loop does, so the proven within-Run staging path is reused
//! unchanged. Integrity is a per-pending-file content-fingerprint compare; a
//! file the user mutated since plan time defers its batch (left unstaged, never
//! lost) rather than risking a wrong replay.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::generator::BatchPlanOutput;
use crate::git::Git;

const DIR: &str = ".aic";
const ACTIVE: &str = "active.json";
const LOG: &str = "run.log";
const LOCK: &str = "lock";

/// One Run's full in-flight state. Serialized to `.aic/active.json`.
#[derive(Serialize, Deserialize)]
pub struct RunState {
    /// When the plan was captured, as Unix epoch seconds.
    pub created_at: u64,
    /// Short HEAD oid at plan time, for the human log and audit.
    pub head_at_plan: String,
    /// The planner's batch split.
    pub plan: BatchPlanOutput,
    /// Each file's captured workdir-vs-HEAD diff, replayed per-batch. Frozen at
    /// plan time so hunk numbering stays stable across the Run's commits.
    pub raw_diffs: std::collections::HashMap<String, String>,
    /// `{file -> content fingerprint}` of the worktree file at plan time
    /// (`Some(hash)` when present, `None` when absent/deleted). Recomputed on
    /// resume; a mismatch defers the batch. Typed as `Option<String>` so the
    /// deleted case is unforgeable rather than relying on a string sentinel.
    pub file_hashes: std::collections::HashMap<String, Option<String>>,
    /// Per-batch progress, aligned with `plan.batches`.
    pub batches: Vec<BatchEntry>,
}

/// A batch's progress within a Run.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub enum BatchEntry {
    /// Not yet committed.
    Pending,
    /// Committed as `sha`.
    Committed { sha: String },
    /// Deferred — a file it touches changed since plan time; left unstaged so
    /// the user's change is never lost. (CONTEXT.md ubiquitous language:
    /// "Deferred batch".)
    Deferred { reason: String },
}

impl RunState {
    /// Count how many batches have committed.
    pub fn count_committed(&self) -> usize {
        self.batches
            .iter()
            .filter(|e| matches!(e, BatchEntry::Committed { .. }))
            .count()
    }

    /// Count how many batches have been deferred.
    pub fn count_deferred(&self) -> usize {
        self.batches
            .iter()
            .filter(|e| matches!(e, BatchEntry::Deferred { .. }))
            .count()
    }

    /// Indices of batches still pending (not committed, not deferred).
    pub fn pending_indices(&self) -> Vec<usize> {
        self.batches
            .iter()
            .enumerate()
            .filter(|(_, e)| matches!(e, BatchEntry::Pending))
            .map(|(i, _)| i)
            .collect()
    }

    /// For every still-pending batch, find files whose worktree content no
    /// longer matches the plan-time fingerprint. Each returned entry is
    /// `(batch index, changed files)` — a batch with any changed file must be
    /// deferred rather than replayed against a stale snapshot.
    pub fn integrity_violations(&self) -> Vec<(usize, Vec<String>)> {
        let mut out = Vec::new();
        for i in self.pending_indices() {
            let changed: Vec<String> = self.plan.batches[i]
                .unique_files()
                .into_iter()
                .filter(|f| {
                    let stored = self.file_hashes.get(f).cloned().flatten();
                    fingerprint(f) != stored
                })
                .collect();
            if !changed.is_empty() {
                out.push((i, changed));
            }
        }
        out
    }

    /// Atomically write state to `.aic/active.json` (temp + rename). Ensures
    /// `.aic/` is gitignored first so the state file never appears as an
    /// unstaged change in the next Run.
    pub fn save(&self) -> Result<()> {
        let dir = aic_dir()?;
        std::fs::create_dir_all(&dir)?;
        ensure_self_ignored(&dir);
        let active = dir.join(ACTIVE);
        let tmp = dir.join(format!("{ACTIVE}.tmp"));
        let json = serde_json::to_string_pretty(self).context("serialize run state")?;
        std::fs::write(&tmp, &json).with_context(|| format!("write {tmp:?}"))?;
        std::fs::rename(&tmp, &active).context("atomic rename of active.json")?;
        Ok(())
    }

    /// Load in-flight state, or `None` when no Run is open.
    pub fn load() -> Result<Option<RunState>> {
        let path = aic_dir()?.join(ACTIVE);
        let Some(content) = std::fs::read_to_string(&path).ok() else {
            return Ok(None);
        };
        let rs = serde_json::from_str(&content)
            .with_context(|| format!("parse {path:?} — remove it to start fresh"))?;
        Ok(Some(rs))
    }

    /// Remove in-flight state (on clean completion or explicit discard).
    pub fn clear() -> Result<()> {
        let _ = std::fs::remove_file(aic_dir()?.join(ACTIVE));
        Ok(())
    }
}

/// Content fingerprint of a worktree file: hex SHA-256 of its bytes, or
/// `None` when it cannot be read (gone, or otherwise unhashable). A
/// present-then-deleted file (`Some(h)` vs `None`) therefore never matches its
/// plan-time fingerprint, and the deleted state is represented by a typed
/// `None` rather than a forgeable string sentinel.
pub fn fingerprint(path: &str) -> Option<String> {
    let bytes = Git::read_worktree(path).ok()?;
    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

/// Append one timestamped line to `.aic/run.log`. Best-effort: log failures are
/// swallowed (status output, never load-bearing).
pub fn log(line: &str) {
    let Ok(dir) = aic_dir() else {
        return;
    };
    let _ = std::fs::create_dir_all(&dir);
    ensure_self_ignored(&dir);
    let entry = format!("[{}] {line}\n", iso_utc_now());
    if let Ok(mut f) = OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(LOG))
    {
        let _ = f.write_all(entry.as_bytes());
    }
}

/// Exclusive advisory lock at `.aic/lock`. Created with `O_CREAT|O_EXCL`; a
/// second concurrent `aic` in the same worktree is refused. Removed on drop —
/// but a crashed process leaves a stale lock, recoverable by deleting the file.
pub struct RunLock {
    path: PathBuf,
}

impl RunLock {
    /// Acquire the lock, refusing if another Run holds it.
    pub fn acquire() -> Result<Self> {
        let dir = aic_dir()?;
        std::fs::create_dir_all(&dir)?;
        ensure_self_ignored(&dir);
        let path = dir.join(LOCK);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => Ok(Self { path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => anyhow::bail!(
                "another aic run is in progress in this worktree; remove .aic/lock if it is stale"
            ),
            Err(e) => Err(e).with_context(|| format!("acquire {path:?}")),
        }
    }
}

impl Drop for RunLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn aic_dir() -> Result<PathBuf> {
    Ok(Git::workdir()?.join(DIR))
}

/// Make `.aic/` invisible to `git status` without ever touching the repo's own
/// top-level `.gitignore`. A `.gitignore` inside `.aic/` containing `*` ignores
/// every scratch file (including itself); git then collapses the directory —
/// all contents ignored → the directory is never listed as untracked.
fn ensure_self_ignored(dir: &Path) {
    let g = dir.join(".gitignore");
    if std::fs::read_to_string(&g).unwrap_or_default().trim() != "*" {
        let _ = std::fs::write(&g, "*\n");
    }
}

/// Current time as Unix epoch seconds. The single source of "now" shared by
/// `active.json`'s `created_at` and the run-log timestamps, computed from
/// `std::time` so no time crate is needed.
pub fn epoch_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// ISO-8601 UTC (`YYYY-MM-DDTHH:MM:SSZ`) for the current instant.
fn iso_utc_now() -> String {
    iso_utc(epoch_now())
}

fn iso_utc(epoch_secs: u64) -> String {
    let days = (epoch_secs / 86400) as i64;
    let sod = epoch_secs % 86400;
    // Howard Hinnant's civil-from-days.
    let z = days + 719468;
    let era = (if z >= 0 { z } else { z - 146096 }) / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if m <= 2 { y + 1 } else { y };
    let hh = sod / 3600;
    let mm = (sod % 3600) / 60;
    let ss = sod % 60;
    format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_utc_matches_known_epoch() {
        // 2021-01-01T00:00:00Z = 1609459200.
        assert_eq!(iso_utc(1_609_459_200), "2021-01-01T00:00:00Z");
    }

    #[test]
    fn iso_utc_handles_leap_year_and_year_rollover() {
        // 2000-03-01T00:00:00Z = 951868800 (day after leap day 2000).
        assert_eq!(iso_utc(951_868_800), "2000-03-01T00:00:00Z");
        // 1999-12-31T23:59:59Z = 946684799.
        assert_eq!(iso_utc(946_684_799), "1999-12-31T23:59:59Z");
    }
}
