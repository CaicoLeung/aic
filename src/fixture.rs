//! Offline fixture mode for reproducible demos (ADR 0009, AIC-34).
//!
//! When `AIC_FIXTURE_DIR` is set, the commit workflow is served from a recorded
//! manifest instead of calling an LLM. This keeps `scripts/demo.sh` (and the
//! README GIF) deterministic and network-free: the same sample repo always
//! yields the same atomic commits, so the demo never rots and runs in CI
//! without a model.
//!
//! The manifest is a single `fixtures.json` in `$AIC_FIXTURE_DIR`:
//!
//! ```jsonc
//! {
//!   "plan":    { "batches": [ /* a BatchPlanOutput */ ] },
//!   "commits": [ /* one CommitOutput per batch, in batch order */ ]
//! }
//! ```
//!
//! [`serve_plan`] returns the recorded batch plan; [`serve_commit`] returns the
//! next recorded commit message in order. Call order is deterministic within
//! one Run (one planner call, then one messenger call per batch), so the
//! order-based commit queue is stable.
//!
//! Fixture mode is a demo affordance, not a shortcut: the served plan still
//! flows through [`crate::generator::validate_batch_plan`] against the *real*
//! diff's hunk counts, so a stale fixture (wrong file, wrong hunk count) fails
//! loudly instead of committing nonsense. A broken or unreadable manifest is a
//! hard error — fixture mode never silently falls through to a live LLM, which
//! would surprise a no-network demo/CI run with a network call.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::generator::{BatchPlanOutput, CommitOutput};

/// Env var that activates fixture mode. Unset (or empty) = normal live LLM.
pub const FIXTURE_DIR_ENV: &str = "AIC_FIXTURE_DIR";

/// Manifest filename read from `$AIC_FIXTURE_DIR`.
const MANIFEST_FILE: &str = "fixtures.json";

/// Monotonic index into the manifest's `commits` within one process. Each
/// `aic` invocation is one Run in one process, so this starts at 0 for every
/// run and advances once per messenger call (planner calls don't touch it).
static COMMIT_INDEX: AtomicUsize = AtomicUsize::new(0);

#[derive(serde::Deserialize)]
struct Manifest {
    plan: BatchPlanOutput,
    #[serde(default)]
    commits: Vec<CommitOutput>,
}

/// Resolve and read the manifest when fixture mode is active.
///
/// - `Ok(None)` — fixture mode is off (env unset/empty).
/// - `Ok(Some(manifest))` — manifest parsed.
/// - `Err(_)` — fixture mode is active but the manifest is unreadable or
///   malformed. This is a hard error: the caller surfaces it so a broken
///   fixture fails loudly instead of silently reaching for a live LLM.
fn load_manifest() -> anyhow::Result<Option<Manifest>> {
    let Some(dir) = env::var_os(FIXTURE_DIR_ENV).filter(|s| !s.is_empty()) else {
        return Ok(None);
    };
    let path = PathBuf::from(&dir).join(MANIFEST_FILE);
    let path_display = path.display();
    let text = fs::read_to_string(&path)
        .map_err(|e| anyhow::anyhow!("fixture mode active but cannot read {path_display}: {e}"))?;
    let manifest: Manifest = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("fixture mode active but {path_display} is invalid: {e}"))?;
    Ok(Some(manifest))
}

/// Recorded batch plan, or `None` when fixture mode is off.
///
/// Served verbatim; the caller still runs [`validate_batch_plan`] so a stale
/// plan (mismatched file/hunk counts) is rejected downstream.
pub fn serve_plan() -> Option<anyhow::Result<BatchPlanOutput>> {
    match load_manifest() {
        Ok(None) => None,
        Ok(Some(m)) => Some(Ok(m.plan)),
        Err(e) => Some(Err(e)),
    }
}

/// Next recorded commit message (in batch order), or `None` when fixture mode
/// is off. Errors if the queue is exhausted — more batches than recorded
/// commits — so a manifest that drifts out of sync with its plan fails loudly.
pub fn serve_commit() -> Option<anyhow::Result<CommitOutput>> {
    match load_manifest() {
        Ok(None) => None,
        Ok(Some(m)) => {
            let i = COMMIT_INDEX.fetch_add(1, Ordering::SeqCst);
            match m.commits.get(i) {
                Some(c) => Some(Ok(c.clone())),
                None => Some(Err(anyhow::anyhow!(
                    "fixture mode: exhausted recorded commits at index {i} \
                     (manifest holds {}); the plan produced more batches than \
                     the manifest records — re-record the fixtures",
                    m.commits.len()
                ))),
            }
        }
        Err(e) => Some(Err(e)),
    }
}

/// Reset the commit counter. Only used by tests so a single test binary can
/// exercise the commit sequence deterministically regardless of run order.
#[cfg(test)]
pub(crate) fn reset_commit_index_for_tests() {
    COMMIT_INDEX.store(0, Ordering::SeqCst);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::generator::{BatchChange, BatchPlanBatch};
    use std::io::Write;

    fn commit(message: &str) -> CommitOutput {
        CommitOutput {
            message: message.to_string(),
            body: None,
        }
    }

    fn plan_batch(file: &str, hunk: usize) -> BatchPlanBatch {
        BatchPlanBatch {
            changes: vec![BatchChange {
                file: file.to_string(),
                hunks: vec![hunk],
            }],
            reason: None,
        }
    }

    /// Write a manifest into `dir`.
    fn write_manifest(dir: &std::path::Path, plan: &BatchPlanOutput, commits: &[&str]) {
        let plan_json = serde_json::to_string(plan).unwrap();
        let commits_json: Vec<String> = commits
            .iter()
            .map(|m| serde_json::to_string(&commit(m)).unwrap())
            .collect();
        let json = format!(
            "{{\"plan\":{},\"commits\":[{}]}}",
            plan_json,
            commits_json.join(",")
        );
        fs::write(dir.join(MANIFEST_FILE), json).unwrap();
    }

    #[test]
    fn serve_plan_and_commit_return_none_when_unset() {
        temp_env::with_var_unset(FIXTURE_DIR_ENV, || {
            assert!(serve_plan().is_none());
            assert!(serve_commit().is_none());
        });
    }

    #[test]
    fn serve_plan_and_commit_return_none_when_empty() {
        temp_env::with_var(FIXTURE_DIR_ENV, Some(""), || {
            assert!(serve_plan().is_none());
            assert!(serve_commit().is_none());
        });
    }

    #[test]
    fn missing_manifest_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        // No fixtures.json written.
        temp_env::with_var(FIXTURE_DIR_ENV, Some(dir.path().as_os_str()), || {
            let plan = serve_plan().expect("fixture mode is active");
            assert!(plan.is_err(), "missing manifest must error, not fall back");
            assert!(plan.unwrap_err().to_string().contains("cannot read"));
        });
    }

    #[test]
    fn malformed_manifest_is_a_hard_error() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join(MANIFEST_FILE), "{ not json").unwrap();
        temp_env::with_var(FIXTURE_DIR_ENV, Some(dir.path().as_os_str()), || {
            let plan = serve_plan().expect("fixture mode is active");
            assert!(plan.is_err());
            assert!(plan.unwrap_err().to_string().contains("invalid"));
        });
    }

    #[test]
    fn serve_plan_returns_recorded_plan() {
        let dir = tempfile::tempdir().unwrap();
        let plan = BatchPlanOutput {
            batches: vec![
                plan_batch("src/a.rs", 1),
                plan_batch("src/a.rs", 2),
                plan_batch("src/a.rs", 3),
            ],
        };
        write_manifest(dir.path(), &plan, &["m1", "m2", "m3"]);
        temp_env::with_var(FIXTURE_DIR_ENV, Some(dir.path().as_os_str()), || {
            let served = serve_plan().expect("active").expect("valid manifest");
            assert_eq!(served.batches.len(), 3);
            assert_eq!(served.batches[0].changes[0].hunks, vec![1]);
            assert_eq!(served.batches[2].changes[0].file, "src/a.rs");
        });
    }

    /// The only test that advances the commit counter. Reset first so the
    /// global counter is deterministic regardless of test run order.
    #[test]
    fn serve_commit_advances_in_order_and_errors_when_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let plan = BatchPlanOutput {
            batches: vec![plan_batch("a.rs", 1), plan_batch("a.rs", 2)],
        };
        write_manifest(dir.path(), &plan, &["fix: one", "feat: two"]);
        temp_env::with_var(FIXTURE_DIR_ENV, Some(dir.path().as_os_str()), || {
            reset_commit_index_for_tests();
            assert_eq!(serve_commit().unwrap().unwrap().message, "fix: one");
            assert_eq!(serve_commit().unwrap().unwrap().message, "feat: two");
            let exhausted = serve_commit().unwrap();
            assert!(exhausted.is_err());
            assert!(exhausted.unwrap_err().to_string().contains("exhausted"));
        });
    }

    /// A manifest with no `commits` (only a plan) still serves the plan; the
    /// optional field defaults to empty.
    #[test]
    fn commits_field_is_optional() {
        let dir = tempfile::tempdir().unwrap();
        let plan = BatchPlanOutput {
            batches: vec![plan_batch("a.rs", 1)],
        };
        let full = format!("{{\"plan\":{}}}", serde_json::to_string(&plan).unwrap());
        fs::write(dir.path().join(MANIFEST_FILE), full).unwrap();
        temp_env::with_var(FIXTURE_DIR_ENV, Some(dir.path().as_os_str()), || {
            reset_commit_index_for_tests();
            let served = serve_plan().unwrap().unwrap();
            assert_eq!(served.batches.len(), 1);
            // No commits recorded → first serve_commit errors (exhausted at 0).
            assert!(serve_commit().unwrap().is_err());
        });
    }
}
