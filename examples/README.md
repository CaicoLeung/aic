# examples/

Reproducible artifacts for the **hunk-level split** demo — the "one file, three
unrelated edits → three atomic commits" headline from the README.

## Layout

```
examples/
├── sample-repo/        tiny throwaway Rust "project" used as the demo input
│   ├── src/auth.rs     the file aic splits (baseline; three regions get edited)
│   └── edits/          one unified-diff patch per logical change (1 hunk each)
│       ├── 01-style.patch
│       ├── 02-fix.patch
│       └── 03-feat.patch
└── before-after/       the demo result captured as TEXT (not just video)
    ├── before.txt      what a file-level tool produces (1 mixed commit)
    └── after.txt       what aic produces (3 atomic Conventional Commits)
```

## The scenario

`sample-repo/src/auth.rs` is a small auth module. The three patches in `edits/`
each touch a **different, unrelated region** of that one file, far enough apart
that git emits a separate hunk per change:

| patch | concern | region |
| --- | --- | --- |
| `01-style.patch` | `style` | the `Auth::new` constructor |
| `02-fix.patch` | `fix` | the token-expiry comparison in `is_valid` |
| `03-feat.patch` | `feat` | a new `oauth2_login` method |

Applied together they are **one file changed in three unrelated ways** — exactly
the case where file-level commit tools produce one muddy commit and `aic`
produces three clean ones.

## Reproduce it

```sh
scripts/demo.sh                   # no API key, no network — recorded fixture
AIC_DEMO_LIVE=1 scripts/demo.sh   # run the real aic binary (needs `aic setup`)
```

See [`scripts/demo.sh`](../scripts/demo.sh) and
[`docs/demo/README.md`](../docs/demo/README.md) for details and for how to
re-record the GIF.

> `examples/` is excluded from the published crate (see `exclude` in
> `Cargo.toml`); it ships only in the git repo for contributors/reviewers.

## Why patches (not a nested git repo)

`sample-repo/` ships as **plain source files**, not a git repository. A nested
`.git` would either be ignored or turn into an unwanted submodule, so
`scripts/demo.sh` initializes the throwaway git repo itself at run time (under
`examples/.demo-work/`, gitignored). That keeps this directory lean and the run
fully reproducible from a fresh clone.
