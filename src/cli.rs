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
}

#[derive(Subcommand)]
pub enum Commands {
    /// Interactively configure LLM provider, API key, and model
    Setup,
    /// Show current resolved configuration
    List,
}
