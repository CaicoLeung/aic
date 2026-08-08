# sample-repo

The input for the aic hunk-level split demo. Not a real project — just one Rust
file with three deliberately-unrelated regions.

## Intended outcome

Running `aic` over this repo (after the three edits are applied) yields **three
atomic Conventional Commits**, one per logical change — instead of the single
mixed-concern commit a file-level tool would produce:

```
feat(auth): add OAuth2 login support
fix(auth): allow tokens expiring at current second
style(auth): format Auth constructor
chore: baseline auth module
```

(Messages above are the recorded output of a real `aic` run; live runs may vary
slightly by model. The capture is in
[`../before-after/after.txt`](../before-after/after.txt).)

## Files

- `src/auth.rs` — the **baseline** module (its committed HEAD state).
- `edits/01-style.patch`, `edits/02-fix.patch`, `edits/03-feat.patch` — one
  single-hunk unified diff per logical change, each based on the baseline.

`scripts/demo.sh` seeds a throwaway git repo from this directory, commits the
baseline, applies all three patches to the working tree, and then splits the
three hunks into three commits — staging each hunk via `git apply --cached`,
exactly the way `aic` stages.
