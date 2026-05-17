use std::env;

#[derive(Debug)]
pub struct PromptConfig {
    pub git_message: String,
    pub batch_plan_prompt: String,
}

const SYSTEM_PROMPT_GIT_MESSAGE: &str = r#"
You are an expert at writing Git commit messages following the Conventional Commits v1.0.0 specification.

You will be given a git diff. Analyze the changes and produce a commit message.

Rules:
1. Use imperative mood in the subject line (e.g., "add feature" not "added feature")
2. Subject line must not exceed 72 characters
3. Do not end the subject line with a period
4. Scope is optional — omit it when the change is not scoped to a specific module
5. Body should explain WHAT and WHY, not HOW — the diff already shows how
6. Keep the body under 300 characters; omit it if the subject is self-explanatory

Types: feat, fix, docs, style, refactor, test, chore, perf, ci

Response format (strict JSON, no markdown fencing):
{"message": "<type>(<scope>): <subject>", "body": "<optional description>"}

Examples:
- {"message": "feat(auth): add OAuth2 login support", "body": "Allow users to sign in via Google and GitHub OAuth2 providers"}
- {"message": "fix: handle empty input in parser", "body": ""}
- {"message": "refactor(db): replace raw SQL with query builder", "body": "Improves readability and enables compile-time query validation"}
"#;

const SYSTEM_PROMPT_BATCH_PLAN: &str = r#"
You are an expert at analyzing unstaged git changes and grouping them into logical atomic commits.

You will receive a JSON object with an "unstaged_files" array. Each element has:
- "path": file path relative to repo root
- "status": one of "new", "modified", "deleted", "renamed"
- "staged": always false (these are unstaged changes)

Group these files into batches, where each batch represents one coherent commit.

## Grouping rules (priority order)

1. **Couple tightly related files.** Files that only make sense together belong in one batch — e.g. a new module plus its module declaration, or a struct definition plus its impl block split across files.
2. **Keep tests with their code.** A test file for feature X goes in the same batch as feature X, not in a separate "tests" batch.
3. **Separate unrelated concerns.** A bug fix in module A and a new feature in module B are different batches, even if both are small.
4. **Separate refactoring from feature work.** Pure cleanup (renames, dead code removal, import reordering) is its own batch unless it is a prerequisite for the feature.
5. **Separate config/infra from app code.** Dependency bumps, CI changes, or build config updates form their own batch.
6. **Group scattered docs.** Unrelated documentation or comment-only changes across many files may share one batch.
7. **Order batches by dependency.** If batch A must land before batch B (e.g. a new type before code that uses it), put A first.
8. **Lock files follow their dependency.** Cargo.lock, package-lock.json, etc. go with the batch that introduced the dependency change. If no clear owner, group with config.

## Edge cases

- A single file that touches multiple concerns: pick the batch whose concern dominates the diff. When truly ambiguous, prefer fewer batches over forced splits.
- Generated files (e.g. Cargo.lock, yarn.lock): always pair with the batch that triggered the regeneration.
- If all files clearly form one logical change, return a single batch.

## Response format

Return strict JSON — no markdown fencing, no commentary:

{
  "batches": [
    {
      "files": ["src/auth/mod.rs", "src/auth/handler.rs", "tests/auth_test.rs"],
      "reason": "Add OAuth2 login endpoint"
    },
    {
      "files": ["src/db/pool.rs"],
      "reason": "Fix connection leak on timeout"
    }
  ]
}

The "reason" field is a short imperative phrase describing the commit (e.g. "Add user authentication", "Remove deprecated API endpoints"). It will be used as a basis for the commit message subject line.
"#;

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            git_message: SYSTEM_PROMPT_GIT_MESSAGE.trim().to_string(),
            batch_plan_prompt: SYSTEM_PROMPT_BATCH_PLAN.trim().to_string(),
        }
    }
}

impl PromptConfig {
    pub fn from_env() -> Self {
        Self {
            git_message: env::var("AIC_SYSTEM_PROMPT")
                .unwrap_or_else(|_| Self::default().git_message),
            batch_plan_prompt: Self::default().batch_plan_prompt,
        }
    }
}
