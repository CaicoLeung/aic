use clap::{Parser, Subcommand};

use crate::llm::{Provider, cli_agent::PRESETS};

/// The `aic use <name>` vocabulary: CLI-agent presets first (they win at
/// match time — `aic use claude` is the claude code CLI agent, not the
/// Anthropic API provider), then every registry provider name and alias that
/// doesn't collide with a preset. The single source of truth for both the
/// clap possible values ([`use_values`]) and the completion test that pins
/// them — the shell can never offer a word `aic use` rejects, or hide one it
/// accepts.
pub(crate) fn use_vocabulary() -> Vec<&'static str> {
    let mut words: Vec<&str> = PRESETS.to_vec();
    words.extend(
        Provider::all()
            .iter()
            .flat_map(|p| std::iter::once(p.name()).chain(p.aliases().iter().copied())),
    );
    // Order-preserving dedupe: a provider alias shadowed by a preset (claude)
    // disappears from the vocabulary instead of appearing twice.
    let mut seen = std::collections::HashSet::new();
    words.into_iter().filter(|w| seen.insert(*w)).collect()
}

/// Flat clap possible values so shell completion offers exactly what
/// `aic use` accepts — built from [`use_vocabulary`].
fn use_values() -> clap::builder::PossibleValuesParser {
    clap::builder::PossibleValuesParser::new(
        use_vocabulary()
            .into_iter()
            .map(clap::builder::PossibleValue::new)
            .collect::<Vec<_>>(),
    )
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
    /// `aic setup`, or a CLI agent (claude, codex, pi, opencode, omp, gemini,
    /// cursor, windsurf, copilot, trae, qwen)
    Use {
        /// API provider name/alias (e.g. openai, anthropic, google), or a
        /// CLI agent (claude, codex, pi, opencode, omp, gemini, cursor,
        /// windsurf, copilot, trae, qwen)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The `use` vocabulary contract: presets first (they win at match
    /// time), every registry canonical name and alias present, and a
    /// preset-shadowed alias (claude) appearing exactly once — so completion
    /// and clap acceptance can never drift apart (both derive from this).
    #[test]
    fn use_vocabulary_lists_presets_first_and_dedupes_shadowed_aliases() {
        let words = use_vocabulary();
        for (i, preset) in PRESETS.iter().enumerate() {
            assert_eq!(&words[i], preset, "presets must lead the vocabulary");
        }
        for p in Provider::all() {
            assert!(words.contains(&p.name()), "{} missing", p.name());
            for alias in p.aliases() {
                assert!(words.contains(alias), "{alias} missing");
            }
        }
        // The shadowed Anthropic alias: exactly one claude — the CLI agent.
        assert_eq!(
            words.iter().filter(|&&w| w == "claude").count(),
            1,
            "got {words:?}"
        );
        let unique: std::collections::HashSet<&&str> = words.iter().collect();
        assert_eq!(unique.len(), words.len(), "no duplicates: {words:?}");
    }
}
