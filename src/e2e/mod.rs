//! End-to-end tests for the user-facing workflows: the commit Run
//! (`commit.rs`, `grouping.rs`, `hooks.rs`), the resolve flow (`resolve.rs`,
//! ADR 0005), and the confirmation menu (`confirm.rs`).
//!
//! These drive the workflow entries directly — `default_run`/`commit_run`
//! (seams bundled in `RunDeps`) and `resolve_run` (seams in `ResolveDeps`) —
//! against real on-disk git repositories in tempdirs, with every
//! nondeterministic boundary stubbed via `common`: the LLM calls (resolver,
//! batch planner, commit messenger) and the interactive surfaces (y/n
//! prompt, confirmation menu, message editor) are scripted queues. Git
//! stays real: we set up actual merge / rebase conflicts via the `git` CLI
//! and libgit2, then assert on the resulting repo state (state machine,
//! index blobs, working-tree contents, finalize commit).
//!
//! Why stub the LLM and not the git layer: the LLM call is a thin wrapper over
//! a third-party HTTP client — its correctness is rig's problem, not aic's.
//! The workflows' logic lives in the orchestration around it (status/conflict
//! gating → planning and validation → staging → commit/finalize), and that is
//! exactly what these tests exercise against a real repository.

#![cfg(test)]
// Each e2e test constructs its own `Git` handle at its tempdir (`Git::at`) and
// passes it into the workflow under test. No process CWD is mutated, so tests
// need no chdir guard and no global lock — they run in parallel on independent
// tempdirs, and every real git operation the workflow drives is pinned to the
// repo the handle discovered.
#![allow(clippy::await_holding_lock)]

mod commit;
mod common;
mod confirm;
mod grouping;
mod hooks;
mod resolve;
