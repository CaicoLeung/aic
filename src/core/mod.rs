//! Config, CLI entry, cross-cutting type aliases, self-update, shell completion.
//!
//! These modules are consumed by every layer; they hold no workflow logic.
//!
//! Naming note: this module shadows the built-in `core` crate on unqualified
//! `use core::…` paths — when a child module shares a name with a `core`
//! item (`fmt`, `ptr`, …), the local one silently wins that path. Qualify
//! std-`core` imports as `::core::…` if one is ever needed. Today's
//! children (`cli`, `completion`, `config`, `types`, `update`) collide
//! with nothing.

pub mod cli;
pub mod completion;
pub mod config;
pub mod types;
pub mod update;
