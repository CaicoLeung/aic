# ADR 0008: Config file is the single source of truth (drop provider env vars)

- **Status:** Accepted
- **Date:** 2026-08-08

## Context

aic resolves four provider fields — `backend`, `api_key`, `model`, `base_url` — at
runtime. Before this decision, each field followed a four-tier precedence: generic
env var (`LLM_BACKEND` / `LLM_API_KEY` / `LLM_MODEL` / `LLM_BASE_URL`) >
provider-specific env var (`OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, …) > config file
> built-in default.

This caused a recurring class of **silent-override** bugs:

- A user ran `aic setup`, chose a model, and it was saved to `config.toml` correctly
  — but an `LLM_MODEL` exported elsewhere in their shell won at runtime, so
  *"the custom model I enter in setup doesn't take effect; the old model is still
  used."*
- A stale API key in `config.toml` was masked by a valid `DEEPSEEK_API_KEY` env var,
  so it *"worked"* until the env var was removed.

`aic setup` was already the intended configuration surface, but env vars could
silently override what it wrote, so the file was never truly authoritative. The
verify step in setup even probed the wrong value and reported spurious 401s.

## Decision

`aic` now reads **only** the config file (`~/.config/aic/config.toml` /
`~/Library/Application Support/aic/config.toml`). All provider-configuration
environment variables are removed:

- Generic: `LLM_BACKEND`, `LLM_API_KEY`, `LLM_MODEL`, `LLM_BASE_URL`
- Provider-specific: `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `DEEPSEEK_API_KEY`, …
  (all of them)
- Vestigial: `AIC_SYSTEM_PROMPT` (documented but never actually read)

Field resolution collapses from `env > config > default` to **`config > default`**.
The `Source::Env` tier is deleted from `ResolvedConfig`; per-provider registry
metadata switches from `env_key: Option<&str>` to `requires_key: bool`; `LLM::load`
replaces the misnomer `LLM::from_env`. This supersedes the env-var portions of
[ADR 0003](0003-provider-registry-and-per-provider-clients.md) — the registry-table
pattern itself is unchanged.

## Consequences

- **Positive:** What `aic setup` saves is exactly what `aic` uses. The entire class
  of silent-override bugs is removed, and `aic list` reports the real runtime source
  for every field.
- **Negative / breaking:** Users who relied on environment variables for any field
  must persist those values into the config file once — `aic setup` (which pre-fills
  from existing config) or editing the file. Users who configured everything via
  `aic setup` are unaffected.
- **Positive:** The smoke test (`scripts/smoke-test-providers.sh`) now writes a
  per-provider `config.toml` into a throwaway `HOME` and invokes the binary with
  every `LLM_*` var unset, proving the config file is the sole input.
- The migration is one-time and `aic setup` guides it; resolution for config-file
  users is unchanged.
