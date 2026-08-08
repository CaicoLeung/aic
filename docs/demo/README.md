# Demo assets

This directory holds the show-don't-tell proof for the README headline: **one
file edited in three unrelated ways becomes three clean atomic commits**.

| File | What it is |
| --- | --- |
| [`aic-hunk-split.gif`](./aic-hunk-split.gif) | The GIF embedded at the top of `README.md` / `README.zh-CN.md`. Autoplay-loops in the GitHub repo header (~6 s, <300 KB). |
| [`aic-hunk-split.cast`](./aic-hunk-split.cast) | The asciinema cast the GIF was rendered from — a faithful capture of a **real** `aic` run, committed so the GIF can be re-rendered or re-recorded deterministically. |
| [`demo-run.sh`](./demo-run.sh) | The self-contained demo: builds an isolated throwaway repo, edits one file in three unrelated ways, runs `aic`, and prints the resulting atomic commits. Also the script the cast records. |
| [`record.sh`](./record.sh) | Re-record the cast + re-render the GIF in one command. |

## Re-run the demo live

```sh
./docs/demo/demo-run.sh
```

The run is fully isolated — a throwaway git repo and a throwaway `aic` config
under a scratch `HOME`, so it never touches your real repositories or your real
`aic` config.

Provider resolution (first match wins):

1. `AIC_DEMO_BACKEND` + `AIC_DEMO_API_KEY` (+ optional `AIC_DEMO_MODEL`) env
   vars — used by `record.sh` and CI to pin a provider.
2. Your existing `aic` config (copied into the scratch `HOME`, with the
   pre-commit confirmation disabled so the run stays non-interactive).
3. A running local **Ollama** (`http://localhost:11434`) — **no API key
   required**.
4. Otherwise the script prints setup instructions and exits non-zero.

So the no-API-key path is a local Ollama model: `ollama pull llama3.3` then
`./docs/demo/demo-run.sh`.

## Re-record the GIF

```sh
pip install asciinema          # or: uv tool install asciinema
brew install agg               # asciinema → GIF renderer

AIC_DEMO_BACKEND=deepseek AIC_DEMO_API_KEY=sk-… ./docs/demo/record.sh
```

`record.sh` runs `demo-run.sh` under asciinema, normalises the cast dimensions,
and renders `aic-hunk-split.gif` with `agg`. Tune the loop tightness with
`AIC_DEMO_SPEED` (default `2.5`) and the terminal size with
`AIC_DEMO_COLS` / `AIC_DEMO_ROWS`.

## What the cast shows

A real `aic` run against `src/auth.rs` with three maximally-unrelated, widely
spaced edits — git therefore emits **three hunks**, and `aic` partitions them
into **three atomic commits** of three different Conventional-Commits types:

```
988b50a feat(auth): add OAuth2 login support
131ef4f fix(auth): reject tokens at exact expiry boundary
79608d0 docs(auth): expand module-level documentation
39b9aaa initial: auth helpers
```

This is the exact scenario the README headline describes, captured from a live
run — not animated by hand.
