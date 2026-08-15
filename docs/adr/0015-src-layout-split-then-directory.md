# ADR 0015: src/ layout policy — split-then-directory, test siblings, lib/bin split

- **Status:** Accepted
- **Date:** 2026-08-15

## Context

`src/` grew to 35 top-level files with no directories: 31 domain modules
(extracted on purpose, each mapped in CONTEXT.md), `main.rs`, and `src/e2e/`.
Two files had passed the point where one file reads well — `setup.rs` (2108
lines: wizard core, provider sub-flow, CLI sub-flow, verify probes, finalize,
~700 test lines) and `git.rs` (1550 lines: handle, status, diff formatting,
~775 test lines).

`docs/research-src-layout.md` measured a full bucket reorganization
(regrouping all 31 modules into topic directories) and rejected it: 51% of
the 72 directed `use crate::` edges cross the candidate cluster lines, so
directories would regroup files without reducing coupling, at ~191
path-rewrite lines. That measurement is about *regrouping existing modules*.
It says nothing against *splitting one oversized module into a directory* —
which adds edges only inside what was already one module.

The crate was also bin-only: `main.rs` was both module root (31 `pub mod`
declarations) and composition root, with e2e tests hanging off it via
`#[cfg(test)] mod e2e;`.

## Decision

Four rules, adopted together:

1. **Flat by default; directories only from splitting.** A module becomes a
   directory (`foo.rs` + `foo/`) only when the single file has been split for
   readability — never to group unrelated modules into topic buckets. The
   split threshold is a seam judgment (would the halves read independently?),
   not a line count; in practice both splits to date were files past ~1500
   lines. `docs/research-src-layout.md`'s anti-regrouping evidence stands.

2. **Modern module style.** The parent stays `foo.rs`; children are
   `foo/{child}.rs` files declared from it. No `mod.rs`.

3. **Tests live in a sibling `tests.rs` once the file is large.** Small
   modules keep inline `#[cfg(test)] mod tests`. A module whose total (product
   + tests) reaches ~1000 lines moves its test mod to `foo/tests.rs`
   (`#[cfg(test)] mod tests;` in the parent), so the product file reads as
   product. Applied to config, cli_agent, llm, display, markdown, progress,
   decoder, plus setup and git as part of their splits.

4. **lib/bin split; e2e stays in-crate.** `src/lib.rs` owns the module
   declarations and `#[cfg(test)] mod e2e;`; `src/main.rs` is only the thin
   dispatch (CLI parse → migrations → run). The e2e suite stays inside the
   crate (not `tests/`) because it reads `cfg(test)` `pub(crate)` helpers
   (`crate::git::tests`, `crate::conflict::tests`); moving it out would force
   those helpers public — a wider interface for a test-layout preference.

## Alternatives considered

- **Full topic-directory reorganization.** Rejected on the research doc's
  measurement: 51% cross-cluster edges, ~191 rewrite lines, no coupling
  reduction.
- **Cargo workspace split.** Rejected: one binary, one deployable, one test
  suite; a workspace adds ceremony with nothing to vary across crates.
- **Move e2e to `tests/` (integration tests).** Rejected as in Decision 4 —
  it would widen the public API.

## Consequences

- **Positive:** `setup.rs` and `git.rs` read as cores with named sub-flows;
  their product code is no longer buried under 700+ test lines (nor is any
  other ≥1000-line module's).
- **Positive:** `main.rs` no longer doubles as module root; `lib.rs` is the
  single map of the crate.
- **Negative:** Two more directories to traverse; a module's definition can
  now span parent + children (mitigated by the parent's `//!` pointer).
- **Negative:** History tracking needs care: file splits rely on
  `git log --follow` + copy detection (`blame -C`) rather than pure rename
  detection. Accepted; the split commits are mechanical moves for review.
