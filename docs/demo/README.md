# Re-recording the demo

`scripts/demo.sh` is the single source of truth for the headline demo (aic
splits one file's mixed edits into **block-level atomic commits**). Re-recording
the GIF / asciinema cast for the README is one command.

## One-command re-record

```bash
# Default (no network): deterministic fixture — always works, CI-safe.
scripts/demo.sh

# Live: run the real aic against a local Ollama model, then capture a cast.
AIC_DEMO_LIVE=1 asciinema rec demo.cast --command scripts/demo.sh
```

## What you need for a *live* recording

1. A local Ollama instance running and a model pulled, e.g.:

   ```bash
   ollama serve &
   ollama pull qwen2.5-coder:7b
   ```

2. `aic` available — either on your `PATH` (`cargo install --path .`) or built
   locally (`scripts/demo.sh` auto-detects `target/release/aic` then
   `target/debug/aic`). Override with `AIC_BIN=/path/to/aic` if needed.

3. Opt in to the live path:

   ```bash
   AIC_DEMO_LIVE=1 scripts/demo.sh
   ```

   `scripts/demo.sh` probes `$AIC_OLLAMA_URL` (default `http://localhost:11434`);
   if Ollama is unreachable or `aic` is missing, it **automatically falls back**
   to the deterministic fixture so the demo never fails mid-record.

## Fixture mode (default, no network)

With `AIC_DEMO_LIVE` unset, `scripts/demo.sh` replays the recorded commit
history pinned in [`examples/before-after/after.txt`](../../examples/before-after/after.txt).
The printed `git log --oneline` is identical every run: one base commit plus
three atomic Conventional Commits (`fix`, `feat`, `style`). This is what runs in
CI and what powers the no-network GIF.

## Keeping the fixture fresh

When you change the demo edits (in `scripts/demo.sh`'s `write_after_*` functions),
update [`examples/before-after/after.txt`](../../examples/before-after/after.txt)
to match so the fixture and the recorded expectation stay in sync.
