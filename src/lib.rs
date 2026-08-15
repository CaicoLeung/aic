//! Library root: the module tree. The binary is a thin dispatch shell over
//! this crate; the workflows live in their modules — the default commit Run
//! in [`run`], the resolve workflow in [`resolve`].

pub mod cli;
pub mod cli_agent;
pub mod commit_type;
pub mod completion;
pub mod config;
pub mod confirm;
pub mod conflict;
pub mod cursor;
pub mod decoder;
pub mod diff;
pub mod diff_json;
pub mod display;
pub mod generator;
pub mod git;
pub mod grouping;
pub mod input;
pub mod layout;
pub mod llm;
pub mod markdown;
pub mod palette;
pub mod parse;
pub mod progress;
pub mod prompt;
pub mod reasoning_feed;
pub mod resolve;
pub mod retry;
pub mod run;
pub mod setup;
pub mod staging;
pub mod types;
pub mod update;

#[cfg(test)]
mod e2e;
