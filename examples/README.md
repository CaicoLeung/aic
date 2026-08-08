# examples/

Reproducible demo assets for aic. Everything here exists to make the headline
(aic splits one file's mixed edits into **block-level atomic commits**) trivial
to reproduce and re-record.

## Layout

| Path | Purpose |
| --- | --- |
| [`sample-repo/`](sample-repo/) | Seed files for the throwaway git repo the demo runs against. **Not** a nested git repo — `scripts/demo.sh` materializes a fresh one in a temp dir. |
| [`before-after/`](before-after/) | Recorded `git log --oneline` of the demo: the **before** (base state) and **after** (three atomic Conventional Commits) the demo is expected to print. This is the deterministic, no-network fixture. |

## Intended outcome

`scripts/demo.sh` applies three deliberately-unrelated edits to `sample-repo/src/main.rs`
(a `fix` in `main`, a `feat` in `greet`, and a `refactor` in `log_access`) **at
once, with nothing staged**, in three functions spaced far enough apart that git
emits one hunk per concern. Running `aic` then yields three separate atomic
commits:

```
<sha> refactor(main): simplify log_access with inline formatting
<sha> feat(main): add --upper flag to greet
<sha> fix(main): guard against missing name argument
<sha> chore: seed sample-repo base state
```

The exact hashes vary; the **count (≥ 3 atomic Conventional Commits) and the
types/scopes do not**. That is what `examples/before-after/after.txt` pins.

## Live vs fixture

`scripts/demo.sh` is **no-network-by-default**. It runs the real `aic` only when
you opt in with `AIC_DEMO_LIVE=1` **and** a provider is configured (a local
Ollama, or one set via the `AIC_DEMO_PROVIDER`/`AIC_DEMO_KEY` knobs); otherwise
it replays the recorded fixture from `before-after/` so the demo is
deterministic and CI-safe. See [`docs/demo/README.md`](../docs/demo/README.md).

> `examples/` is excluded from the published crate (see `exclude` in
> `Cargo.toml`). It ships only in the git repo for contributors/reviewers.
