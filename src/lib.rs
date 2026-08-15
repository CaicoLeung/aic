//! Library root: the module tree. The binary is a thin dispatch shell over
//! this crate; the workflows live in their modules — the default commit Run
//! in [`run`], the resolve workflow in [`resolve`].

// Config, CLI entry, cross-cutting aliases, self-update.
pub mod cli;
pub mod completion;
pub mod config;
pub mod types;
pub mod update;

// The git surface: repo handle (split into git/), conflict resolution,
// diff parsing, the diff-JSON envelope, batch staging.
pub mod conflict;
pub mod diff;
pub mod diff_json;
pub mod git;
pub mod staging;

// LLM backends: API providers, CLI agents, stream decoding, shared
// parsing, retries, prompt assembly, message generation.
pub mod cli_agent;
pub mod decoder;
pub mod generator;
pub mod llm;
pub mod parse;
pub mod prompt;
pub mod retry;

// Terminal rendering: the Display surface, progress/spinners, markdown
// rows, palette, layout, cursor, commit-type vocabulary, reasoning feed.
pub mod commit_type;
pub mod cursor;
pub mod display;
pub mod layout;
pub mod markdown;
pub mod palette;
pub mod progress;
pub mod reasoning_feed;

// Workflows: the commit Run, resolve, setup wizard (split into setup/),
// confirmation menus, deterministic grouping, input primitives.
pub mod confirm;
pub mod grouping;
pub mod input;
pub mod resolve;
pub mod run;
pub mod setup;

#[cfg(test)]
mod e2e;
