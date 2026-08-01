use std::collections::{HashMap, HashSet};

use crate::llm::LLM;
use crate::prompt::PromptConfig;

pub struct Generator {}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct CommitOutput {
    pub message: String,
    pub body: Option<String>,
}

/// One file's contribution to a batch. A single file can appear in several
/// batches with disjoint hunks — `git add -p` style — because a file often
/// mixes changes of different scopes.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchChange {
    /// Repo-relative file path.
    pub file: String,
    /// 1-based indices of the hunks (numbered in the diff shown to the model)
    /// that belong to this batch. Empty means every hunk of the file.
    #[serde(default)]
    pub hunks: Vec<usize>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct BatchPlanBatch {
    pub changes: Vec<BatchChange>,
    pub reason: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
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
        LLM::from_env()?
            .agent(&p)
            .schema::<CommitOutput>(diff)
            .await
    }

    /// Split the workdir diff into logical commit batches, streaming the
    /// model's reasoning to `on_reasoning` as it thinks. We stream the raw
    /// completion (rather than `prompt_typed`) so reasoning tokens are
    /// surfaced live; the system prompt already demands strict JSON, so we
    /// parse the accumulated text ourselves.
    pub async fn split_patch_streaming(
        diff: &str,
        on_reasoning: impl FnMut(&str),
    ) -> anyhow::Result<BatchPlanOutput> {
        let p = PromptConfig::default().batch_plan_prompt;
        let raw = LLM::from_env()?
            .agent(&p)
            .stream_with_reasoning(diff, on_reasoning)
            .await?;
        parse_json_response::<BatchPlanOutput>(&raw)
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

/// Parse a JSON-structured LLM response, tolerating the stray prose or code
/// fence models occasionally emit around the payload. Strips a wrapping
/// ```` ``` ````-fence, jumps to the first value start (skipping any leading
/// prose), and lets serde_json's streaming deserializer parse exactly one value
/// — so trailing commentary is ignored without us hand-rolling brace matching.
fn parse_json_response<T: serde::de::DeserializeOwned>(raw: &str) -> anyhow::Result<T> {
    let body = strip_code_fence(raw);
    let start = body.find(['{', '[']).unwrap_or(0);
    let mut stream =
        serde_json::Deserializer::from_str(body[start..].trim_start()).into_iter::<T>();
    match stream.next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(e)) => anyhow::bail!("failed to parse LLM JSON response: {e}\n--- raw ---\n{raw}"),
        None => anyhow::bail!("LLM response contained no JSON value"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_json_response_ignores_leading_prose_and_trailing_junk() {
        // Fence sits inside leading prose (so strip_code_fence can't help);
        // jump-to-`{` + serde_json's streaming parser handle both ends.
        let raw = "Here is the plan:\n```json\n{\"batches\": []}\n```\ndone";
        let out: BatchPlanOutput = parse_json_response(raw).unwrap();
        assert!(out.batches.is_empty());
    }

    #[test]
    fn parse_json_response_handles_escaped_quotes() {
        let raw =
            r#"{"batches": [{"changes": [{"file": "a\"b.rs", "hunks": []}], "reason": "x"}]}"#;
        let out: BatchPlanOutput = parse_json_response(raw).unwrap();
        assert_eq!(out.batches[0].changes[0].file, "a\"b.rs");
    }

    #[test]
    fn parse_json_response_returns_err_when_no_json() {
        let res: anyhow::Result<BatchPlanOutput> = parse_json_response("no json here at all");
        assert!(res.is_err());
    }

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
