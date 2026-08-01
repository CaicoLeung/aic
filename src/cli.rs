use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "aic",
    version,
    about = "An AI-powered Rust CLI for generating git commit messages in bulk.\naic[https://github.com/CaicoLeung/aic]"
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,

    /// Resume an interrupted batch-plan run from its frozen snapshot, instead
    /// of re-planning. Errors if no interrupted run is on disk.
    #[arg(long, conflicts_with = "no_resume")]
    pub resume: bool,

    /// Discard any interrupted run's state and start a fresh plan. Silences the
    /// auto-detected resume offer.
    #[arg(long, conflicts_with = "resume")]
    pub no_resume: bool,
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
    /// Generate shell completion script
    ///
    /// Prints a completion script for the given shell to stdout. Redirect it to
    /// the location your shell expects, for example:
    ///
    ///   aic generate-completion zsh > _aic   # place on a directory in $fpath
    GenerateCompletion {
        #[arg(value_enum)]
        shell: CompletionShell,
    },
}

#[derive(Debug, Clone, Copy, clap::ValueEnum, PartialEq, Eq)]
pub enum CompletionShell {
    #[value(name = "bash")]
    Bash,
    #[value(name = "elvish")]
    Elvish,
    #[value(name = "fish")]
    Fish,
    #[value(name = "nushell")]
    Nushell,
    #[value(name = "powershell")]
    PowerShell,
    #[value(name = "zsh")]
    Zsh,
    #[value(name = "carapace-spec")]
    Spec,
}
