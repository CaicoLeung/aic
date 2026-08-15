use clap::{Parser, Subcommand};

use crate::llm::{Provider, cli_agent::PRESETS};

/// The `aic use <name>` vocabulary: CLI-agent presets first (they win at
/// match time — `aic use claude` is the claude code CLI agent, not the
/// Anthropic API provider), then every registry provider name and alias that
/// doesn't collide with a preset. Flat clap possible values so shell
/// completion offers exactly what `aic use` accepts.
fn use_values() -> clap::builder::PossibleValuesParser {
    let mut words: Vec<&str> = PRESETS.to_vec();
    words.extend(
        Provider::all()
            .iter()
            .flat_map(|p| std::iter::once(p.name()).chain(p.aliases().iter().copied())),
    );
    // Order-preserving dedupe: a provider alias shadowed by a preset (claude)
    // disappears from the vocabulary instead of appearing twice.
    let mut seen = std::collections::HashSet::new();
    let values: Vec<clap::builder::PossibleValue> = words
        .into_iter()
        .filter(|w| seen.insert(*w))
        .map(clap::builder::PossibleValue::new)
        .collect();
    clap::builder::PossibleValuesParser::new(values)
}

#[derive(Parser)]
#[command(
    name = "aic",
    version,
    about = "An AI-powered Rust CLI for generating git commit messages in bulk.\naic[https://github.com/CaicoLeung/aic]"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Interactively configure LLM provider, API key, and model
    Setup,
    /// Show current resolved configuration
    List,
    /// Update aic to the latest version
    Update,
    /// Resolve git merge conflicts in the working tree via the LLM
    Resolve,
    /// Switch the active backend: an API provider already configured via
    /// `aic setup`, or a CLI agent (claude, codex, pi, opencode)
    Use {
        /// API provider name/alias (e.g. openai, anthropic, gemini), or a
        /// CLI agent (claude, codex, pi, opencode)
        #[arg(value_parser = use_values(), ignore_case = true)]
        provider: String,
    },
    /// Install shell completion script
    ///
    /// Interactively pick a shell and install its completion (the highlight
    /// defaults to your `$SHELL`):
    ///
    ///   aic completion
    Completion,
}
