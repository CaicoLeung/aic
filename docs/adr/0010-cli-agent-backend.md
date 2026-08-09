# ADR 0010: CLI-agent backend (reuse a local agent's auth instead of an API key)

- **Status:** Accepted
- **Date:** 2026-08-09

## Context

aic's LLM capability was exclusively the `rig-core` API path: 12 providers
(`src/llm.rs`, the `Provider` registry + the `with_agent!` macro), each requiring
an `api_key` in `config.toml`. A user who already pays for a coding-agent CLI
— Claude Code, OpenAI Codex, GitHub Copilot, pi — and has it installed and
authenticated must **also** provision an API key to use aic. The pain is the
key, not the model quality: commit-message generation is a constrained task the
existing providers handle well.

Each of those CLIs already ships its own auth and exposes a **headless/print
mode** that takes a prompt on argv and prints the answer to stdout
(`claude -p`, `codex exec`, `pi -p`). aic can reuse that auth by shelling out in
print mode instead of making an API call.

## Decision

Add a second backend kind alongside the rig API path: a **CLI-agent backend**
that invokes an external coding-agent CLI in headless/print mode.

### Selection: the `command` field, not a magic backend name

CLI mode is selected by the `command` config field being set (non-empty). There
are **no reserved `backend` names** for CLI agents. This avoids a collision the
naïve design would have introduced: `claude` is already an **alias for the
Anthropic provider** in the registry (ADR 0003), so making `claude` mean "Claude
Code CLI" would silently break users who set `backend = "claude"` for the
Anthropic API. "`command` is set" is unambiguous and matches the **generic
command-template** shape (see below). A `command` set alongside an `api_key` is
rejected as contradictory — the two backends are mutually exclusive.

New optional config fields: `command`, `args` (template with a `{prompt}`
placeholder), `timeout_secs`. All optional, so existing configs are unchanged —
**no migration**.

### Generic command template, not one adapter per CLI

The backend is a single `CliSpec { command, args, timeout_secs }` carrying an
argv template with a literal `{prompt}` placeholder. Three presets
(`cli_preset("claude" | "codex" | "pi")`) are offered as snippets by `aic setup`
and the docs; any other CLI (Opencode, Copilot, OhMyPi, future ones) is covered
by a custom `command`/`args` with **zero new code**. This deliberately rejects
the "one hardcoded adapter per CLI" alternative — six adapters doing the same
text-in/text-out job is a maintenance treadmill.

### Dispatch: an enum, not `Box<dyn>`

`LLM::agent(...)` now returns a `Backend` enum (`Rig(LLMAgent) | Cli(CliAgent)`)
whose methods (`call`, `schema<T>`, `stream_typed_with_reasoning<T>`, `verify`)
dispatch per variant. The original grilling decision named a `LlmBackend` trait
with `Box<dyn>` dispatch; **refined to an enum** because `schema<T>` /
`stream_typed_with_reasoning<T>` are generic methods, and generic methods are
not object-safe. An enum keeps them monomorphized per backend with identical
behavior. `LLM` / `LLMAgent` (the rig path) are **unchanged**, so `setup.rs`'s
verify flow and the `with_agent!` macro are untouched — the CLI path is purely
additive. `LlmConfig::load()` decides `Cli` vs `Rig` from config.

### Headless/print only

The CLI is invoked strictly in print mode — never agentic/tool-use. aic feeds a
single prompt and reads stdout. No tool loop is ever allowed: an agent that can
run tools could act on prompt-injected instructions against the working tree.
Print mode removes that entire class of risk.

**Least-permission presets.** The promise above is enforced by the invocation
itself, not by trusting each CLI's default — every preset pins itself to a
text-only / read-only stance, because defaults differ and one (pi) is unsafe:

| Preset | Pinned flags | Why |
| --- | --- | --- |
| `claude` | `-p` (print) | `--dangerously-skip-permissions` is opt-in and print mode cannot prompt, so no privileged tool auto-executes. claude has no reliable `--no-tools` flag (`--allowedTools` is variadic and greedily consumes the prompt), so print mode's conservative default is the lever. |
| `codex` | `exec -s read-only` | `exec` runs non-interactively; the sandbox is pinned to `read-only` so model-generated shell commands cannot write, even if a global config widens the default. `--dangerously-bypass-approvals-and-sandbox` is opt-in. |
| `pi` | `--no-tools -p` | **Required.** pi enables `read/bash/edit/write` tools by default; in print mode on a *trusted* project it can auto-run them (it cannot prompt) — effectively yolo. `--no-tools` disables all tools so print mode is genuinely text-only. |

Custom `command`/`args` backends are the user's responsibility to harden; the
presets are the safe defaults.

### Typed output via prompt-for-JSON + lenient parse

The commit-message and batch-plan paths need typed JSON. The CLI backend does
**not** use rig's schema-enforced `prompt_typed`; instead it relies on the
existing system prompts (which already specify the exact JSON shape with
examples), appends a "respond with ONLY the JSON" reminder, runs the CLI, and
tolerant-parses the output with `parse_json_response` — the same helper the
batch-plan API path already uses for its streamed text. This unifies the two
paths on one accepted lenient-parse pattern.

### Prompt-injection boundary

Untrusted content (the diff / file body) is wrapped in
`<aic_input>…</aic_input>` with a directive that it is data, never instructions.
Output is parsed into a struct and only ever printed for review; it is never
executed, and `confirm_before_commit` still gates the commit. Print-only (above)
already rules out command-execution attacks; the boundary shrinks the
output-injection surface and makes malformed output fail loudly.

### Reliability

- `CommandRunner` is an `async_trait` (`TokioRunner` real, `FakeRunner` in
  tests) so the arg-substitution / fence-strip / parse / retry / error-mapping
  glue is unit-tested without spawning real CLIs.
- Subprocess is capped at `timeout_secs` (default 60) and killed on timeout via
  `kill_on_drop`.
- `LlmError` classifies `CliNotInstalled` / `CliNotAuthenticated` / `Timeout` /
  `NonZeroExit` with human hints — never a raw panic.
- Retry policy: **one retry max** on a parse failure (re-running a full CLI
  agent is expensive); infrastructure errors propagate immediately.
- Streaming/reasoning: print mode is single-shot, so `on_reasoning` is accepted
  (to share the call site) but never fires — the "Analyzing changes" spinner
  goes quiet under the CLI backend. Reasoning was always cosmetic only.

## Consequences

- Users with an authenticated local agent can use aic with **no API key**.
- The rig API path and all 12 providers are untouched; no regression surface.
- `aic-web` is unaffected: it parses the `Provider` enum and `default_model()`
  arms, which are unchanged.
- Presets are best-effort against external CLIs whose flags can change; the
  `custom` `command`/`args` escape hatch is the reliable fallback.
