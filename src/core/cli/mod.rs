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

/// `--hint` value parser: trim, then reject blank input — an empty
/// directive would append a "User directive" block that says nothing, so
/// clap stops it at the edge with a message instead of the Run silently
/// ignoring it. (A trim-only pass is accepted: surrounding whitespace
/// carries no intent.)
fn hint_value(raw: &str) -> Result<String, String> {
    let hint = raw.trim();
    if hint.is_empty() {
        Err("--hint must not be blank".to_string())
    } else {
        Ok(hint.to_string())
    }
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
    about = "An AI-powered Rust CLI for generating git commit messages in bulk.\naic[https://github.com/CaicoLeung/aic]",
    // `--hint` and `--no-redact` are Run-scoped, the only top-level user
    // args; pairing either with a subcommand must error, not silently drop
    // the intent.
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    /// One-off directive for this Run's commits — e.g. `aic --hint "breaking
    /// change"` or `aic --hint "closes #78"`. Appended to both the batch-plan
    /// and commit-message prompts, so it can steer grouping, type, scope, and
    /// body content. Per-Run intent only; there is deliberately no config
    /// field (a persisted hint would silently stamp every future commit).
    /// Run-scoped: rejected alongside a subcommand, and rejected when blank.
    #[arg(long, value_parser = hint_value)]
    pub hint: Option<String>,

    /// Skip the secrets Redaction Gate for this Run (issue #155): by default
    /// `aic` refuses to send a diff whose content looks like a secret (cloud
    /// keys, tokens, private key blocks) to the LLM, naming the offending
    /// file and pattern kind. This flag proceeds knowingly, sending the diff
    /// as-is. One-off CLI intent only; there is deliberately no config field
    /// — a persisted bypass would silently disable a safety gate for every
    /// future Run. Run-scoped: rejected alongside a subcommand.
    #[arg(long)]
    pub no_redact: bool,

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
    /// Undo the last commit Run: reset the repo back to where that Run
    /// started, with all its changes back in the working tree (unstaged)
    Undo,
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
    use clap::Parser;

    /// A blank `--hint` (empty or whitespace-only) is rejected at parse
    /// time — the Run must never append an empty "User directive" block.
    #[test]
    fn blank_hint_is_rejected_at_parse_time() {
        for raw in ["", " \t "] {
            let err = Cli::try_parse_from(["aic", "--hint", raw])
                .err()
                .expect("blank hint must fail to parse");
            assert!(
                err.to_string().contains("--hint must not be blank"),
                "raw {raw:?} got: {err}"
            );
        }
    }

    /// A hint is Run-scoped: pairing it with a subcommand is rejected
    /// instead of silently ignored (`aic --hint "x" undo` would otherwise
    /// swallow the directive).
    #[test]
    fn hint_cannot_be_combined_with_a_subcommand() {
        let err = Cli::try_parse_from(["aic", "--hint", "closes #78", "undo"])
            .err()
            .expect("hint with subcommand must fail to parse");
        assert!(
            err.to_string().contains("cannot be used with"),
            "got: {err}"
        );
    }

    /// A hint is trimmed on the way in, so the directive block carries the
    /// intent only — no stray shell quoting whitespace.
    #[test]
    fn hint_value_trims_surrounding_whitespace() {
        let cli = Cli::try_parse_from(["aic", "--hint", "  closes #78  "]).unwrap();
        assert_eq!(cli.hint.as_deref(), Some("closes #78"));
    }

    /// `--no-redact` is Run-scoped like `--hint`: pairing it with a
    /// subcommand is rejected instead of silently ignored.
    #[test]
    fn no_redact_cannot_be_combined_with_a_subcommand() {
        let err = Cli::try_parse_from(["aic", "--no-redact", "undo"])
            .err()
            .expect("--no-redact with subcommand must fail to parse");
        assert!(
            err.to_string().contains("cannot be used with"),
            "got: {err}"
        );
    }

    /// `--no-redact` parses as a plain flag with no value.
    #[test]
    fn no_redact_parses_as_a_bare_flag() {
        let cli = Cli::try_parse_from(["aic", "--no-redact"]).unwrap();
        assert!(cli.no_redact);
        let cli = Cli::try_parse_from(["aic"]).unwrap();
        assert!(!cli.no_redact);
    }

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
