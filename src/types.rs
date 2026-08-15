//! Workflow seam types — and only those.
//!
//! The erased-closure vocabulary of the two workflow modules (run.rs,
//! resolve.rs) and their fan-out infrastructure (progress.rs,
//! reasoning_feed.rs). They live here — the shared-type module — rather than
//! in any one consumer so no workflow module owns (or reaches back into the
//! crate root for) another's seam vocabulary, keeping the module graph
//! acyclic (confirm.rs also imports `CommitMessenger` for its re-generate
//! phase).
//!
//! The vocabulary and palette that used to share this file live next to
//! their concepts now: [`crate::commit_type`] (CommitType + ParsedMessage)
//! and [`crate::palette`] (every color decision).

use std::future::Future;
use std::pin::Pin;

use crate::generator::{BatchPlanOutput, CommitOutput};

/// A boxed, `Send` future — the return type of every async seam.
pub(crate) type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Erased resolver: a closure that takes the conflicted file content and
/// returns a future yielding the resolved (marker-free) content. Boxed so the
/// workflow signature stays concrete — no generic `where` clauses — while tests
/// can swap in stubs without touching the LLM.
pub(crate) type Resolver = Box<dyn Fn(String) -> BoxFuture<anyhow::Result<String>>>;
/// Erased y/n prompt: answers a labeled question. Boxed for the same reason.
pub(crate) type Prompt = Box<dyn Fn(&str) -> anyhow::Result<bool>>;

/// Erased batch planner: takes the combined unstaged diff JSON and returns the
/// per-hunk batch plan. Boxed for the same reason as [`Resolver`] — tests swap
/// in a stub plan without touching the LLM.
pub(crate) type BatchPlanner = Box<dyn Fn(String) -> BoxFuture<anyhow::Result<BatchPlanOutput>>>;
/// Erased commit-message writer: takes one batch's staged diff JSON and returns
/// its Conventional-Commits message + body. Boxed for the same reason.
///
/// Invariant: the production implementation is a bare LLM call — no spinner.
/// Each caller owns its spinner: the pre-draft phase uses one shared spinner
/// across all concurrent drafts (N standalone spinners would collide on a
/// single terminal line — only one clears, the rest leave residue), and the
/// serial paths (staged single-commit, confirm re-generate) wrap each call.
pub(crate) type CommitMessenger = Box<dyn Fn(String) -> BoxFuture<anyhow::Result<CommitOutput>>>;
