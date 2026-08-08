# Recording the aic demo

The README GIF/asciinema cast is recorded from **one command**. Because the
demo is deterministic and offline by default, re-recording is painless and the
output never drifts.

## Prerequisites

```bash
cargo build            # builds the aic binary the script runs
# optional, for an animated cast:
brew install asciinema # or: pipx install asciinema, cargo install asciinema
```

## One-command record (offline, deterministic)

```bash
asciinema rec --command "scripts/demo.sh" demo.cast
# then convert to a GIF for the README, e.g.:
#   agg demo.cast docs/demo/aic-demo.gif      # https://github.com/asciinema/agg
```

`scripts/demo.sh` defaults to **offline fixture mode** (`AIC_FIXTURE_DIR`), so
the cast is identical on every machine and every run: no network, no API key,
no local model. That is what keeps the README demo from rotting. aic reads
recorded responses from `examples/fixtures/fixtures.json` but still runs its
real parse → validate → stage-by-hunk → commit pipeline, so the cast shows the
actual splitting behavior, not a replay of hardcoded commits.

## Live re-record (real model)

To show a real model doing the splitting — e.g. when refreshing the fixtures or
demonstrating a new provider — point the script at a local Ollama server:

```bash
ollama pull llama3.3          # one-time
scripts/demo.sh --live
# or pin a different model:
AIC_DEMO_MODEL=qwen2.5 scripts/demo.sh --live
```

The live run writes a throwaway Ollama `config.toml` (per ADR 0008, aic reads
only the config file — no env vars) and is **not** deterministic (model output
varies), so only record the README cast from the default offline mode. Use
`--live` to eyeball a real model or to regenerate `examples/fixtures/` if the
sample repo's diff ever changes.

## What the demo proves

One file carrying three unrelated concerns (a `fix`, a `feat`, a `style`) is
split into **three atomic Conventional-Commits** at the hunk level — the
behavior that distinguishes aic from file-level committers. See
[`examples/README.md`](../../examples/README.md) for the edit/hunk/commit
mapping.
