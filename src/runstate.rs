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
//! On Unix the lock records its owner PID and self-heals: a lock left behind
//! by a crashed process is detected (owner no longer alive) and reclaimed, so
//! an interrupt can never wedge future Runs.
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

    /// The HEAD oid the resume replay *assumes* is current: the sha of the
    /// last batch that made a real commit, or the plan-time HEAD if no batch
    /// committed yet. Batches recorded `Committed` with an empty sha ("nothing
    /// left to stage" — a pre-commit hook landed their change via an earlier
    /// batch) do *not* advance HEAD, so they are skipped. Returns `None` only
    /// when this cannot be determined (no committed sha and an empty plan-time
    /// HEAD); the caller then skips the drift check rather than aborting.
    ///
    /// The returned string is a *prefix* of the full oid (a 7-char commit sha
    /// from `Git::commit`, or git's variable-length `--short` for the plan-time
    /// HEAD). The caller matches it as a prefix against the full current HEAD,
    /// so the two abbreviation forms never produce a false mismatch.
    pub fn expected_head(&self) -> Option<&str> {
        for entry in self.batches.iter().rev() {
            if let BatchEntry::Committed { sha } = entry
                && !sha.is_empty()
            {
                return Some(sha.as_str());
            }
        }
        let h = self.head_at_plan.trim();
        (!h.is_empty()).then_some(h)
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

/// Exclusive advisory lock at `.aic/lock`. On Unix the lock is a kernel
/// `flock(2)` on a held file descriptor — acquired atomically by the kernel
/// and released automatically when the process exits (even on crash or
/// `kill -9`), so an interrupted Run can never wedge future Runs and there is
/// no stale sentinel to sniff or reclaim. On non-Unix (where `flock` is not
/// available) the lock falls back to an `O_EXCL` sentinel file: first writer
/// wins, with no self-heal (a crash leaves the sentinel until manual removal)
/// but — having no reclaim step — no reclaim *race* either.
///
/// The lock file is a permanent target, never removed: unlinking it while it
/// might be open would hand a fresh inode to a concurrent opener and silently
/// break exclusion (the new fd flocks a different inode). It lives under the
/// gitignored `.aic/`, so it never appears in `git status`.
pub struct RunLock {
    /// Held open for its Drop side-effect: closing the fd releases the flock.
    #[cfg(unix)]
    #[allow(dead_code)]
    file: std::fs::File,
    #[cfg(not(unix))]
    path: PathBuf,
}

impl RunLock {
    /// Acquire the lock, refusing if another live Run already holds it.
    pub fn acquire() -> Result<Self> {
        let dir = aic_dir()?;
        std::fs::create_dir_all(&dir)?;
        ensure_self_ignored(&dir);
        let path = dir.join(LOCK);
        #[cfg(unix)]
        {
            Ok(Self {
                file: flock_acquire(&path)?,
            })
        }
        #[cfg(not(unix))]
        {
            OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .with_context(|| format!("acquire lock {path:?}"))?;
            Ok(Self { path })
        }
    }
}

/// Open (creating if absent) the Unix lock *target* and take a non-blocking
/// exclusive `flock` on it. The file is just a stable inode to lock; mutual
/// exclusion is the advisory lock on the returned fd, NOT the file's
/// existence — so `O_CREAT` (not `O_EXCL`) sidesteps the reclaim race: two
/// concurrent openers get the same inode and contend on `flock`, which the
/// kernel arbitrates atomically.
#[cfg(unix)]
fn flock_acquire(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("open lock target {path:?}"))?;
    // LOCK_NB: fail at once if held, instead of blocking the Run forever.
    // EWOULDBLOCK/EAGAIN → another live Run holds it; any other errno is a
    // real I/O error surfaced to the caller.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = std::io::Error::last_os_error();
        // EWOULDBLOCK and EAGAIN share the same value on every Rust Unix
        // target, so one comparison covers both "would block" errno forms.
        let held = e.raw_os_error() == Some(libc::EWOULDBLOCK);
        if held {
            anyhow::bail!(
                "another aic run is in progress in this worktree; \
                 it releases automatically when that run exits"
            );
        }
        return Err(e).with_context(|| format!("flock {path:?}"));
    }
    Ok(file)
}

// Only the non-Unix sentinel path needs a Drop (to remove the file it
// created). On Unix the held fd is the lock: RunLock has no custom Drop, so
// dropping the struct closes the fd and the kernel releases the flock — the
// canonical release path, and exactly what happens on a crash.
#[cfg(not(unix))]
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

    // --- RunLock (kernel flock exclusion) ------------------------------------
    //
    // The lock is a `flock` on a held fd: a second acquire on the same target
    // fails at once, and dropping the holder (closing the fd) lets the next
    // acquire succeed — which is also exactly what the kernel does on a crash,
    // so an interrupted run can never wedge future ones. These exercise
    // `flock_acquire` on arbitrary temp paths (bypassing `aic_dir`/git).

    #[cfg(unix)]
    #[test]
    fn flock_lock_excludes_a_second_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        let _first = flock_acquire(&path).expect("first acquire must succeed");
        let err =
            flock_acquire(&path).expect_err("a second acquire while the first is held must fail");
        assert!(
            err.to_string().contains("another aic run"),
            "expected the in-progress message, got: {err}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn flock_lock_releases_on_drop_so_it_can_be_reacquired() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("lock");
        {
            let _first = flock_acquire(&path).expect("first acquire");
        } // dropped → fd closed → flock released by the kernel
        flock_acquire(&path).expect("re-acquire after the holder dropped must succeed");
    }

    // --- RunState::expected_head (resume base-drift predicate) ---------------

    fn empty_state(head: &str) -> RunState {
        RunState {
            created_at: 0,
            head_at_plan: head.into(),
            plan: crate::generator::BatchPlanOutput {
                batches: vec![crate::generator::BatchPlanBatch {
                    changes: vec![],
                    reason: None,
                }],
            },
            raw_diffs: Default::default(),
            file_hashes: Default::default(),
            batches: vec![BatchEntry::Pending],
        }
    }

    #[test]
    fn expected_head_falls_back_to_plan_head_with_no_commits() {
        // Nothing committed yet → the replay assumes the plan-time HEAD.
        let rs = empty_state("abc1234");
        assert_eq!(rs.expected_head(), Some("abc1234"));
    }

    #[test]
    fn expected_head_is_the_last_real_commit_sha() {
        // A committed batch supersedes the plan-time HEAD as the assumed tip.
        let mut rs = empty_state("abc1234");
        rs.batches[0] = BatchEntry::Committed {
            sha: "deadbeef".into(),
        };
        assert_eq!(rs.expected_head(), Some("deadbeef"));

        // A "nothing left to stage" batch (empty sha) does NOT advance HEAD —
        // the last real commit remains the tip.
        rs.batches.push(BatchEntry::Pending);
        rs.batches
            .push(BatchEntry::Committed { sha: String::new() });
        assert_eq!(
            rs.expected_head(),
            Some("deadbeef"),
            "an empty-sha (nothing-staged) batch must not become the tip"
        );

        // A later real commit becomes the new tip.
        rs.batches[2] = BatchEntry::Committed {
            sha: "cafebabe".into(),
        };
        assert_eq!(rs.expected_head(), Some("cafebabe"));
    }

    #[test]
    fn expected_head_is_none_when_undeterminable() {
        // No committed sha and an empty plan-time HEAD → can't validate.
        let rs = empty_state("");
        assert_eq!(rs.expected_head(), None);
    }
}
