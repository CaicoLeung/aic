# sample-repo

Throwaway git repo used **only** by [`scripts/demo.sh`](../../scripts/demo.sh) to
demonstrate aic's headline feature: splitting a single file's mixed, unrelated
edits into **block-level atomic commits**.

`scripts/demo.sh` copies this directory into a fresh temp location, `git init`s
it, commits the base state, then applies three deliberately-unrelated edits to
`src/main.rs` at once — a `fix`, a `feat`, and a `style` change. Running `aic`
against that working tree (with nothing staged) yields three separate Conventional
Commits instead of one muddled commit.

This directory is the **seed** (base state). It is not itself a git repository —
no nested `.git` — so it stays clean inside the aic repo. The demo materializes
the throwaway repo on the fly.

See [`examples/before-after/`](../before-after/) for the recorded result and
[`docs/demo/README.md`](../../docs/demo/README.md) for the one-command re-record
recipe.
