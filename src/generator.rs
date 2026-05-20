use std::collections::HashSet;

use crate::llm::LLM;
use crate::prompt::PromptConfig;

pub struct Generator {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct CommitOutput {
    pub message: String,
    pub body: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchPlanBatch {
    pub files: Vec<String>,
    pub reason: Option<String>,
}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchPlanOutput {
    pub batches: Vec<BatchPlanBatch>,
}

pub fn validate_batch_plan(
    plan: &BatchPlanOutput,
    original_paths: &[String],
) -> anyhow::Result<()> {
    if plan.batches.is_empty() {
        anyhow::bail!("LLM returned no batches — no commits were created");
    }

    let original: HashSet<&str> = original_paths.iter().map(|s| s.as_str()).collect();
    let mut seen: HashSet<&str> = HashSet::new();

    for (i, batch) in plan.batches.iter().enumerate() {
        if batch.files.is_empty() {
            anyhow::bail!("batch {} has no files", i + 1);
        }

        for file in &batch.files {
            if !original.contains(file.as_str()) {
                anyhow::bail!("batch {} references unknown file: {file}", i + 1);
            }
            if !seen.insert(file.as_str()) {
                anyhow::bail!("file {file} appears in multiple batches");
            }
        }
    }

    let returned: HashSet<&str> = seen;
    let missing: Vec<&str> = original.difference(&returned).copied().collect();
    if !missing.is_empty() {
        anyhow::bail!(
            "LLM response did not cover all files. Missing: {}",
            missing.join(", ")
        );
    }

    Ok(())
}

impl Generator {
    pub async fn generate_commit_message(diff: &str) -> anyhow::Result<CommitOutput> {
        let p = PromptConfig::default().git_message;
        LLM::from_env().agent(&p).schema::<CommitOutput>(diff).await
    }

    pub async fn split_patch(diff: &str) -> anyhow::Result<BatchPlanOutput> {
        let p = PromptConfig::default().batch_plan_prompt;
        LLM::from_env()
            .agent(&p)
            .schema::<BatchPlanOutput>(diff)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(files: &[&str]) -> Vec<String> {
        files.iter().map(|s| s.to_string()).collect()
    }

    fn batch(files: &[&str], reason: &str) -> BatchPlanBatch {
        BatchPlanBatch {
            files: paths(files),
            reason: Some(reason.to_string()),
        }
    }

    #[test]
    fn valid_single_batch() {
        let plan = BatchPlanOutput {
            batches: vec![batch(&["a.rs", "b.rs"], "add feature")],
        };
        assert!(validate_batch_plan(&plan, &paths(&["a.rs", "b.rs"])).is_ok());
    }

    #[test]
    fn valid_multiple_batches() {
        let plan = BatchPlanOutput {
            batches: vec![
                batch(&["a.rs"], "fix bug"),
                batch(&["b.rs"], "add feature"),
            ],
        };
        assert!(validate_batch_plan(&plan, &paths(&["a.rs", "b.rs"])).is_ok());
    }

    #[test]
    fn rejects_empty_batches() {
        let plan = BatchPlanOutput { batches: vec![] };
        let result = validate_batch_plan(&plan, &paths(&["a.rs"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no batches"));
    }

    #[test]
    fn rejects_empty_file_list_in_batch() {
        let plan = BatchPlanOutput {
            batches: vec![BatchPlanBatch {
                files: vec![],
                reason: Some("empty".to_string()),
            }],
        };
        let result = validate_batch_plan(&plan, &paths(&["a.rs"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no files"));
    }

    #[test]
    fn rejects_unknown_file_path() {
        let plan = BatchPlanOutput {
            batches: vec![batch(&["a.rs", "phantom.rs"], "oops")],
        };
        let result = validate_batch_plan(&plan, &paths(&["a.rs"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown file"));
    }

    #[test]
    fn rejects_duplicate_file_across_batches() {
        let plan = BatchPlanOutput {
            batches: vec![
                batch(&["a.rs"], "batch 1"),
                batch(&["a.rs"], "batch 2"),
            ],
        };
        let result = validate_batch_plan(&plan, &paths(&["a.rs"]));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("multiple batches"));
    }

    #[test]
    fn rejects_missing_files() {
        let plan = BatchPlanOutput {
            batches: vec![batch(&["a.rs"], "partial")],
        };
        let result = validate_batch_plan(&plan, &paths(&["a.rs", "b.rs", "c.rs"]));
        assert!(result.is_err());
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("Missing:"));
        assert!(msg.contains("b.rs"));
        assert!(msg.contains("c.rs"));
    }

    #[test]
    fn deserialization_from_json() {
        let json = r#"{"batches":[{"files":["a.rs","b.rs"],"reason":"add auth"}]}"#;
        let plan: BatchPlanOutput = serde_json::from_str(json).unwrap();
        assert_eq!(plan.batches.len(), 1);
        assert_eq!(plan.batches[0].files, vec!["a.rs", "b.rs"]);
        assert_eq!(plan.batches[0].reason.as_deref(), Some("add auth"));
    }

    #[test]
    fn deserialization_without_reason() {
        let json = r#"{"batches":[{"files":["a.rs"]}]}"#;
        let plan: BatchPlanOutput = serde_json::from_str(json).unwrap();
        assert!(plan.batches[0].reason.is_none());
    }
}
