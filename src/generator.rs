use std::collections::{HashMap, HashSet};

use crate::llm::LlmConfig;
use crate::prompt::PromptConfig;

pub struct Generator {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommitOutput {
    pub message: String,
    pub body: Option<String>,
}

/// One file's contribution to a batch. A single file can appear in several
/// batches with disjoint hunks — `git add -p` style — because a file often
/// mixes changes of different scopes.
#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchChange {
    /// Repo-relative file path.
    pub file: String,
    /// 1-based indices of the hunks (numbered in the diff shown to the model)
    /// that belong to this batch. Empty means every hunk of the file.
    #[serde(default)]
    pub hunks: Vec<usize>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchPlanBatch {
    pub changes: Vec<BatchChange>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchPlanOutput {
    pub batches: Vec<BatchPlanBatch>,
}

/// Validate that the batch plan is an exact partition of every file's hunks:
/// each hunk of each file is assigned to exactly one batch, with no overlaps,
/// gaps, out-of-range, or unknown-file references.
///
/// `file_hunk_counts` carries every original file alongside how many hunks its
/// workdir-vs-HEAD diff has (the same numbering the model saw).
pub fn validate_batch_plan(
    plan: &BatchPlanOutput,
    file_hunk_counts: &[(String, usize)],
) -> anyhow::Result<()> {
    if plan.batches.is_empty() {
        anyhow::bail!("LLM returned no batches — no commits were created");
    }

    let counts: HashMap<&str, usize> = file_hunk_counts
        .iter()
        .map(|(path, count)| (path.as_str(), *count))
        .collect();
    // file -> hunks assigned so far (accumulated across batches)
    let mut assigned: HashMap<&str, HashSet<usize>> = HashMap::new();

    for (i, batch) in plan.batches.iter().enumerate() {
        if batch.changes.is_empty() {
            anyhow::bail!("batch {} has no changes", i + 1);
        }
        for change in &batch.changes {
            let Some(&count) = counts.get(change.file.as_str()) else {
                anyhow::bail!("batch {} references unknown file: {}", i + 1, change.file);
            };
            // Empty `hunks` = all hunks of the file in this one batch.
            let indices: Vec<usize> = if change.hunks.is_empty() {
                (1..=count).collect()
            } else {
                change.hunks.clone()
            };
            let slot = assigned.entry(change.file.as_str()).or_default();
            for idx in &indices {
                if *idx < 1 || *idx > count {
                    anyhow::bail!(
                        "batch {} references hunk {} of {}, which has only {} hunk(s)",
                        i + 1,
                        idx,
                        change.file,
                        count
                    );
                }
                if !slot.insert(*idx) {
                    anyhow::bail!(
                        "hunk {} of {} is assigned to more than one batch",
                        idx,
                        change.file
                    );
                }
            }
        }
    }

    // Every hunk of every file must be covered exactly once.
    let mut missing: Vec<String> = Vec::new();
    for (path, count) in file_hunk_counts {
        let slot = assigned.get(path.as_str()).cloned().unwrap_or_default();
        for h in 1..=*count {
            if !slot.contains(&h) {
                missing.push(format!("{path}:hunk {h}"));
            }
        }
    }
    if !missing.is_empty() {
        anyhow::bail!(
            "LLM response did not cover all hunks. Missing: {}",
            missing.join(", ")
        );
    }

    Ok(())
}

impl Generator {
    pub async fn generate_commit_message(diff: &str) -> anyhow::Result<CommitOutput> {
        let p = PromptConfig::default().git_message;
        LlmConfig::load()?
            .agent(&p)
            .schema::<CommitOutput>(diff)
            .await
    }

    /// Split the workdir diff into logical commit batches, streaming the
    /// model's reasoning to `on_reasoning` as it thinks. The LLMAgent seam
    /// owns streaming + tolerant parsing + retry (a budget-starved model's
    /// truncated JSON is retried with reasoning re-streamed), so this is a
    /// single typed call.
    pub async fn split_patch_streaming(
        diff: &str,
        on_reasoning: impl FnMut(&str),
    ) -> anyhow::Result<BatchPlanOutput> {
        let p = PromptConfig::default().batch_plan_prompt;
        LlmConfig::load()?
            .agent(&p)
            .stream_typed_with_reasoning::<BatchPlanOutput>(diff, on_reasoning)
            .await
    }

    /// Resolve one conflicted file. The LLM returns the full marker-free file
    /// content as raw text (not JSON) — feeding a whole source file through a
    /// JSON string field would bloat and risk truncation. Any accidental
    /// markdown code fence around the output is stripped (ADR 0005).
    pub async fn resolve_conflict(file_content: &str) -> anyhow::Result<String> {
        let p = PromptConfig::default().resolve_prompt;
        let raw = LlmConfig::load()?.agent(&p).call(file_content).await?;
        Ok(crate::llm::strip_code_fence(&raw).to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn change(file: &str, hunks: &[usize]) -> BatchChange {
        BatchChange {
            file: file.to_string(),
            hunks: hunks.to_vec(),
        }
    }

    fn batch(changes: &[BatchChange], reason: &str) -> BatchPlanBatch {
        BatchPlanBatch {
            changes: changes.to_vec(),
            reason: Some(reason.to_string()),
        }
    }

    fn counts(items: &[(&str, usize)]) -> Vec<(String, usize)> {
        items
            .iter()
            .map(|(path, count)| (path.to_string(), *count))
            .collect()
    }

    #[test]
    fn valid_whole_file_in_single_batch() {
        let plan = BatchPlanOutput {
            batches: vec![batch(&[change("a.rs", &[])], "add feature")],
        };
        assert!(validate_batch_plan(&plan, &counts(&[("a.rs", 2)])).is_ok());
    }

    #[test]
    fn valid_multiple_files_in_batches() {
        let plan = BatchPlanOutput {
            batches: vec![
                batch(&[change("a.rs", &[])], "fix bug"),
                batch(&[change("b.rs", &[])], "add feature"),
            ],
        };
        assert!(validate_batch_plan(&plan, &counts(&[("a.rs", 1), ("b.rs", 1)])).is_ok());
    }

    /// The headline behavior of hunk splitting: one file's hunks distributed
    /// across two batches.
    #[test]
    fn valid_one_file_split_across_batches() {
        let plan = BatchPlanOutput {
            batches: vec![
                batch(&[change("display.rs", &[1])], "feat: restyle commit line"),
                batch(
                    &[change("display.rs", &[2, 3])],
                    "refactor: drop batch summary",
                ),
            ],
        };
        assert!(
            validate_batch_plan(&plan, &counts(&[("display.rs", 3)])).is_ok(),
            "a single file may span multiple batches by hunk"
        );
    }

    #[test]
    fn rejects_empty_batches() {
        let plan = BatchPlanOutput { batches: vec![] };
        let result = validate_batch_plan(&plan, &counts(&[("a.rs", 1)]));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no batches"));
    }

    #[test]
    fn rejects_empty_changes_in_batch() {
        let plan = BatchPlanOutput {
            batches: vec![BatchPlanBatch {
                changes: vec![],
                reason: Some("empty".to_string()),
            }],
        };
        let result = validate_batch_plan(&plan, &counts(&[("a.rs", 1)]));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no changes"));
    }

    #[test]
    fn rejects_unknown_file() {
        let plan = BatchPlanOutput {
            batches: vec![batch(&[change("phantom.rs", &[])], "oops")],
        };
        let result = validate_batch_plan(&plan, &counts(&[("a.rs", 1)]));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown file"));
    }

    #[test]
    fn rejects_overlapping_hunk_across_batches() {
        let plan = BatchPlanOutput {
            batches: vec![
                batch(&[change("a.rs", &[1, 2])], "batch 1"),
                batch(&[change("a.rs", &[2])], "batch 2"),
            ],
        };
        let result = validate_batch_plan(&plan, &counts(&[("a.rs", 2)]));
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("more than one batch")
        );
    }

    #[test]
    fn rejects_out_of_range_hunk() {
        let plan = BatchPlanOutput {
            batches: vec![batch(&[change("a.rs", &[5])], "oops")],
        };
        let result = validate_batch_plan(&plan, &counts(&[("a.rs", 2)]));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("only 2 hunk"));
    }

    #[test]
    fn rejects_uncovered_hunk() {
        let plan = BatchPlanOutput {
            batches: vec![batch(&[change("a.rs", &[1])], "partial")],
        };
        let result = validate_batch_plan(&plan, &counts(&[("a.rs", 3)]));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Missing:"));
        assert!(msg.contains("a.rs:hunk 2"));
        assert!(msg.contains("a.rs:hunk 3"));
    }

    #[test]
    fn deserialization_from_json() {
        let json =
            r#"{"batches":[{"changes":[{"file":"a.rs","hunks":[1,2]}],"reason":"add auth"}]}"#;
        let plan: BatchPlanOutput = serde_json::from_str(json).unwrap();
        assert_eq!(plan.batches.len(), 1);
        assert_eq!(plan.batches[0].changes[0].file, "a.rs");
        assert_eq!(plan.batches[0].changes[0].hunks, vec![1, 2]);
        assert_eq!(plan.batches[0].reason.as_deref(), Some("add auth"));
    }

    #[test]
    fn deserialization_without_reason_or_hunks() {
        // Omitted `reason` and omitted `hunks` (whole-file) both default.
        let json = r#"{"batches":[{"changes":[{"file":"a.rs"}]}]}"#;
        let plan: BatchPlanOutput = serde_json::from_str(json).unwrap();
        assert!(plan.batches[0].reason.is_none());
        assert!(plan.batches[0].changes[0].hunks.is_empty());
    }
}
