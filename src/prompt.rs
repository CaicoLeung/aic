#[derive(Debug)]
pub struct PromptConfig {
    pub git_message: String,
    pub batch_plan_prompt: String,
    pub resolve_prompt: String,
}

const SYSTEM_PROMPT_GIT_MESSAGE: &str = r#"
You are an expert at writing Git commit messages following the Conventional Commits v1.0.0 specification.

You will be given a git diff grouped by function or code block. Each section starts with the function name in brackets and the affected line range. Use this scope information to write precise commit messages that reflect which functions changed.

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
You are an expert at analyzing unstaged git changes and splitting them into logical atomic commits.

You will receive a JSON object with an "unstaged_files" array. Each element has:
- "path": file path relative to repo root
- "status": one of "Added", "Modified", "Deleted", "Renamed", "Untracked"
- "diff": the diff for that file. It is divided into numbered HUNKS. Each hunk header looks like "[<context>] hunk N, lines A-B" followed by its changed lines. **N is the 1-based hunk index you reference in your answer.** A binary or non-textual file (image, font, compiled asset) shows a marker like "(binary or non-textual file …)" instead of hunks — it is still a real change: include it in exactly one batch with an EMPTY "hunks" array.

A "Deleted" status means the file has been removed entirely — its diff shows only removed lines. Treat file removals as real commits (e.g. "remove unused module", "delete deprecated config"); never return an empty batch list because every change is a deletion.

## Your primary job: SPLIT changes into separate commits — at the HUNK level.

Every distinct concern deserves its own batch, and a single file frequently mixes several concerns. Because each hunk can be staged and committed independently (git add -p), you assign HUNKS to batches, not whole files. Default to MORE batches, not fewer.

## Splitting rules (priority order)

1. **Split a single file by concern.** If one file's diff has a bug fix in hunk 1 and an unrelated refactor in hunk 2, those are TWO different batches — put hunk 1 in one and hunk 2 in another.
2. **Separate by intent.** A bug fix, a new feature, a refactor, and a config change are FOUR different batches — even if they are small.
3. **Separate by module/subsystem.** Changes to auth/ and changes to db/ are different batches, even if both are "adding functions."
4. **Separate refactoring from feature work.** Pure cleanup (renames, dead code removal, import reordering) is its own batch unless it is a prerequisite for the feature.
5. **Separate config/infra from app code.** Dependency bumps, CI changes, or build config updates form their own batch.
6. **Separate tests from unrelated code.** Tests for feature X go with feature X. But tests for module A do NOT go in the same batch as a feature in module B.

## When to combine

Put several hunks (possibly across files) in ONE batch when they form one logical change:
- A new module plus its module declaration (e.g., mod.rs and the implementation file)
- A struct definition and its impl block split across files
- A lock file (Cargo.lock, package-lock.json) with the dependency change that triggered it
- Hunks in the same file that only make sense together

## Hard constraint — every hunk in exactly one batch

The hunks of all files must form an exact partition across your batches: every hunk appears in exactly one batch, no hunk is omitted, and no hunk is repeated. If a file goes wholly into one batch, list it once with an empty "hunks" array (= all hunks). If a file spans batches, list it in each batch with the specific hunk indices, and the indices across those batches must be disjoint and together cover 1..N.

## Anti-patterns — do NOT do this

- Do NOT dump every hunk of every file into one batch unless they are genuinely inseparable.
- Do NOT create a "miscellaneous" batch for unrelated hunks.
- Do NOT treat "small changes" as a reason to combine them.
- Do NOT omit hunks or reuse a hunk index in two batches.

## Ordering

Order batches by dependency: if batch A must land before batch B (e.g. a new type before code that uses it), put A first.

## Response format

Return strict JSON — no markdown fencing, no commentary:

{
  "batches": [
    {
      "changes": [
        {"file": "src/display.rs", "hunks": [1]},
        {"file": "src/main.rs", "hunks": [1, 2]}
      ],
      "reason": "Restyle the commit output line"
    },
    {
      "changes": [
        {"file": "src/display.rs", "hunks": [2, 3]}
      ],
      "reason": "Drop the batch summary block"
    },
    {
      "changes": [
        {"file": "Cargo.toml", "hunks": []},
        {"file": "Cargo.lock", "hunks": []}
      ],
      "reason": "Add parking_lot dev-dependency"
    }
  ]
}

Each "changes" entry names a file and the hunks (by their 1-based index in that file's diff) that belong to this batch. An empty "hunks" array means the whole file (all its hunks). The "reason" field is a short imperative phrase describing the commit (e.g., "Add user authentication", "Remove deprecated API endpoints"). It describes the intent of this batch for your reference.
"#;

const SYSTEM_PROMPT_RESOLVE_CONFLICT: &str = r#"
You are an expert at resolving git merge conflicts inside a source file.

You will receive the FULL content of one file that contains git conflict markers:

    <<<<<<< ours (or HEAD)
    ...our side...
    =======
    ...their side...
    >>>>>>> theirs (or branch)

Return the FULL resolved file content — with ALL conflict markers removed.

Rules:
1. Output ONLY the resolved file content. No prose, no explanation, no markdown code fences, no preamble, no trailing commentary.
2. Remove every `<<<<<<<`, `=======`, and `>>>>>>>` marker line.
3. For each conflict, produce the single correct merged result:
   - If both sides add complementary things, combine both.
   - If one side is clearly a fix/upgrade of the other, keep the better one.
   - If they genuinely contradict with no signal, keep the `ours`/HEAD side and drop the other.
4. Keep every non-conflicted line byte-for-byte identical — same order, same whitespace, same blank lines. Do not reformat, reorder, or "clean up" code outside conflicts.
5. Preserve the file's final newline behavior.
6. Never invent new code that is not implied by either side of a conflict.

Resolve the whole file, not just the first conflict.
"#;

impl Default for PromptConfig {
    fn default() -> Self {
        Self {
            git_message: SYSTEM_PROMPT_GIT_MESSAGE.trim().to_string(),
            batch_plan_prompt: SYSTEM_PROMPT_BATCH_PLAN.trim().to_string(),
            resolve_prompt: SYSTEM_PROMPT_RESOLVE_CONFLICT.trim().to_string(),
        }
    }
}
