use clap::{Parser, Subcommand};

use crate::llm::Provider;

/// The `aic use <provider>` vocabulary — every registry canonical name with
/// its aliases right behind it — as clap possible values, so shell completion
/// offers the same words `Provider::from_name` accepts. Built from the
/// registry (single source of truth); aliases are flat entries because
/// possible-value matching is how clap validates the input.
fn provider_values() -> clap::builder::PossibleValuesParser {
    let values: Vec<clap::builder::PossibleValue> = Provider::all()
        .iter()
        .flat_map(|p| std::iter::once(p.name()).chain(p.aliases().iter().copied()))
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
    /// Switch the active API provider to one already configured via `aic setup`
    Use {
        /// Provider name or alias (e.g. openai, anthropic, gemini, deepseek)
        #[arg(value_parser = provider_values(), ignore_case = true)]
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
