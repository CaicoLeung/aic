pub mod cli;
pub mod config;
pub mod generator;
pub mod git;
pub mod llm;
pub mod prompt;
pub mod update;

use crate::cli::Commands;
use crate::git::Git;
use anyhow::Context;
use clap::Parser;
use indicatif::ProgressBar;
use owo_colors::OwoColorize;
use std::time::Duration;
use tui_banner::{Align, Banner, Fill, Style};

async fn with_spinner<F, T>(msg: &str, fut: F) -> anyhow::Result<T>
where
    F: std::future::Future<Output = anyhow::Result<T>>,
{
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::default_spinner()
            .template("{spinner} {msg}")?
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
    );
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));

    let result = fut.await;
    pb.finish_and_clear();
    result
}

fn format_rust_files(paths: &[String]) {
    let rust_files: Vec<&str> = paths
        .iter()
        .filter(|p| p.ends_with(".rs"))
        .map(|s| s.as_str())
        .collect();

    if rust_files.is_empty() {
        return;
    }

    // Use `cargo fmt --all` rather than bare `rustfmt`: bare rustfmt parses as
    // edition 2015 (no let-chains; different import ordering / construct
    // formatting), which diverges from CI's `cargo fmt --all -- --check` and
    // made commits fail CI. cargo fmt reads the edition from the manifest.
    match std::process::Command::new("cargo")
        .args(["fmt", "--all"])
        .status()
    {
        Ok(s) if s.success() => {
            eprintln!("🎨 Formatted {} file(s)", rust_files.len());
        }
        Ok(s) => {
            eprintln!("⚠️  rustfmt exited with {}", s);
        }
        Err(e) => {
            eprintln!("⚠️  Failed to run rustfmt: {e}");
        }
    }
}

async fn generate_and_commit(paths: &[String], _reason: Option<&str>) -> anyhow::Result<()> {
    let files: Vec<serde_json::Value> = paths
        .iter()
        .map(|p| {
            let diff = Git::diff(Some(p.as_str()))?;
            let scoped = git::format_diff_scoped(&diff, p);
            Ok(serde_json::json!({ "path": p, "diff": scoped }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let diff = serde_json::json!({ "staged_files": files });
    let result = with_spinner(
        "Generating commit message",
        generator::Generator::generate_commit_message(&diff.to_string()),
    )
    .await?;
    eprintln!("✏️  {}", result.message.bold().green());
    if let Some(body) = &result.body {
        for line in body.lines() {
            eprintln!("   {}", line.dimmed());
        }
    }
    eprintln!("📁 {}", paths.join(", ").cyan());
    Git::commit(result.message, result.body)?;
    Ok(())
}

async fn run_commit_workflow() -> anyhow::Result<()> {
    let status = Git::status()?;
    let staged_files: Vec<_> = status.iter().filter(|f| f.staged).collect();

    if staged_files.is_empty() {
        let unstaged_files: Vec<_> = status.iter().filter(|f| !f.staged).collect();
        let files: Vec<serde_json::Value> = unstaged_files
            .iter()
            .map(|f| {
                let diff = Git::diff_workdir(Some(f.path.as_str()))?;
                let scoped = git::format_diff_scoped(&diff, &f.path);
                Ok(serde_json::json!({ "path": f.path, "diff": scoped }))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let diff = serde_json::json!({ "unstaged_files": files });
        let result = with_spinner(
            "Analyzing changes",
            generator::Generator::split_patch(&diff.to_string()),
        )
        .await?;

        let all_unstaged: Vec<String> = unstaged_files.iter().map(|f| f.path.clone()).collect();
        format_rust_files(&all_unstaged);

        let original_paths: Vec<String> = all_unstaged;
        generator::validate_batch_plan(&result, &original_paths)
            .context("batch plan validation failed")?;

        let count = result.batches.len();
        if count == 1 {
            eprintln!("🔀 Grouped into {} commit", "1".bold().yellow());
        } else {
            eprintln!(
                "🔀 Split into {} commits",
                count.to_string().bold().yellow()
            );
        }
        for (i, batch) in result.batches.iter().enumerate() {
            let paths: Vec<&str> = batch.files.iter().map(|s| s.as_str()).collect();
            Git::add(&paths)?;
            if let Err(e) = generate_and_commit(&batch.files, batch.reason.as_deref()).await {
                anyhow::bail!(
                    "failed after committing {} of {} batches. \
                     Batch {} files are staged but uncommitted: {e}",
                    i,
                    count,
                    i + 1
                );
            }
        }
    } else {
        let paths: Vec<String> = staged_files.iter().map(|f| f.path.clone()).collect();
        format_rust_files(&paths);
        let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
        Git::add(&refs)?;
        generate_and_commit(&paths, None).await?;
    }

    Ok(())
}

fn banner() -> Banner {
    Banner::new("AIC")
        .expect("failed to create banner")
        .style(Style::FireWarning)
        .fill(Fill::Keep)
        .align(Align::Center)
        .padding(1)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let banner = banner();
    banner.animate_sweep(5, None)?;
    let cli = cli::Cli::parse();

    match cli.command {
        Some(Commands::Setup) => config::run_setup(),
        Some(Commands::List) => config::run_list(),
        Some(Commands::Update) => update::run_update(),
        None => run_commit_workflow().await,
    }
}
