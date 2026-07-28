# ADR 0005: Per-file conflict resolution with sticky approval and finalize-gating

- **Status:** Accepted
- **Date:** 2026-07-27

## Context

aic's contract was "read a diff, draft a conventional-commit message, commit."
It had no awareness of merge conflicts. Two gaps were reported:

1. Users hitting conflicts during `git merge` / `cherry-pick` / `revert` had to
   resolve marker-laden files by hand, then come back to aic for the message.
2. Nothing stopped aic from committing a file that still contained
   `<<<<<<<` / `=======` / `>>>>>>>` markers — the LLM would happily describe
   the marker text and the commit would land broken code.

We needed (a) AI-assisted conflict resolution with human review before commit,
and (b) a guard that aborts any commit while markers remain or the repo is
mid-operation.

## Decision

Resolution runs **per-file**: one LLM call per conflicted file, each returning
the full marker-free file content. Resolutions are reviewed via a combined
unified diff, but approval is **per-file**. An approved file is written to the
working tree and `git add`-ed **immediately**, and that staging is **sticky** —
later verdicts (reject / skip) never void it.

**Finalize is gated on completeness.** `git cherry-pick --continue` (and
siblings) refuse while any path is still unmerged, so aic only finalizes when
*every* conflicted file in the run was approved. If any file was rejected or
skipped, aic leaves the approved files staged and hands finalization back to the
user (`resolve these manually, then git cherry-pick --continue`). Re-running
`aic resolve` picks up only the remaining conflicted files, since staged files
are no longer conflicted.

**Two safety gates, innermost first:**

1. *Marker validation (automatic).* If an LLM Resolution still contains conflict
   markers, it is auto-retried once; a second failure marks the file unresolved.
   The human never sees a marker-laden diff.
2. *Commit guard (always-on, feature #2).* Every `Git::commit` first asserts
   `Repository::state() == Clean` and scans staged file contents for markers.
   Either condition aborts the run with a redirect to `aic resolve`. Already-
   committed batches stay committed; the rest are left staged.

**Scope.** v1 resolves and finalizes `Merge`, `CherryPick`, `CherryPickSequence`,
`Revert`, `RevertSequence`. `Rebase`/`RebaseInteractive`/`RebaseMerge` and
`ApplyMailbox` are detected (so the guard and the default-run prompt still fire)
but `aic resolve` refuses them — rebase finalization replays commits and can
spawn successive conflict rounds, a state machine of its own. The finalize
message is git's default (`MERGE_MSG` / original cherry-pick message / git's
`Revert "…"`); aic does **not** generate a message for finalize.

**Unresolvable files** (binary, delete/modify, or over 50 KB / 2000 lines) are
skipped per-file and reported; they do not abort the run — the rest still
resolve and stage.

## Alternatives considered

- **Per-repo batch — one LLM call for all conflicted files, atomic review.**
  Rejected. Token blowout on repos with many conflicted files; one bad file
  forced rejecting the whole batch and re-running from zero. Also made partial
  progress impossible — any single rejection voided all approved resolutions.
- **Per-hunk resolution (one LLM call per conflict region).** Deferred.
  Surgical and token-cheap per call, but starves the LLM of file-level context
  (a conflict inside a function often depends on types/imports elsewhere in the
  file), needs N round-trips, and requires splicing regions back into the file.
  Per-file gives the LLM the whole file and returns a whole file; the review
  diff catches any silent edits to non-conflicted lines.
- **Per-file review with atomic apply (reject voids all approvals).** Rejected.
  Git's `--continue` blocks on any unmerged path regardless, so voiding
  approved work on one rejection just wastes the user's reviewed effort for no
  git-level benefit. Sticky staging preserves real progress and is git-native
  (staged resolutions persist across runs).
- **aic generates the finalize commit message.** Rejected. Cherry-pick keeping
  the original commit's message is expected git behavior; rewriting it loses
  provenance and surprises reviewers. aic's message-generator stays scoped to
  normal authored commits.

## Consequences

- **Positive:** Partial progress is safe and durable. A user can resolve 9 of 10
  files, walk away, and resume — the 9 stay staged; re-running picks up the 10th.
- **Positive:** Half-state is benign. While `state() != Clean`, the commit guard
  blocks any unrelated `aic` commit, so nothing escapes mid-merge. Consistent
  with feature #2 rather than in tension with it.
- **Positive:** The commit guard closes the "LLM commits marker-laden code" hole
  universally — it lives in `Git::commit`, so every future commit path inherits
  it.
- **Negative:** Finalize cannot complete while any file is rejected/skipped.
  This is forced by git, not aic; the hand-off message makes it explicit.
- **Negative:** `aic resolve` shells out to `git` for finalize
  (`commit --no-edit`, `cherry-pick --continue`, `revert --continue`) rather
  than using libgit2. libgit2's merge-commit parent handling and `--continue`
  semantics are fiddly; the native CLI does the right thing for all three
  states. This mirrors the existing pattern of shelling out for `cargo fmt` and
  `brew upgrade`.
- **Negative:** Binary/delete-modify/oversized conflicts still require manual
  resolution. Acceptable for v1; per-hunk fallback is a future option for the
  oversized case specifically.
