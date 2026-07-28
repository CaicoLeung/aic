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
        LLM::from_env()?
            .agent(&p)
            .schema::<CommitOutput>(diff)
            .await
    }

    pub async fn split_patch(diff: &str) -> anyhow::Result<BatchPlanOutput> {
        let p = PromptConfig::default().batch_plan_prompt;
        LLM::from_env()?
            .agent(&p)
            .schema::<BatchPlanOutput>(diff)
            .await
    }

    /// Resolve one conflicted file. The LLM returns the full marker-free file
    /// content as raw text (not JSON) — feeding a whole source file through a
    /// JSON string field would bloat and risk truncation. Any accidental
    /// markdown code fence around the output is stripped (ADR 0005).
    pub async fn resolve_conflict(file_content: &str) -> anyhow::Result<String> {
        let p = PromptConfig::default().resolve_prompt;
        let raw = LLM::from_env()?.agent(&p).call(file_content).await?;
        Ok(strip_code_fence(&raw).to_string())
    }
}

/// Strip a surrounding ```…``` code fence if the model ignored the "no fences"
/// instruction. Only touches a fence that wraps the entire output; partial
/// fences (e.g. a fenced block legitimately inside the file) are left alone.
fn strip_code_fence(mut s: &str) -> &str {
    s = s.strip_suffix('\n').unwrap_or(s).trim();
    if !s.starts_with("```") {
        return s;
    }
    // Drop the opening fence line (``` or ```lang).
    let Some(nl) = s.find('\n') else {
        return s;
    };
    s = &s[nl + 1..];
    // Drop a trailing closing fence.
    let trimmed_end = s.trim_end();
    if let Some(idx) = trimmed_end.rfind("```")
        && trimmed_end[idx..].trim() == "```"
    {
        return trimmed_end[..idx].trim();
    }
    s.trim()
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
            batches: vec![batch(&["a.rs"], "fix bug"), batch(&["b.rs"], "add feature")],
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
            batches: vec![batch(&["a.rs"], "batch 1"), batch(&["a.rs"], "batch 2")],
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

    #[test]
    fn strip_fence_removes_wrapping_fence() {
        assert_eq!(strip_code_fence("```\nfn main() {}\n```"), "fn main() {}");
    }

    #[test]
    fn strip_fence_removes_language_tag() {
        assert_eq!(strip_code_fence("```rust\nlet x = 1;\n```"), "let x = 1;");
    }

    #[test]
    fn strip_fence_leaves_plain_content_alone() {
        assert_eq!(strip_code_fence("fn main() {}"), "fn main() {}");
    }

    #[test]
    fn strip_fence_leaves_inner_fences_alone() {
        // A fenced block that is legitimately part of the file is not stripped —
        // only a fence wrapping the *entire* output is.
        let inner = "text before\n\n```rs\ncode\n```\n\ntext after";
        assert_eq!(strip_code_fence(inner), inner);
    }
}
