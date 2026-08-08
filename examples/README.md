# aic examples

This directory holds a **reproducible demo** of aic's headline behavior: taking
one file that mixes three unrelated concerns and splitting it into **three
atomic Conventional-Commits** — at the hunk level, not the file level.

Everything here is consumed by [`scripts/demo.sh`](../scripts/demo.sh); you do
not run anything in this directory by hand.

## Layout

```
examples/
├── sample-repo/
│   ├── src/calculator.rs      # baseline file (the "before" state)
│   └── patches/
│       ├── 01-fix.patch       # edit 1 — fix:  subtract the right operand
│       ├── 02-feat.patch      # edit 2 — feat: add a multiply function
│       └── 03-style.patch     # edit 3 — style: label the output lines
├── fixtures/
│   └── fixtures.json          # recorded plan + commit messages (offline mode)
└── before-after/
    ├── before.txt             # git log before running aic
    └── after.txt              # git log after  — three atomic commits
```

## The three edits

`src/calculator.rs` is a tiny calculator. `scripts/demo.sh` commits it as a
baseline, then applies three deliberately-unrelated edits **unstaged**, so the
working tree looks exactly like the messy state a developer hands to `aic`:

| # | Type    | Hunk | What changes                                   |
|---|---------|------|------------------------------------------------|
| 1 | `fix`   | 1    | `subtract` was computing `a - a`; now `a - b`. |
| 2 | `feat`  | 2    | Adds a `multiply(a, b)` function.              |
| 3 | `style` | 3    | Labels the `main` printout lines.              |

The methods are spaced apart (see the banner comment in `calculator.rs`) so git
produces **three separate hunks** — one per concern. aic's batch planner assigns
each hunk to its own batch, stages them one at a time (`git add -p` style), and
writes a focused message for each.

## Intended outcome

Running `scripts/demo.sh` produces exactly **three** atomic commits:

```
<sha> style: label calculator output lines
<sha> feat: add multiply function
<sha> fix: subtract the right operand in calculator
<sha> chore: seed calculator   ← baseline, not counted
```

(See [`before-after/`](before-after/) for the captured `git log --oneline`.)

## Offline (fixture) vs. live mode

`scripts/demo.sh` defaults to **offline fixture mode**: `aic` reads recorded
responses from `fixtures/fixtures.json` (via the `AIC_FIXTURE_DIR` env var)
instead of calling a model. The demo is then fully deterministic and needs no
network, no API key, and no local model — so the README GIF never rots and CI
can run it.

To watch a real model do the splitting, run `scripts/demo.sh --live` against a
local Ollama server (`ollama pull llama3.3` first). See
[`docs/demo/README.md`](../docs/demo/README.md) for the one-command re-record
recipe.
