# aic

aic is an AI-powered git commit tool: it reads a diff, drafts a conventional-commit message, and commits it. When nothing is staged it groups unstaged work into logical commits and commits each group.

## Language

**Run**:
One execution of the default commit workflow — either a single commit over staged files, or a batch plan over unstaged files.
_Avoid_: execution, invocation, session

**Batch**:
A group of files the LLM's split plan assigns to one commit. A Run contains one or more Batches; each Batch yields one Drafted Message and one commit.
_Avoid_: group, chunk, package

**Drafted Message**:
The conventional-commit message (with optional body) the LLM produces for a Batch's diff.
_Avoid_: suggestion, proposal, generated text

**Batch staging**:
The phase of a Run that maps a Batch's planned hunks onto the current index→workdir diff and stages them, tracking which original (plan-time) hunk indices have already landed. Staging re-reads the current diff rather than the plan-time snapshot, so a pre-commit hook that restages whole files (lint-staged/prettier) cannot desync later Batches of the same Run.
_Avoid_: group staging, chunk staging, add -p

**Block**:
A coherent atomic unit of change the deterministic grouping engine produces — a set of hunks (possibly across files) that belong in one commit. A Block is the engine's own vocabulary; one Block converts to one Batch when fed into the staging path. Carries a `BlockHeuristic` (why its hunks were joined) surfaced as the batch `reason`.
_Avoid_: group, chunk, cluster

**Block grouping**:
The deterministic, LLM-free phase (`src/grouping.rs`) that turns a workdir diff into atomic Blocks using two v1 heuristics: _adjacency_ (within one file, merge consecutive hunks separated by ≤ `adjacency_gap` unchanged lines, or sharing git's context header) and _same-scope_ (across files, merge per-file Blocks that share a parent directory). v1 is conservative: cross-file same-scope and root-scope auto-merge are OFF by default, adjacency is tight, and split files are left alone. The output is always a valid partition — every hunk lands in exactly one Block — so it feeds straight into `validate_batch_plan` and `Staging`. The production Run still uses the LLM planner; the engine is the deterministic foundation/fallback underneath it.
_Avoid_: auto-splitting, clustering, chunking

**Block heuristic**:
The reason the engine joined a Block's hunks: `Single` (lone hunk), `Adjacency` (small within-file gap), `SameContext` (shared function/section header), or `SameScope` (shared directory across files). Surfaced as each batch's `reason`.
_Avoid_: rule, strategy

**Commit confirmation**:
An opt-in phase (gated by the `confirm_before_commit` config option) that interrupts the commit path after the Drafted Message is produced and before the commit lands. The full Drafted Message plus the Batch's file list are shown, then a four-option menu is offered: **Commit**, **Re-generate**, **Edit**, **Abort**. Re-generate and Edit loop back to the same menu (re-showing the message); Commit proceeds to commit; Abort ends the Run with nothing further committed. Does not apply to the Finalize step, which uses git's default message.
_Avoid_: review prompt, pre-commit check, confirmation gate

**Re-generate**:
A Commit-confirmation menu action that re-runs the LLM on the same Batch diff to produce a fresh Drafted Message, then returns to the menu. A plain re-run — it does not feed critique back to the model.
_Avoid_: retry, redo, re-roll

**Message edit**:
A Commit-confirmation menu action that lets the user modify the full Drafted Message (subject + body) before committing. Opens `$VISUAL`/`$EDITOR` on a temp file (via the `edit` crate, which also falls back to nano/vim/vi/emacs when neither is set) — git-style. Returns to the menu after editing so the result can be re-verified.
_Avoid_: message tweak, inline edit (overloaded), editor step

**Provider**:
A named LLM backend the user can route a Run through (OpenAI, Anthropic, Gemini, DeepSeek, Groq, Ollama, xAI, Mistral, OpenRouter, Perplexity, Together, plus the generic OpenAI-compatible provider). Resolved per Run from env, then config, then default.
_Avoid_: backend, engine

**Base URL**:
The endpoint a Provider sends requests to. Optional and overridable (env/config); required for the OpenAI-compatible provider. Defaults to each Provider's canonical API host.
_Avoid_: endpoint, server, URL

**OpenAI-compatible provider**:
A generic Provider that routes through the OpenAI client against a user-supplied Base URL, for servers that speak the OpenAI chat-completions API (LM Studio, vLLM, gateways). Has no Default Model — the user must supply one.
_Avoid_: custom provider, generic provider, passthrough

**Default Model**:
The model used for a Provider when the user has not set one (env/config). Chosen for speed and cost, since aic's workload (commit messages) is lightweight.
_Avoid_: fallback model, base model

### Conflict resolution

**Conflict**:
A repository state where `Repository::state()` reports an active merge-style operation (`Merge`, `CherryPick`, `CherryPickSequence`, `Revert`, `RevertSequence`) and the index holds unmerged entries with conflict markers (`<<<<<<<`, `=======`, `>>>>>>>`) in working-tree files. aic resolves and finalizes these; it detects but refuses `Rebase`/`RebaseInteractive`/`RebaseMerge`/`ApplyMailbox` states.
_Avoid_: merge issue, clash, collision

**Resolution**:
The marker-free file content the LLM produces for one conflicted file. Approved Resolutions are written to the working tree and staged; rejected or skipped Resolutions leave the file untouched.
_Avoid_: fix, merged file, resolved content

**Finalize**:
The git operation that ends a Conflict after all its Resolutions are approved: `git commit` for a Merge, `git cherry-pick --continue` for a CherryPick, `git revert --continue` for a Revert. aic finalizes with git's default message — it does not call the LLM for a Finalize message.
_Avoid_: complete, finish, commit (overloaded — see Run, Drafted Message)

**Conflict module**:
The module (`src/conflict.rs`, reached as `Git::conflict() -> Conflict<'_>`) that owns conflict detection, classification, worktree I/O for conflicted files, and Finalize. Owns the `RepoState`, `ConflictKind`, `ConflictedFile` types and `has_conflict_markers`. The commit guards (`assert_commit_safe`, `verify_commit_clean`) stay on `Git` and call across this seam; `Git` keeps the repo handle, `run_git`, and the libgit2/CLI commit path.
_Avoid_: resolve module, merge module, conflict service
