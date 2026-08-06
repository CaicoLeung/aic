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

**Commit confirmation**:
An opt-in phase (gated by the `confirm_before_commit` config option) that interrupts the commit path after the Drafted Message is produced and before the commit lands: the full Drafted Message plus the Batch's file list are shown, then the user is prompted to proceed. A decline aborts the Run — nothing in the current Batch commits, already-committed Batches stay, and remaining work stays recoverable in the working tree. Does not apply to the Finalize step, which uses git's default message.
_Avoid_: review prompt, pre-commit check, confirmation gate

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
