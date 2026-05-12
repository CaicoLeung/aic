use std::env;

#[derive(Debug)]
pub struct PromptConfig {
    pub system_prompt: String,
    pub batch_plan_prompt: String,
}

const SYSTEM_PROMPT: &str = r#"
You are an expert at writing Git commit messages following the Conventional Commits v1.0.0 specification.

You will be given:
1. A description of a batch of changes
2. The combined git diff for that batch

Generate a commit message that accurately describes the batch.

Conventional Commits format:
<type>[optional scope]: <description>

Types: feat, fix, docs, style, refactor, test, chore, perf, ci

Rules:
- Keep description under 72 characters
- Use imperative mood
- Do not end with period
- Use scope when helpful (e.g., feat(auth):)

Return ONLY the commit message, no explanation.
"#;

const BATCH_PLAN_PROMPT: &str = r#"
You are an expert at analyzing git changes and grouping them into logical commits.

You will be given a list of unstaged file changes with their diffs.
Your task is to group these changes into logical commit batches.

Rules for grouping:
1. Files that are part of the same feature/fix should be in one batch
2. Different features should be in separate batches
3. Refactoring should be separate from feature work
4. Tests for a feature should be in the same batch as the feature
5. Documentation updates can be grouped together

Response format (strict JSON):
{
  "batches": [
    {
      "files": ["path/to/file1.py", "path/to/file2.py"],
      "reason": "Brief explanation of what this batch does (e.g., 'Add user authentication feature')"
    }
  ]
}

Return ONLY valid JSON, no explanation.
"#;

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            system_prompt: SYSTEM_PROMPT.trim().to_string(),
            batch_plan_prompt: BATCH_PLAN_PROMPT.trim().to_string(),
        }
    }
}

impl PromptConfig {
    pub fn from_env() -> Self {
        Self {
            system_prompt: env::var("AIC_SYSTEM_PROMPT")
                .unwrap_or_else(|_| Self::default().system_prompt),
            batch_plan_prompt: Self::default().batch_plan_prompt,
        }
    }
}
