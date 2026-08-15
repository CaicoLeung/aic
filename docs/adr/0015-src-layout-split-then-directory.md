# ADR 0015: src/ layout policy — domain groups, module directories, test siblings, lib/bin split

- **Status:** Accepted — amended same day: rule 1 inverted from
  "never regroup" to "domain grouping adopted" (owner's call on navigation
  grounds; see rule 1).
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

1. **Five domain directories group the tree.** `src/` holds `lib.rs`,
   `main.rs`, `e2e/`, and five group roots: `core/` (config, CLI entry,
   cross-cutting aliases, self-update, completion), `git/`, `llm/`,
   `render/`, `workflow/`. Grouping is for **navigation** — the src/ root
   reads as five domains instead of 32 siblings. It is explicitly *not* a
   coupling firewall: 55% of `use crate::` edges cross group lines
   (re-measured after the split), which is expected for topic directories
   (rust-analyzer's `ide/` depends on `syntax/` the same way). The research
   doc's measurement remains true — regrouping does not reduce coupling —
   but the owner judged findability the deciding criterion.


2. **Every module is a directory (`foo/mod.rs`) inside its group.** Product
   code, its children, and its tests live together in the module's directory;
   the directory `foo/` exists for every module — including small unsplit
   ones — so no module is a special case and each group reads as a flat
   list of modules.

3. **Tests live in a sibling `tests.rs` once the file is large.** Small
   modules keep inline `#[cfg(test)] mod tests`. A module whose total (product
   + tests) reaches ~1000 lines moves its test mod to `foo/tests.rs`
   (`#[cfg(test)] mod tests;` in `foo/mod.rs`), so the product file reads as
   product. Applied to config, cli_agent, llm, display, markdown, progress,
   decoder, plus setup and git as part of their splits.

4. **lib/bin split; e2e stays in-crate.** `src/lib.rs` owns the module
   declarations and `#[cfg(test)] mod e2e;`; `src/main.rs` is only the thin
   dispatch (CLI parse → migrations → run). The e2e suite stays inside the
   crate (not `tests/`) because it reads `cfg(test)` `pub(crate)` helpers
   (`crate::git::tests`, `crate::conflict::tests`); moving it out would force
   those helpers public — a wider interface for a test-layout preference.

## Alternatives considered

- **Modern parent-file style (`foo.rs` + `foo/` siblings, no `mod.rs`).**
  Rejected: it leaves the `src/` root holding ~30 flat product files next
  to test-only directories, and a directory whose only child is `tests.rs`
  reads as a half-moved module. One uniform rule (`foo/mod.rs` for every
  module) keeps `src/` a pure table of contents.
- **Full topic-directory reorganization.** Initially rejected on the
  research doc's coupling measurement (51% cross-cluster edges, ~191 rewrite
  lines, no coupling reduction); **overruled by the owner on navigation
  grounds the same day** — see amended rule 1. The coupling data stands as a
  true statement about what grouping does and does not buy.
- **Cargo workspace split.** Rejected: one binary, one deployable, one test
  suite; a workspace adds ceremony with nothing to vary across crates.
- **Move e2e to `tests/` (integration tests).** Rejected as in Decision 4 —
  it would widen the public API.

## Consequences

- **Positive:** `setup/` and `git/` read as cores with named sub-flows;
  their product code is no longer buried under 700+ test lines (nor is any
  other ≥1000-line module's).
- **Positive:** `main.rs` no longer doubles as module root; `lib.rs` is the
  single map of the crate, and `src/` is only that map plus `main.rs`.
- **Negative:** Every module adds a directory hop (`src/foo/mod.rs`);
  a split module's definition spans `mod.rs` + children (mitigated by the
  parent's `//!` pointer).
- **Negative:** History tracking needs care: file splits rely on
  `git log --follow` + copy detection (`blame -C`) rather than pure rename
  detection. Accepted; the split commits are mechanical moves for review.
