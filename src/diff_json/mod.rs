//! The diff-JSON envelope: the payload format the commit-message and
//! batch-planner models actually see. One shape, shared by every drafting
//! path — the staged first draft, the plan-time pre-draft slicer, and the
//! confirmation Re-generate action all must hand the model identical JSON,
//! so the envelope lives in one place.
//!
//! Pure by design: raw data in (file paths, diff text, hunk selections),
//! model-contract JSON out. No `Git` here — *which* diff is fetched at which
//! phase is Run policy (`crate::run`); *how it is wrapped* is this module.

use std::collections::HashMap;

use anyhow::Context;

use crate::diff;
use crate::generator;

/// The commit-message LLM's diff payload: `{"staged_files":[{"path","diff"}]}`,
/// each `diff` the numbered scoped view (`format_diff_scoped`).
pub(crate) fn files_json(files: impl IntoIterator<Item = (String, String)>) -> String {
    let files: Vec<serde_json::Value> = files
        .into_iter()
        .map(|(path, diff)| serde_json::json!({ "path": path, "diff": diff }))
        .collect();
    serde_json::json!({ "staged_files": files }).to_string()
}

/// One batch's plan-time diff as the commit-message JSON. Each file's planned
/// hunks are sliced out of the plan-time workdir diff — the numbering the model
/// saw and the plan refers to — then scoped; an empty `hunks` slice means the
/// whole file. Used to pre-draft a batch's message before its staging turn.
pub(crate) fn plan_batch_diff_json(
    batch: &generator::BatchPlanBatch,
    raw_diffs: &HashMap<String, String>,
) -> anyhow::Result<String> {
    let pairs = batch
        .changes
        .iter()
        .map(|c| {
            let raw = raw_diffs
                .get(&c.file)
                .with_context(|| format!("no plan-time diff for batch file {}", c.file))?;
            let scoped = if c.hunks.is_empty() {
                diff::format_diff_scoped(raw, &c.file)
            } else {
                let sliced = diff::parse_file_patch(raw).slice(&c.hunks)?;
                diff::format_diff_scoped(&sliced, &c.file)
            };
            Ok((c.file.clone(), scoped))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(files_json(pairs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-hunk single-file workdir diff whose hunks add distinct tokens
    /// (`alpha` / `beta`) so slicing can be told apart from the whole file.
    fn two_hunk_diff() -> &'static str {
        "diff --git a/f.rs b/f.rs\n\
index 1..2 100644\n\
--- a/f.rs\n\
+++ b/f.rs\n\
@@ -1,3 +1,3 @@\n\
 ctx\n\
-x\n\
+alpha\n\
 ctx\n\
@@ -10,3 +10,3 @@ fn b\n\
 ctx\n\
-y\n\
+beta\n\
 ctx\n"
    }

    fn batch(file: &str, hunks: &[usize]) -> generator::BatchPlanBatch {
        generator::BatchPlanBatch {
            changes: vec![generator::BatchChange {
                file: file.to_string(),
                hunks: hunks.to_vec(),
            }],
            reason: None,
        }
    }

    /// Pull the single file's scoped diff out of the `staged_files` JSON so a
    /// test asserts on hunk content, not on the envelope shape.
    fn only_diff(json: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(json).unwrap();
        v["staged_files"][0]["diff"].as_str().unwrap().to_string()
    }

    /// Non-empty `hunks` slice out only the planned hunks from the plan-time
    /// diff — hunk 2's `beta` lands, hunk 1's `alpha` does not.
    #[test]
    fn plan_batch_diff_json_slices_selected_hunks() {
        let raw_diffs = HashMap::from([("f.rs".to_string(), two_hunk_diff().to_string())]);
        let json = plan_batch_diff_json(&batch("f.rs", &[2]), &raw_diffs).unwrap();
        let diff = only_diff(&json);
        assert!(diff.contains("beta"), "selected hunk 2 must be present");
        assert!(!diff.contains("alpha"), "unselected hunk 1 must be absent");
    }

    /// Empty `hunks` means every hunk of the file — both `alpha` and `beta`.
    #[test]
    fn plan_batch_diff_json_empty_hunks_keeps_whole_file() {
        let raw_diffs = HashMap::from([("f.rs".to_string(), two_hunk_diff().to_string())]);
        let json = plan_batch_diff_json(&batch("f.rs", &[]), &raw_diffs).unwrap();
        let diff = only_diff(&json);
        assert!(diff.contains("alpha") && diff.contains("beta"));
    }

    /// A batch file with no captured plan-time diff is a programming error,
    /// not a recoverable one — name the file in the error.
    #[test]
    fn plan_batch_diff_json_missing_file_errors() {
        let raw_diffs = HashMap::<String, String>::new();
        let err = plan_batch_diff_json(&batch("ghost.rs", &[]), &raw_diffs)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no plan-time diff"), "unexpected error: {err}");
        assert!(err.contains("ghost.rs"));
    }

    /// The shared envelope both draft paths emit — locked so a change at one
    /// call site can't drift the shape the model (and the other path) expects.
    #[test]
    fn files_json_envelope_shape_is_stable() {
        let json = files_json([("a.rs".to_string(), "diff-a".to_string())]);
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["staged_files"][0]["path"], "a.rs");
        assert_eq!(v["staged_files"][0]["diff"], "diff-a");
    }
}
