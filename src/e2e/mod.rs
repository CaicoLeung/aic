//! End-to-end tests for the `aic resolve` feature (ADR 0005).
//!
//! These drive the full workflow functions (`run_resolve_workflow_impl`,
//! `run_commit_workflow_impl`) against real on-disk git repositories in
//! tempdirs, with the LLM resolver and the y/n prompt replaced by stubs. Git
//! stays real: we set up actual merge / rebase conflicts via the `git` CLI and
//! libgit2, then assert on the resulting repo state (state machine, index
//! blobs, working-tree contents, finalize commit).
//!
//! Why stub the LLM and not the git layer: the LLM call is a thin wrapper over
//! a third-party HTTP client — its correctness is rig's problem, not aic's.
//! The feature's logic lives in the orchestration around it (state detection →
//! classification → per-file resolution → marker validation → sticky staging →
//! finalize-gating), and that is exactly what these tests exercise against a
//! real repository.

#![cfg(test)]
// Each e2e test holds `GIT_CWD_MUTEX` across its workflow `.await` by design:
// the whole point is to pin the process CWD to the tempdir for the duration of
// the real git operations the workflow drives. Tests serialize on this single
// mutex and run on single-threaded runtimes, so the guard can't deadlock.
#![allow(clippy::await_holding_lock)]

mod commit;
mod common;
mod fmt;
mod hooks;
mod resolve;
