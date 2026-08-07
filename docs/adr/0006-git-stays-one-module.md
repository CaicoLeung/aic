# ADR 0006: Git stays one module around the shared repo handle

- **Status:** Accepted
- **Date:** 2026-08-07

## Context

`src/git.rs` was a 1638-line module mixing five concerns: libgit2 read
operations (`status`, `diff`, `diff_workdir`), libgit2 index writes (`add`),
git-CLI shell-out (`run_git`, used by `stage_hunks`, `commit`, `finalize`),
worktree filesystem I/O (`read_worktree`, `write_worktree`), and commit policy
(`assert_commit_safe`, `verify_commit_clean`).

An architecture review surfaced the conflict-resolution surface
(`RepoState`, `ConflictKind`, `ConflictedFile`, `state`, `conflicted_files`,
`read_worktree`, `write_worktree`, `finalize`, `has_conflict_markers`) as a
coherent deep module buried inside it. That surface was extracted into
`src/conflict.rs` (reached as `Git::conflict() -> Conflict<'_>`); see
CONTEXT.md "Conflict module". The commit guards stay on `Git` and call across
that seam.

After the extraction `Git` is still ~1200 lines and still mixes libgit2 reads,
CLI shell-out, worktree I/O, and commit policy. The question this ADR answers
is whether to split `Git` further — e.g. extract the CLI shell-out layer into
its own module, or separate read operations from writes.

## Decision

`Git` stays **one module**. The libgit2 read path, the CLI shell-out path, the
index-mutation path, and the commit guards are not separated into sub-modules.

The binding reason is shared, load-bearing state:

- `run_git` (the CLI shell-out) and `index()` (the libgit2 index, force-refreshed
  from disk) are shared by `status`, `diff`, `diff_workdir`, `add`,
  `stage_hunks`, and `commit`. Every one of these crosses the libgit2/CLI
  duality.
- The `index()` force-refresh exists *because* the CLI rewrites the index file
  behind libgit2's back. That cache-coherence hack is meaningful only when both
  backends live behind one handle that owns the single source of truth for the
  refreshed index. Splitting the CLI layer out would either duplicate the
  refresh strategy in two places (drift risk — exactly the staleness bug the
  hack prevents) or force a seam across shared `git2::Repository` state for no
  behavioural gain.
- `stage_hunks` already bridges the two backends inside one method: a pure
  Rust parser (`diff::parse_file_patch`) slices hunks, then `run_git("apply",
  "--cached")` applies them. The pure parser and the CLI must agree on hunk
  format; separating them adds a seam across that agreement.

The conflict domain was extractable precisely because it is *self-contained*:
its types, its classification logic, and its worktree I/O do not share the
index-refresh strategy or the commit path with the rest of `Git`. The remainder
of `Git` does not have that property — it is coupled by the shared handle.

## Alternatives considered

- **Extract a CLI/exec sub-module** (`run_git` + `nonzero_exit` into
  `git::exec`, shared by `Git` and `Conflict`). Rejected. `Conflict::finalize`
  reaches `run_git` through the `&Git` borrow (`run_git` is `pub(crate)`); one
  implementation, one place the command-line-in-error-once contract lives. A
  shared exec module is a seam for seam's sake — both callers are in-process,
  same backend (the repo), same contract. The codebase-design principle "one
  adapter means a hypothetical seam, two means a real one" applies only when
  something *varies* across the seam; nothing varies here.
- **Split reads from writes** (a read-only `GitReader` vs a `GitWriter`).
  Rejected. `commit` is a write that must read HEAD before and verify the
  landed tree after; `stage_hunks` reads the workdir diff to stage into the
  index; `add` reads the index to decide add-vs-remove. The read/write split
  is not a real boundary in this codebase — nearly every mutation reads first.
- **Split by backend** (a libgit2 module vs a CLI module). Rejected for the
  same cache-coherence reason as the decision above: the libgit2/CLI duality is
  the coupling, not a seam. `index()` exists to reconcile the two; separating
  them makes that reconciliation belong to no one.

## Consequences

- **Positive:** The index-refresh strategy has one owner. The staleness bug it
  prevents (`git apply --cached` rejecting remapped hunks mid-Run) cannot
  regress by one half forgetting to refresh.
- **Positive:** `stage_hunks`'s parser/CLI agreement stays local — one method,
  one place to change hunk handling.
- **Positive:** Future architecture reviews can stop at "the conflict domain
  was extracted (ADR 0006's companion change); the remainder is coupled by the
  shared handle and deliberately unified." This ADR is the reason not to
  re-suggest splitting `Git` further.
- **Negative:** `git.rs` remains a large file. Accepted: the size reflects
  genuine coupling, not accidental accumulation. Depth is a property of the
  interface, not the line count; `Git`'s interface is wide because the concept
  (one repo handle bridging two backends) is genuinely wide.
- **Negative:** `run_git` and `index()` are `pub(crate)` rather than private, so
  `Conflict` can reach them. This is the minimal visibility that admits the
  second in-process caller without inventing a trait or sub-module seam.
