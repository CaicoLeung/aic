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
(a `fix`, a `feat`, and a `style` change) **at once, with nothing staged**, then runs `aic`.
The expected result is three separate atomic commits:

```
<sha> style: add module doc comment and trailing newline
<sha> feat: print the name in uppercase when --upper is passed
<sha> fix: guard against a missing argument to avoid a panic
<sha> chore: initial commit
```

The exact hashes vary; the **count (≥ 3 atomic Conventional Commits) and the
types/scopes do not**. That is what `examples/before-after/after.txt` pins.

## Live vs fixture

`scripts/demo.sh` is **no-network-by-default**. It runs the real `aic` only when
you opt in with `AIC_DEMO_LIVE=1` **and** a local provider (Ollama) is reachable;
otherwise it replays the recorded fixture from `before-after/` so the demo is
deterministic and CI-safe. See [`docs/demo/README.md`](../docs/demo/README.md).

> `examples/` is excluded from the published crate (see `exclude` in
> `Cargo.toml`). It ships only in the git repo for contributors/reviewers.
