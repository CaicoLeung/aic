# Recording / re-recording the demo

`scripts/demo.sh` is the single source of truth for the headline demo (aic
splits one file's mixed edits into **block-level atomic commits**). Recording or
re-recording the GIF / asciinema cast for the README is one command.

## One-command record (offline, deterministic)

```bash
scripts/demo.sh                       # default: no network, deterministic fixture
asciinema rec docs/demo/aic-hunk-split.cast --command scripts/demo.sh
agg docs/demo/aic-hunk-split.cast docs/demo/aic-hunk-split.gif   # https://github.com/asciinema/agg
```

The default run is **offline**: `scripts/demo.sh` replays the recorded history
from [`examples/before-after/after.txt`](../../examples/before-after/after.txt),
so the cast is identical on every machine and never drifts. That is what keeps
the README demo from rotting.

## Live recording (real model)

To capture a real model doing the split — e.g. when refreshing the fixture or
demonstrating a provider — point the script at a provider:

```bash
cargo build --release                 # scripts/demo.sh runs target/release/aic

# Local Ollama (no API key):
ollama serve &; ollama pull qwen2.5-coder:7b
AIC_DEMO_LIVE=1 scripts/demo.sh

# Or any remote provider (DeepSeek, OpenAI, Groq, ...):
AIC_DEMO_LIVE=1 AIC_DEMO_PROVIDER=deepseek AIC_DEMO_KEY=sk-... \
  AIC_DEMO_MODEL=deepseek-v4-flash scripts/demo.sh
```

`scripts/demo.sh` writes an **isolated** aic config to a throwaway `$HOME` for
the live run — your real `~/.config/aic/config.toml` is never touched. (aic
reads its provider from the config file only; env-var overrides were removed in
the config-single-source-of-truth refactor, so the script passes the provider
via `AIC_DEMO_*` knobs that it translates into that isolated config.)

If the provider is unreachable or `aic` is missing, the script **automatically
falls back** to the deterministic fixture so a recording never fails mid-cast.

## What the demo proves

One file carrying three unrelated concerns (a `fix` in `main`, a `feat` in
`greet`, and a `refactor` in `log_access`) is split into **three atomic
Conventional-Commits** at the hunk level — the behavior that distinguishes aic
from file-level committers. The three edits live in separate functions spaced
far enough apart that git emits one hunk per concern, so the live run produces
the same three commits the fixture pins.

## Keeping the fixture fresh

When you change the demo edits (in `scripts/demo.sh`'s `write_after_*`
functions) or the fixture commit messages (in `run_fixture`), update
[`examples/before-after/after.txt`](../../examples/before-after/after.txt) to
match so the fixture and the recorded expectation stay in sync.
