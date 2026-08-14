//! Composition root: module declarations, one-time config migrations, and
//! CLI dispatch. The workflows live in their modules — the default commit Run
//! in [`run`], the resolve workflow in [`resolve`].

pub mod cli;
pub mod cli_agent;
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

use crate::cli::Commands;
use clap::Parser;
use std::io::IsTerminal;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // One-time location migration (ADR 0012) — move a pre-0012 macOS config
    // from `~/Library/Application Support/aic/` to the fixed `~/.config/aic/`
    // location the docs have always claimed. Must run before preset migration
    // so the file lands at its new path first. Idempotent: copy old → new then
    // delete old; skip silently if the new file already exists; no-op when the
    // paths coincide or the old file is absent. A notice prints only when a
    // file is actually moved; a failure is logged, never blocks the run.
    match config::Config::migrate_location() {
        Ok(notices) => notices.iter().for_each(|n| eprintln!("aic: {n}")),
        Err(e) => eprintln!("aic: config location migration skipped: {e:#}"),
    }

    // Auto-migrate a stale CLI-agent config to the current preset shape before
    // any run that uses it — the fix for configs stranded on an older aic's
    // preset (e.g. claude before `stream-json`). Idempotent and conservative:
    // only configs byte-identical to a known legacy preset snapshot are
    // rewritten; a custom command is never touched. Notices print to stderr so
    // the file rewrite is transparent; a migration failure is logged but
    // never blocks the run (the user can still `aic setup` to refresh).
    match config::Config::migrate_if_stale() {
        Ok(notices) => notices.iter().for_each(|n| eprintln!("aic: {n}")),
        Err(e) => eprintln!("aic: config migration skipped: {e:#}"),
    }

    match cli.command {
        Some(Commands::Setup) => setup::run_setup(),
        Some(Commands::List) => config::run_list(),
        Some(Commands::Use { provider }) => config::run_use(&provider),
        Some(Commands::Update) => update::run_update(),
        Some(Commands::Resolve) => resolve::resolve_workflow().await,
        Some(Commands::Completion) => {
            // Interactive when stdout is a terminal; fall back to $SHELL
            // detection for scripts and pipes.
            let shell = if std::io::stdout().is_terminal() {
                match completion::prompt_shell(completion::detect_shell())? {
                    Some(shell) => shell,
                    None => {
                        eprintln!("Cancelled.");
                        return Ok(());
                    }
                }
            } else {
                completion::detect_shell().ok_or_else(|| {
                    anyhow::anyhow!(
                        "couldn't detect your shell from $SHELL; run `aic completion` in a \
                         terminal to pick one (bash, zsh, fish, nushell)"
                    )
                })?
            };
            completion::install_completion(shell)
        }
        None => run::default_workflow().await,
    }
}
