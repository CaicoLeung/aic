//! Workflows: the commit Run, resolve, the setup wizard, confirmation menus,
//! deterministic grouping, input primitives. These modules compose the
//! lower layers (git, llm, render) into user-facing flows.

pub mod confirm;
pub mod grouping;
pub mod input;
pub mod resolve;
pub mod run;
pub mod setup;
