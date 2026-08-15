# ADR 0016: git as a module directory (supersedes ADR 0006)

- **Status:** Accepted
- **Date:** 2026-08-15

## Context

ADR 0006 kept `Git` in one file (`src/git.rs`) with a coupling argument:
`run_git` and `index()` are shared load-bearing state behind one
`git2::Repository` handle, the index force-refresh exists *because* the CLI
rewrites the index behind libgit2's back, and `stage_hunks` bridges the pure
parser and the CLI inside one method. Splitting those apart would duplicate
the refresh strategy or seam the shared handle for no behavioural gain.

That argument is about *module* coupling, and it still holds. But ADR 0006
equated "one module" with "one file". In Rust those are different claims: a
module's definition can span a parent file and its submodule files
(`git.rs` + `git/`), with **zero seam introduced** — child modules see the
parent's private items, `impl Git` blocks can live in several files, and the
module path `crate::git` is unchanged. What actually changed since ADR 0006
is that `git.rs` grew to 1550 lines (~775 of them tests), and the file — not
the module — stopped reading well. ADR 0015 settled the layout policy this
fits: directories grow from splits, never from regrouping.

## Decision

`git.rs` becomes a directory root: `src/git.rs` + `src/git/{status,
diff_view, tests}.rs`. One module, several files — not a new seam.

- **Stays in `git.rs`** (the coupled core ADR 0006 defended): the
  `Git`/`Repository` handle, `repo()`, `conflict()`, `run_git`,
  `nonzero_exit`, `at`, `index()` (the refresh strategy, one owner), `add`,
  `stage_hunks` (parser + CLI in one method), `commit`,
  `verify_commit_clean`, `collect_marked_paths`, `assert_commit_safe`, and
  the data types (`FileStatus`, `StatusKind`, `FileStats`).
- **`git/status.rs`**: the status listing (`Git::status`) and the
  index/wt flag → `StatusKind` mappers. Reads the shared handle; owns no
  write path.
- **`git/diff_view.rs`**: diff computation and formatting — `diff`,
  `diff_workdir`, `staged_stats`, `committed_stats`, `stats_from_diff`, and
  the free functions over `Diff::print` callbacks. Shares nothing private
  beyond reading `self.repo` and `index()`.
- **`git/tests.rs`**: the module's test mod, still `pub(crate)` so
  `e2e/common.rs` reaches its fixtures (`crate::git::tests`) unchanged.

ADR 0006's alternatives (an exec sub-module, a read/write split, a
libgit2/CLI backend split) remain rejected — those introduce real seams.
This change introduces none: no trait, no new visibility beyond
`pub(super)` on one helper, no path changes for any `crate::git::*` user.

## Consequences

- **Positive:** The 1550-line file reads as a ~470-line coupled core plus
  two named leaves; ADR 0006's accepted negative ("`git.rs` remains a large
  file") is addressed without touching the coupling it protected.
- **Positive:** Every coupling invariant ADR 0006 named is preserved by
  construction — the handle, `run_git`, `index()`, and `stage_hunks` never
  left the parent file.
- **Negative:** `Git`'s impl blocks now span three files; finding a method
  means knowing which file owns it (mitigated by `git.rs`'s `//!` pointer).
- **Negative:** `git log --follow` + copy detection replace pure rename
  tracking for the moved regions.
