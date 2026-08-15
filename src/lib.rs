//! Library root: the module tree. The binary is a thin dispatch shell over
//! this crate; the workflows live in their modules — the default commit Run
//! in [`workflow::run`], the resolve workflow in [`workflow::resolve`].
//!
//! The tree is grouped by domain: [`core`] (config/CLI/aliases),
//! [`git`] (repository surface), [`llm`] (backends), [`render`] (terminal
//! output), [`workflow`] (user-facing flows that compose them).

pub mod core;
pub mod git;
pub mod llm;
pub mod render;
pub mod workflow;

#[cfg(test)]
mod e2e;
