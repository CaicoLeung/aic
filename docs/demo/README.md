# docs/demo/

Assets and instructions for the aic demo GIF / asciinema cast (the
"hunk-level split" shown at the top of the README).

## One-command demo

```sh
scripts/demo.sh
```

Needs **no API key and no network**: it replays a recorded fixture of `aic`'s
hunk-level split over [`examples/sample-repo/`](../../examples/sample-repo/) —
one file edited in three unrelated ways becomes three atomic Conventional
Commits. See [`examples/before-after/`](../../examples/before-after/) for the
result captured as text.

## Re-recording the GIF

The cast is regenerated from `scripts/demo.sh`, so it never goes stale when the
UX or messages change.

### 1. Install asciinema + agg (once)

```sh
# asciinema records the terminal session
brew install asciinema        # macOS; or: pipx install asciinema
# agg renders a .cast to an embeddable, autoplaying GIF
cargo install --git https://github.com/nickolasburr/agg
```

### 2. Record the cast

Use the **recorded-fixture** mode so the cast is deterministic (no network):

```sh
asciinema rec docs/demo/aic-hunk-split.cast \
  --command "scripts/demo.sh" \
  --idle-time-limit 1.5 --cols 80 --rows 24
```

> Prefer to show the **live** tool in the GIF? Record with the real binary
> instead (slower, messages may vary by model):
> ```sh
> AIC_DEMO_LIVE=1 asciinema rec docs/demo/aic-hunk-split.cast \
>   --command "scripts/demo.sh" --idle-time-limit 2 --cols 80 --rows 24
> ```

### 3. Render the GIF

```sh
agg --theme monokai --font-size 16 --speed 1 \
  docs/demo/aic-hunk-split.cast docs/demo/aic-hunk-split.gif
```

Keep the GIF **≤ ~8 s, ≤ 5 MB, no audio** so it autoplays in the GitHub repo
header. Tune `--speed` and `--idle-time-limit` to hit that target.

### 4. Commit both

```sh
git add docs/demo/aic-hunk-split.cast docs/demo/aic-hunk-split.gif
git commit -m "docs(demo): refresh hunk-split cast + gif"
```

The README embeds `aic-hunk-split.gif`; the `.cast` is committed so anyone can
re-render or host the interactive version.

## Files

| file | purpose |
| --- | --- |
| `aic-hunk-split.cast` | asciinema recording (committed; re-renderable) |
| `aic-hunk-split.gif`  | autoplaying GIF embedded in the README header |

(Both are produced by the steps above; the GIF/cast themselves land with the
A2-1 README package — this directory owns the *recipe*, `scripts/demo.sh` owns
the *run*.)
