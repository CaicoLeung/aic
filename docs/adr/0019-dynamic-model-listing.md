# ADR 0019: Dynamic model listing — no hardcoded model data

- **Status:** Accepted
- **Date:** 2026-10-28

## Context

`Provider::default_model()` and `Provider::models()` held a curated, hardcoded
model table: one default plus a short picker list per provider, refreshed only
when a maintainer remembered. The lists lagged provider releases, and
OpenRouter / OpenAI-compatible degraded to bare text input because no fixed
list fits them. The setup wizard (ADR 0007) can instead ask the provider what
models exist: every major API exposes a model-listing endpoint, and the wizard
already runs inside a Tokio runtime with a nested-runtime pattern for probes
(`verify.rs::probe`).

A spike against rig 0.41 (the crate already in the tree) established:

- rig ships `rig::client::ModelListingClient` (`list_models()`), unconditional,
  no feature flag, with per-provider listers for OpenAI (`GET /v1/models`),
  Anthropic (`GET /v1/models`, cursor-paginated), Gemini
  (`GET /v1beta/models`, page-token loop), DeepSeek, Mistral, OpenRouter,
  Ollama — plus the OpenAI-compatible path (openai client against a custom
  base URL).
- Groq, xAI, Perplexity, Together declare `ModelListing = Nothing` in rig —
  no listing capability — but all four follow the OpenAI-compatible
  `GET {base}/models` convention (or answer 404, which the fallback absorbs).

## Decision

1. **Fetch live model lists at the Model step.** On entering `step_model`, the
   wizard calls the selected provider's listing endpoint under the existing
   spinner/nested-runtime pattern, with a hard 5 s cap covering any internal
   pagination. The seam is one injected closure bundle (`SetupDeps`, the
   wizard's first seam, mirroring `RunDeps`) so `cargo test` stays hermetic —
   scripted stub queues, zero network.
2. **Provider → endpoint mapping:**

   | Provider | Path | Endpoint |
   |---|---|---|
   | OpenAI | rig | `GET https://api.openai.com/v1/models` |
   | Anthropic | rig | `GET https://api.anthropic.com/v1/models` (paginated) |
   | Gemini | rig | `GET https://generativelanguage.googleapis.com/v1beta/models` (paginated, key param) |
   | DeepSeek | rig | `GET https://api.deepseek.com/models` |
   | Groq | direct | `GET https://api.groq.com/openai/v1/models` |
   | Ollama | rig | rig's Ollama lister (local daemon) |
   | xAI | direct | `GET https://api.x.ai/v1/models` |
   | Mistral | rig | `GET https://api.mistral.ai/v1/models` |
   | OpenRouter | rig | `GET https://openrouter.ai/api/v1/models` |
   | Perplexity | direct | `GET https://api.perplexity.ai/models` |
   | Together | direct | `GET https://api.together.xyz/v1/models` |
   | OpenAI-compatible | rig | `GET {base_url}/models` |

   Custom base URLs (Ollama, OpenAI-compatible) are honored by building the
   fetch client with `.base_url()`; the four direct arms hardcode their
   canonical hosts (they have `BaseUrlRequirement::None`, so no override
   exists).
3. **reqwest becomes a direct dependency** (`default-features = false`) for
   exactly the four direct arms. It is already compiled into the tree via rig
   and self_update (0.13, rustls), and its MIT/Apache-2.0 licenses are
   pre-cleared in `deny.toml` — promotion adds no crate and no TLS backend.
4. **Failure is always a silent degrade, never a block.** Fetch failure,
   timeout, offline, missing key, 401/403 — any error falls back to the
   required free-text model prompt (the existing `models.is_empty()` path).
   The wizard never errors, never retries on its own.
5. **Session cache.** Fetched lists are cached in the wizard draft, keyed by
   provider + base URL + masked key; any change re-fetches.
6. **Large lists truncate.** First 50 entries alphabetically plus
   `Custom model…` — no search, no pagination.
7. **The curated tables are deleted**, and with them the *Default Model*
   concept: a model is explicitly required for every provider, and the config
   must record it. A config without `model` fails resolution with a "run
   `aic setup`" hint. Breaking for existing configs that relied on the
   default; tagged `BREAKING CHANGE`. The provider-flow `Done` gates on a
   model being set (routes into the Model step), while top-level
   `Save & exit` stays ungated — a CLI-agent-only user never sets a model.

## Consequences

- **Positive:** the picker always reflects the provider's live catalog
  (OpenRouter's thousands included, capped); zero maintenance burden for
  model data; OpenRouter/OpenAI-compatible gain a picker for the first time.
- **Negative:** offline first-runs fall back to typed model names (acceptable:
  typing a model id is the pre-0019 status quo for two providers); a network
  touch occurs at setup time (bounded at 5 s); the four direct arms are a
  second HTTP code path to maintain (one match arm each, absorbed by the
  shared seam).
- **Neutral:** `Provider::default_model()`/`models()` and their pinning tests
  are deleted; rig's pagination details (Anthropic cursor, Gemini tokens) stay
  inside rig behind the 5 s cap.
