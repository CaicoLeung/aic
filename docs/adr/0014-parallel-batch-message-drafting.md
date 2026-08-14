# ADR 0014: Parallel batch-message pre-drafting

- **Status:** Accepted
- **Date:** 2026-08-14

## Context

A Run with several Batches commits them **serially**: one LLM round-trip per
Batch for the Drafted Message, then `git commit`, before the next Batch even
begins. The loop is in `run_commit_workflow_impl` (`src/main.rs:410`):

```
for (i, batch) in result.batches.iter().enumerate() {
    staging.stage_batch(git, batch, &display)?;          // git: re-read diff, remap hunks, apply
    generate_and_commit(git, &paths, ...).await?;         // messenger(diff_str) [LLM] + git.commit
}
```

Per Batch, `generate_and_commit` (`src/main.rs:64`) does `git.diff(paths)` to
rebuild a **staged** diff, calls the `CommitMessenger` LLM seam
(`src/main.rs:87`), then `git.commit` (`src/main.rs:101`). The BatchPlan
analysis itself streams reasoning (one call); each Drafted Message runs behind a
bare spinner (no streaming — `generator.rs:134-140`).

The wall-clock cost is therefore **N sequential LLM round-trips** (1 plan + N
messages). The git operations — staging, diff reads, the commit — are local and
millisecond-scale. A Run's latency is LLM-bound, and that latency scales linearly
with Batch count.

The question this ADR answers: what part of the per-Batch loop can be
parallelized, and how, without breaking the staging/commit correctness the Run
already relies on?

## Decision

**Decouple message drafting from the serial commit loop.** After the BatchPlan
is validated, draft *every* Batch's message **concurrently** up front, then run
the existing serial stage→commit loop using the pre-drafted messages.

```
plan (streams reasoning)
  → validate_batch_plan
  → [fan out N concurrent messenger calls, one per Batch]
  → serial loop: stage_batch → commit with pre-drafted message
```

Wall-clock drops from `plan + Σ(draftᵢ + stageᵢ + commitᵢ)` to
`plan + max(draftᵢ) + Σ(stageᵢ + commitᵢ)`. Since a draft is seconds and
stage+commit is milliseconds, N sequential LLM waits collapse to one. Token cost
is unchanged (same N+1 calls); only wall-clock improves, which is exactly the
axis a latency-bound CLI UX is sensitive to.

### Why only drafting can move — staging and committing cannot

- **Git's index is a single shared staging area.** A commit captures the whole
  index; there is no way to stage Batch A's hunks and Batch B's hunks
  simultaneously and commit them independently. The model forces
  stage A → commit A → stage B → commit B.
- **`Staging` is order-dependent by construction.** `stage_batch`
  (`src/staging.rs:70`) re-reads the **current** index→workdir diff and remaps
  each Batch's plan-time hunk indices onto it via `committed_hunks` and
  `map_planned_hunks` (`src/staging.rs:100-134`). The remap is only correct once
  earlier Batches are *already committed* and the diff has shrunk. This is the
  fresh-diff strategy that keeps a Run alive when a pre-commit hook
  (lint-staged/prettier) re-stages whole files (CONTEXT.md "Batch staging").

So the commit loop stays serial and unchanged in shape; the only change is *when*
the messages are produced.

### Why pre-drafting is correct despite the plan-time / staged-diff difference

Today `generate_and_commit` builds the message diff from `git.diff(paths)` — the
**staged** diff, which only exists after `stage_batch`. Pre-drafting cannot read
that, so it slices the **plan-time** workdir diff by Batch instead.

This is sound:

- The plan-time workdir diff is already captured at `src/main.rs:384-401` (each
  file's `git.diff_workdir`, scoped via `diff::format_diff_scoped`, hunk-counted
  via `diff::parse_file_patch`). Today it is built only to feed the `planner` and
  `file_hunk_counts`, then discarded. Retaining it gives every Batch's diff
  content for free.
- Hunk numbering is a single source of truth: `diff::parse_file_patch` is what
  produces the numbered view the model planned against (`src/main.rs:388`) **and**
  what `Staging` remaps from (`src/staging.rs:109`). Slicing each Batch's hunks
  out of the retained plan-time diff therefore yields exactly the diff the plan
  refers to.
- A Drafted Message needs the *semantic* change, which the plan-time hunks
  already capture. A pre-commit hook may reformat bytes (prettier), but that does
  not change the commit *message*. The Re-generate action in the Commit
  confirmation menu (`confirm_draft`) still reads the staged diff, so it remains
  the "redraft against the actually-landed content" escape hatch.

### Concurrency and limits

- `Generator::generate_commit_message` builds a fresh `LlmConfig`/client per call
  (`generator.rs:143`); there is no shared mutable client state, so concurrent
  calls are natural.
- **Bounded concurrency is required.** Fanning out N simultaneous requests can
  trip provider rate limits (HTTP 429). Drafting must go through a semaphore
  (sized to the provider's practical concurrency), or it can be slower than
  serial under backoff.

## Alternatives considered

- **Parallel commits via git worktrees** (one worktree per Batch, commit on a
  branch, then rebase/merge). Rejected. It shatters the single-index model the
  whole Run is built on, breaks pre-commit hooks (which mutate one index),
  breaks `Staging`'s `committed_hunks` tracking, and turns a linear commit list
  into a merge-resolution problem. Vast complexity for no correctness gain.
- **Overlap Batch N+1's staging with Batch N's commit.** Rejected. The git
  operations are already millisecond-scale; overlapping them saves nothing
  measurable while adding index-state interleaving bugs. The latency is in the
  LLM calls, not here.
- **Keep it serial.** Rejected. The Run's UX is latency-bound and the cost is
  purely wall-clock (no extra tokens, no correctness risk); leaving it serial is
  leaving the dominant latency on the table for no benefit.

## Consequences

- **Positive:** A multi-Batch Run's wall-clock becomes dominated by the *single
  slowest* draft rather than the sum of all drafts. Ten Batches on a ~3 s
  round-trip go from ~30 s of message drafting to ~3 s.
- **Positive:** The commit loop, `Staging`, the fresh-diff strategy, the
  Commit-confirmation menu, and Re-generate are all unchanged. The change is
  localized to *when* messages are produced and *which* diff they read.
- **Negative:** A pre-drafted message is computed from the plan-time diff, not
  the staged diff. The two diverge only under a pre-commit hook that rewrites
  content, and a commit message is insensitive to that — but it is a real
  divergence the first reviewer must accept. Re-generate covers the rare
  disagree-with-pre-draft case.
- **Negative:** If `stage_batch` finds a Batch's changes already landed (a
  pre-commit hook staged the whole file; `src/staging.rs:101-108`, skipped at
  `src/main.rs:418-422`), that Batch's pre-drafted message was wasted — one LLM
  call spent for nothing. Rare; accepted, or guardable by drafting lazily after a
  cheap staged-content probe.
- **Negative:** On abort mid-Run (an error or a declined Commit confirmation),
  the already-fanned-out drafts for later Batches are spent though only some
  commits landed. The existing recoverability contract ("K-1 committed, re-run
  `aic` to continue", `src/main.rs:425-450`) still holds; the extra cost is LLM
  spend on an exceptional path, not correctness.
- **Negative:** Requires bounded concurrency (a semaphore) and a per-Batch
  plan-time-diff slicer built on the existing `parse_file_patch` numbering. New
  machinery, but small and on primitives that already exist.

Implementation is the follow-up companion change; this ADR records the direction.
