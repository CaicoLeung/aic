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

> ⚠️ **Selection mechanism superseded by [ADR 0011](0011-explicit-backend-discriminator.md):** the active Backend is now chosen by an explicit `backend_kind` field, not by "`command` is set." Everything else in this ADR stands.

CLI mode is selected by the `command` config field being set (non-empty). There
are **no reserved `backend` names** for CLI agents. This avoids a collision the
naïve design would have introduced: `claude` is already an **alias for the
Anthropic provider** in the registry (ADR 0003), so making `claude` mean "Claude
Code CLI" would silently break users who set `backend = "claude"` for the
Anthropic API. "`command` is set" is unambiguous and matches the **generic
command-template** shape (see below). The two backends' fields may coexist
in the file: `backend_kind` selects the active one and the other's fields are
kept **dormant** (preserved across switches, ignored at run time), so
switching never wipes what was entered for the other — see ADR 0011. (The
earlier "`command` + `api_key` is rejected as contradictory" rule was
superseded by dormant fields.)

New optional config fields: `command`, `args` (template with a `{prompt}`
placeholder), `timeout_secs`. All optional, so existing configs are unchanged —
**no migration**.

### Generic command template, not one adapter per CLI

The backend is a single `CliSpec { command, args, timeout_secs }` carrying an
argv template with a literal `{prompt}` placeholder. Presets are data, not
adapters: the preset names live in one registry ([`PRESETS`], resolved by
`cli_preset`), and adding one is a single match arm plus an `Encoding` choice —
omp needed zero new decoder code because it reuses pi's. A CLI without a
preset is still covered by a custom `command`/`args` with **zero new code**.
This deliberately rejects the "one hardcoded adapter per CLI" alternative —
many adapters doing the same text-in/text-out job is a maintenance treadmill.

### Dispatch: an enum, not `Box<dyn>`

`LLM::agent(...)` now returns a `Backend` enum (`Rig(LLMAgent) | Cli(CliAgent)`)
whose methods (`call`, `schema<T>`, `stream_typed_with_reasoning<T>`, `verify`)
dispatch per variant. The original grilling decision named a `LlmBackend` trait
with `Box<dyn>` dispatch; **refined to an enum** because `schema<T>` /
`stream_typed_with_reasoning<T>` are generic methods, and generic methods are
not object-safe. An enum keeps them monomorphized per backend with identical
behavior. The `LLMAgent` type and the `with_agent!` macro are unchanged, so the
provider registry and per-provider clients are untouched — the CLI path is
additive to them. Backend selection moved from the old `LLM::load()`
constructor to `LlmConfig::load()`, which reads `backend_kind` and returns
`Rig` or `Cli`; the call sites in `generator.rs` use `LlmConfig::agent()` and
are backend-agnostic.

### Headless/print only

The CLI is invoked strictly in print mode — never agentic/tool-use. aic feeds a
single prompt and reads stdout. No tool loop is ever allowed: an agent that can
run tools could act on prompt-injected instructions against the working tree.
Print mode removes that entire class of risk.

**Least-permission presets.** The promise above is enforced by the invocation,
not by tool-use mode: every preset runs single-shot print mode, where no TTY
exists to answer an approval prompt. On top of that shared guarantee, presets
pin explicit flags where the CLI's defaults need it — defaults differ and one
(pi) is unsafe even in print mode:

| Preset | Pinned flags | Why |
| --- | --- | --- |
| `claude` | `-p` + `--output-format stream-json --include-partial-messages` | `-p` is print mode (cannot prompt). The `stream-json` flags surface claude's `thinking_delta` as a live reasoning stream — without them, plain `-p` returns only the final answer and the batch-plan reasoning window stays empty. claude has no reliable `--no-tools` flag (`--allowedTools` is variadic and greedily consumes the prompt), so print mode's conservative default (it cannot prompt → no privileged tool auto-executes; `--dangerously-skip-permissions` stays opt-in) is the lever. The NDJSON envelope is decoded centrally, so the typed paths still receive plain JSON text. |
| `codex` | `exec -s read-only` | `exec` runs non-interactively; the sandbox is pinned to `read-only` so model-generated shell commands cannot write, even if a global config widens the default. `--dangerously-bypass-approvals-and-sandbox` is opt-in. |
| `pi` | `--no-tools -p` | **Required.** pi enables `read/bash/edit/write` tools by default; in print mode on a *trusted* project it can auto-run them (it cannot prompt) — effectively yolo. `--no-tools` disables all tools so print mode is genuinely text-only. |

Custom `command`/`args` backends are the user's responsibility to harden.

The remaining presets pin **no** flags — deliberately. They rely on headless
print mode itself: with no TTY, approval-gated tools cannot run (gemini,
copilot, trae, qwen); cursor runs untrusted (writes disabled) because
`--trust` is omitted; opencode and omp expose no equivalent of pi's
`--no-tools`, so like claude their text-only stance rests on print mode's
conservative default. This weakening of "pin flags everywhere" is accepted
here on purpose: per-CLI permission flags drift between CLI versions, while
the headless default is shared by all eleven and cannot regress per CLI.

The `aic setup` wizard offers **only the presets** ([`PRESETS`]). The five
with a decodable stdout envelope (claude, codex, pi, opencode — and omp, which
reuses pi's decoder) get reasoning streamed or extracted where the CLI exposes
it; the rest are plain print mode (stdout IS the answer). A hand-edited custom
`command`/`args` still runs, but in plain-text mode with no reasoning feed and
no envelope decoding; it is the config-edit escape hatch for a CLI without a
preset, not a wizard option.

### Preset auto-migration

A preset improvement (e.g. claude's switch to `stream-json` for a live
reasoning feed) reaches existing users via `Config::migrate_if_stale`: on every
load, a CLI-backend config whose `(command, args)` is byte-identical to a known
*legacy* preset snapshot is rewritten to that preset's current `args` (only
`args`; `command`, `timeout_secs`, `backend_kind`, and all API fields are
preserved). It is idempotent (a migrated config matches no legacy fingerprint on
the next run) and conservative (a customized command matches no fingerprint and
is left alone), with a stderr notice so the rewrite is transparent. This keeps
stale preset snapshots from stranding users on args they set once and forgot.

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
- Subprocess is capped at an **idle** `timeout_secs` (default 240 — sized for a
  local reasoning CLI, not the API path; see `DEFAULT_TIMEOUT_SECS`) and killed
  via `kill_on_drop` when it fires. The timeout is **not a wall-clock cap**: it
  resets on every line the CLI emits, so an actively-streaming agent runs
  unbounded and only a fully silent one (no stdout/stderr for the whole
  `timeout_secs`) surfaces `Timeout`. This is the right semantics for a local
  reasoning CLI whose latency on a real diff dwarfs an API call — a hard
  deadline would kill a healthy, actively-thinking agent.
- `LlmError` classifies `CliNotInstalled` / `CliNotAuthenticated` / `Timeout` /
  `NonZeroExit` with human hints — never a raw panic.
- Retry policy: **one retry max** on a parse failure (re-running a full CLI
  agent is expensive); infrastructure errors propagate immediately.
- Streaming/reasoning: the CLI's stdout/stderr are streamed **live**,
  line-by-line, into `on_reasoning` as they arrive (two reader tasks forward
  each complete line over a channel; the main loop resets the idle timer per
  line and feeds the callback). This mirrors the API path's reasoning stream
  so the "Analyzing changes" window shows the model's thinking process under
  the CLI backend too — the prior "print mode is single-shot, so `on_reasoning`
  never fires" design left the UI silent for the CLI's whole run.

## Consequences

- Users with an authenticated local agent can use aic with **no API key**.
- The rig API path and all 12 providers are untouched; no regression surface.
- `aic-web` is unaffected: it parses the `Provider` enum and `default_model()`
  arms, which are unchanged.
- Presets are best-effort against external CLIs whose flags can change; the
  `custom` `command`/`args` escape hatch is the reliable fallback.
