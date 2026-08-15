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
The deterministic, LLM-free phase (`src/grouping.rs`) that turns a workdir diff into atomic Blocks using two v1 heuristics: _adjacency_ (within one file, merge consecutive hunks separated by ≤ `adjacency_gap` unchanged lines, or sharing git's context header) and _same-scope_ (across files, merge per-file Blocks that share a parent directory). v1 is conservative: cross-file same-scope and root-scope auto-merge are OFF by default, adjacency is tight, and split files are left alone. The output is always a valid partition — every hunk lands in exactly one Block — so it feeds straight into `validate_batch_plan` and `Staging`. The production Run's primary splitter is the LLM planner; when its plan fails validation, the Run falls back to `plan_from_diffs` (conservative defaults, repo order preserved), warns, and completes — the engine is the deterministic second producer behind the `BatchPlanner` seam, not dead code.
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

**Commit entry**:
The rendered card for one Batch — either the pending Commit preview (the `?`-marked draft) or the landed ✓ line. Both share the subject styling and the File stats footer; the footer shows the diff as it stands when rendered (staged for the preview, just-committed for the ✓ line), so a pre-commit hook that restages whole files can change the ✓ line's numbers.
_Avoid_: commit card, entry, item

**File stats footer**:
The per-file block under a Commit entry's Drafted Message — in the Commit preview and on the landed ✓ line — listing each file with its `+N`/`−M` line counts, a `[new]`/`[del]` tag for added/deleted files, and a `Σ` total line when more than one file. Counts describe the diff the commit will land (preview) or just landed (✓ line); binary files show `(binary)`.
_Avoid_: diff stats, file summary, stats block


**Run module**:
The module (`src/run.rs`) that owns the default commit workflow: `default_run` checks the repo state and routes — mid-Conflict it offers resolve-then-continue via the front door, otherwise it runs `commit_run`, the single-commit / Batch-plan spine (plan → staged Diff JSON → Drafted Message → commit). Deps arrive as two purposeful bundles, `RunDeps` (display, planner, messenger, confirm) and — on the conflicted route — `ResolveDeps`; `src/lib.rs` is the module root and `main.rs` only the thin dispatch (ADR 0015).
_Avoid_: workflow module, engine, orchestrator

**Diff JSON envelope**:
The JSON payload shape the LLM planner and messenger consume: per-file `{path, diff}` records (`files_json`, `src/diff_json.rs`) and the Batch-plan analysis envelope (`plan_batch_diff_json`). Pure functions over model inputs — no `&Git`, no I/O — so `Git` stays a mere adapter feeding them and the envelope shape is unit-testable on its own.
_Avoid_: diff payload, prompt body, JSON blob

**Provider**:
A named LLM service the API-provider Backend routes a Run through (OpenAI, Anthropic, Gemini, DeepSeek, Groq, Ollama, xAI, Mistral, OpenRouter, Perplexity, Together, plus the generic OpenAI-compatible provider). Resolved per Run from config, then default. Meaningful only on the API-provider Backend; the CLI-agent Backend ignores it.
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

### Backends

**Backend**:
How aic obtains LLM answers for a Run. Exactly two kinds exist — the API-provider Backend and the CLI-agent Backend — and only one is active at a time, named explicitly by the `backend_kind` config field (`"api"` or `"cli"`, absent ⇒ `"api"`). The inactive Backend's fields may be kept in the config as dormant state (preserved across switches); `backend_kind` is authoritative and the dormant fields are ignored at run time (ADR 0011). "Which mode am I in" = "which Backend is active."
_Avoid_: mode, engine, provider (overloaded — see Provider)

**API-provider Backend**:
The Backend that calls a Provider over HTTP, authenticated by an `api_key`. The original path; the default when `backend_kind` is unset.
_Avoid_: api_key mode (`api_key` is the credential, not the Backend), API mode, rig path

**CLI-agent Backend**:
The Backend that shells out to an external coding-agent CLI (`claude`, `codex`, `pi`, …) in headless/print mode and reuses that CLI's own auth, so no `api_key` is needed. Selected by `backend_kind = "cli"` with a `command`/`args`/`timeout_secs` template (ADR 0010; selection mechanism superseded by ADR 0011).
_Avoid_: cli_agent mode, command mode

### Config

**Config file location**:
The on-disk path aic reads and writes the config from: `~/.config/aic/config.toml`, resolved from `dirs::home_dir()` (not `dirs::config_dir()`) so the path is identical on macOS, Linux, and Windows and matches what the README and module docs have always claimed (ADR 0012). ADR 0008 still holds: the config file is the single source of truth.
_Avoid_: config dir, app-support path, settings path

**Location migration**:
The one-time move (`Config::migrate_location`, ADR 0012) of a pre-0012 config written to the old OS-native default (`dirs::config_dir()`, i.e. `~/Library/Application Support/aic/config.toml` on macOS) into the Config file location. Copy old → new, then delete old (move semantics); **skipped silently when the new file already exists** (new wins); a no-op when the two paths coincide (plain Linux) or the old file is absent. Runs every startup, idempotent; prints a one-line notice only when it actually moves a file. Distinct from preset migration.
_Avoid_: config move, relocation, migration (overloaded — see Preset migration)

**Preset migration**:
What `Config::migrate_if_stale` does — rewrites a stale CLI-agent preset's `args` _in place_, at the same file path. Improves a preset snapshot frozen at setup time (e.g. claude before `stream-json`). Never moves the file. The older of the two "migrations"; keep the qualifier when prose could confuse them.
_Avoid_: args migration, preset refresh

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
The module (`src/conflict.rs`, reached as `Git::conflict() -> Conflict<'_>`) that owns conflict detection, classification, worktree I/O for conflicted files, Finalize, and the resolve-flow status UI — the free functions over `Display` (`conflict_detected`, `handoff`, `refused`, …) that paint the resolve workflow's lines. They render through the Display module's `styled`/`emit` primitives, so the two surfaces share one seam and margin while the vocabulary stays with the domain. Owns the `RepoState`, `ConflictKind`, `ConflictedFile` types and `has_conflict_markers`. The commit guards (`assert_commit_safe`, `verify_commit_clean`) stay on `Git` and call across this seam; `Git` keeps the repo handle, `run_git`, and the libgit2/CLI commit path.
_Avoid_: merge module, conflict service

**Resolve module**:
The module (`src/resolve.rs`) that owns the `aic resolve` workflow: `resolve_run` walks the Conflict's files, shows each proposed Resolution's unified diff, applies approvals, and Finalizes — parameterized by one `ResolveDeps` bundle (resolve, prompt, display). `default_run` in the Run module hands its whole `ResolveDeps` over when the user accepts `resolve now?`; the dependency runs one way (run → resolve).
_Avoid_: resolution module, conflict workflow (the detection/Finalize half lives in the Conflict module)

### Progress

**Palette**:
The module (`src/palette.rs`) where every color decision lives: the WCAG-guarded named/fallback palettes keyed by Commit Type (`CommitType::color_for` is defined here, next to the data), the single-source neutral-gray/commit-id/sigma accessors, and the terminal-16 role functions (`success`, `pending`, `caution`, `added`, `removed`, `hint`, …) that replaced ~30 ad-hoc `Style::new()` chains across the commit panel and resolve UI. Renderers call roles, never hand-roll styles.
_Avoid_: color utils, theme

**Commit Type**:
The vocabulary module (`src/commit_type.rs`): the `CommitType` enum (the Conventional Commits types plus community additions), its string parsing, and `ParsedMessage` — the single decomposition of a subject line. Purely lexical; colors resolve through the Palette.
_Avoid_: conventional type, types module (that's the seam aliases in `src/types.rs`)

**Reasoning feed**:
The Run's streaming-reasoning display driver (`src/reasoning_feed.rs`): owns the *when-to-paint* policy between a streaming LLM call and a rendering sink. A rolling reasoning window redraws in place as the model thinks and is erased when thinking ends, so nothing lingers in the scrollback; when a backend emits no reasoning deltas at all (a CLI agent in print mode, or an API cold start) a loading frame keeps the screen alive and, past the loading grace, shows an explanatory notice. The driver runs against a `ReasoningSink` trait (concretely `progress::ReasoningRenderer` in production, a recording fake in tests), keeping the byte-level frame/row assembly in `progress` separate from the frame-policy decisions here. Both the BatchPlan analysis and the Drafted Message generation stream through one `drive_streaming` helper at the production wiring layer.
_Avoid_: reasoning service, thinking component, streaming view

**Markdown renderer**:
The pure line→styled-rows painter (`src/markdown.rs`) that turns a partial Markdown reasoning window into ANSI-styled terminal rows — ADR 0013's renderer contract. Line-local classification (`classify_line` + running fence state), inline `**bold**`/`` `code` `` parsing, syntect code highlighting, and the generic `wrap_runs` wrap engine; `reasoning_rows`/`loading_rows` are its front doors. Zero I/O and no `&Git`-style state: every function takes model inputs and returns rows, so the whole pipeline is unit-testable on its own. The when-to-paint policy lives in the Reasoning feed; the terminal surface that frames these rows is `progress::ReasoningRenderer`.
_Avoid_: markdown engine, TUI (ADR 0013 bans a framework), pretty printer
