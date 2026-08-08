# before-after/

The aic hunk-level split demo, captured as **text** (not just a GIF) so the
headline never goes stale and the result is greppable / diffable.

- [`before.txt`](before.txt) — what a **file-level** tool produces: one
  mixed-concern commit (`update src/auth.rs`) over the same three edits.
- [`after.txt`](after.txt) — what **aic** produces: three atomic
  Conventional Commits, one per hunk.

Both are recorded snapshots produced by `scripts/demo.sh` against
[`../sample-repo/`](../sample-repo/). Commit hashes are illustrative — they
change every run — but the **structure** (1 commit vs 3) and the **messages**
are stable.

Reproduce either file from a fresh clone:

```sh
scripts/demo.sh          # prints the "after" history (3 atomic commits)
```
