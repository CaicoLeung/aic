//! Config, CLI entry, cross-cutting type aliases, self-update, shell completion.
//!
//! These modules are consumed by every layer; they hold no workflow logic.

pub mod cli;
pub mod completion;
pub mod config;
pub mod types;
pub mod update;
