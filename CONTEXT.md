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

### Resume

**Run state**:
The persisted snapshot of an interrupted batch-plan Run, written to `.aic/active.json` (gitignored, auto-ensured). Holds the frozen Batch plan, the per-file captured diffs, a content Fingerprint of every planned file, the HEAD oid at plan time, and each Batch's progress. Its presence is the resume-available signal; it is deleted on clean completion.
_Avoid_: checkpoint, save file, session state

**Resume**:
Replaying a Run state's pending Batches from the frozen snapshot — staging and committing exactly as the live Run would — without re-planning or re-capturing diffs. Triggered by auto-detect on a fresh Run (with a y/n offer) or the `--resume` flag; suppressed by `--no-resume`.
_Avoid_: recover, restore, continue

**Deferred batch**:
A Batch skipped during Resume because one of its files drifted since plan time (Fingerprint mismatch). Its change is left unstaged and never lost; the rest of the Run still completes. The user re-runs `aic` to plan the deferred change fresh.
_Avoid_: skipped batch, dropped batch, failed batch

**Fingerprint**:
The hex SHA-256 of a worktree file's bytes, captured at plan time and recomputed on Resume. A mismatch between the stored and current Fingerprint is what defers a Batch — it guards against replaying a stale diff snapshot.
_Avoid_: hash, checksum, signature

**Run log**:
The permanent append-only timeline at `.aic/run.log` (gitignored) — one timestamped line per plan/commit/failure/resume event across all Runs, for post-mortem auditing. Best-effort; never load-bearing.
_Avoid_: history, audit trail, journal
