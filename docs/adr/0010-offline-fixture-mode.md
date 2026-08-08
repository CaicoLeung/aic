# ADR 0009: Offline fixture mode for reproducible demos

- **Status:** Accepted
- **Date:** 2026-08-08
- **Related:** AIC-34, ADR 0008 (config single source of truth)

## Context

The v0.5.0 adoption sprint (AIC-31) needs a demo that any contributor or
reviewer can reproduce in seconds, and a README GIF that **never rots**. aic's
default Run calls an LLM twice per concern — once to split the diff into
batches, once per batch to write the message — so a recorded demo would either:

- require a live model (Ollama) — non-deterministic, and unavailable in CI; or
- require a cloud API key — not acceptable for a public, no-network trial.

Both rot: the GIF drifts as models change, and CI can't assert the demo at all.

A "replay the commit history" script is not an honest alternative — it would
run `git commit` with hardcoded messages and never exercise `aic`, so it would
demonstrate nothing about the tool.

## Decision

Add an **offline fixture mode** gated by a single env var, `AIC_FIXTURE_DIR`.

When set, the two LLM-shaped calls on the commit path are served from a recorded
manifest (`$AIC_FIXTURE_DIR/fixtures.json`) instead of calling a model:

- `Generator::split_patch_streaming` → the recorded `BatchPlanOutput`.
- `Generator::generate_commit_message` → the next recorded `CommitOutput`, in
  batch order (a process-local monotonic counter, one Run per process).

The seam lives in [`src/fixture.rs`](../../src/fixture.rs); the short-circuits
are two `if let Some(..) = fixture::serve_*()` guards in
[`src/generator.rs`](../../src/generator.rs). Fixture mode is **off** when the
env var is unset/empty, so production behavior is unchanged.

Fixture mode is a demo affordance, not a shortcut around the engine:

- The served plan still flows through `validate_batch_plan` against the **real**
  diff's hunk counts, so a stale fixture (wrong file or hunk count) is rejected
  loudly.
- A missing or malformed manifest is a **hard error** — fixture mode never
  silently falls through to a live LLM, which would surprise a no-network
  demo/CI run with an unexpected network call.

The demo (`scripts/demo.sh`) therefore runs the **real** `aic` binary in fixture
mode: aic parses the actual diff, validates the recorded plan against it, stages
each batch's hunks, and commits — the same code path as a live run, just with
the LLM responses recorded. This genuinely demonstrates the splitting behavior,
deterministically and offline.

## Consequences

- `scripts/demo.sh` runs end-to-end with no network, no API key, and no local
  model, deterministically producing ≥3 atomic Conventional-Commits. CI can
  assert it.
- The README GIF is recorded from the same script, so re-recording is one
  command and the output is stable.
- `--live` (Ollama, via a generated config file per ADR 0008) remains available
  for eyeballing a real model or refreshing fixtures when the sample repo's
  diff changes.
- Two extra `Clone` derives (`CommitOutput`) and one new module; no new CLI
  subcommand, provider, or engine change (Phase 2 freeze respected).
