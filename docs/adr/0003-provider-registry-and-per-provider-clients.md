# Provider support via a registry table and per-provider rig clients

Status: accepted

aic supports many LLM providers (OpenAI, Anthropic, Gemini, DeepSeek, Groq, Ollama,
xAI, Mistral, OpenRouter, Perplexity, Together, plus a generic OpenAI-compatible
escape hatch). Provider *identity* — canonical name, aliases, env-key variable,
default model, and setup label — lives in a single static registry table that drives
name resolution, default-model lookup, API-key discovery, and the `aic setup` menu.
Each provider's rig client is still constructed in a dedicated arm of the
`with_agent!` macro.

## Why not collapse all OpenAI-compatible providers onto one client

xAI, OpenRouter, Groq, Together, Perplexity and others all expose OpenAI-compatible
chat-completion endpoints, so they could in principle share rig's OpenAI client with
a per-provider `base_url`. We deliberately did **not** do that. aic depends on rig's
typed/structured extraction (`prompt_typed`) to produce `CommitOutput` and
`BatchPlanOutput`; the fidelity of structured output varies across OpenAI-compatible
endpoints. Keeping rig's native per-provider clients preserves reliable structured
output. The generic `openai-compatible` provider exists only as an explicit, opt-in
escape hatch for users who know their server is compatible (LM Studio, vLLM,
gateways) and accept that their endpoint must support structured output.

## Why aic owns the base URL instead of rig's built-in env vars

rig already reads `OPENAI_BASE_URL` / `OLLAMA_API_BASE_URL` from the environment.
We instead resolve endpoint base URLs through aic's own config (`base_url` field +
`LLM_BASE_URL` env, surfaced in `aic list`) so that a single config surface and the
uniform env > config > default precedence apply to every provider — including the
previously hardcoded Ollama URL (`localhost:11434`).

## Considered options

- **Per-provider enum + macro arms only** (status quo): adding a provider touched
  ~6 scattered call sites (`Provider` enum, `from_name`, `default_model`, `env_key`,
  setup list, macro arm). Rejected as the duplication that makes provider expansion
  painful.
- **Single generic OpenAI-compatible adapter for everything**: smallest code, but
  gambles on structured-output fidelity and discards rig's per-provider handling.
  Rejected; adopted only as the opt-in escape hatch.
- **Registry table + per-provider macro arms** (chosen): DRY for identity metadata
  (~2 edits per new provider: one registry row + one macro arm) while keeping
  reliable per-provider clients.

## Consequences

- Adding a provider is a one-row registry entry plus one macro arm.
- The `openai-compatible` provider has no default model and requires the user to
  supply a model (and base URL); OpenRouter likewise requires an explicit model
  slug.
- `base_url` is a first-class resolved config field and appears in `aic list`.
