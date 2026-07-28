pub mod cli;
pub mod config;
pub mod display;
pub mod generator;
pub mod git;
pub mod llm;
pub mod prompt;
pub mod update;

use crate::cli::Commands;
use crate::display::{BatchSummary, Display};
use crate::git::Git;
use anyhow::Context;
use clap::Parser;
use indicatif::ProgressBar;
use std::time::Duration;

async fn with_spinner<F, T>(msg: &str, fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
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
    pb.disable_steady_tick();
    pb.finish_and_clear();
    result
}

fn format_rust_files(paths: &[String], display: &Display) {
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
            display.formatted_notice(rust_files.len());
        }
        Ok(s) => {
            eprintln!("⚠️  rustfmt exited with {}", s);
        }
        Err(e) => {
            eprintln!("⚠️  Failed to run rustfmt: {e}");
        }
    }
}

async fn generate_and_commit(
    paths: &[String],
    display: &Display,
    prefix: &str,
) -> anyhow::Result<()> {
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
    let hash = Git::commit(result.message.clone(), result.body.clone())?;
    display.commit_line(&hash, &result.message, result.body.as_deref(), prefix);
    Ok(())
}

/// Read a y/n answer from stdin. The label is written to stderr (Display is
/// stderr-only) so piped stdout stays clean.
fn prompt_yes_no(label: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    eprint!("{label} [y/n] ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    Ok(matches!(input.trim().to_lowercase().as_str(), "y" | "yes"))
}

/// Unified line diff between two strings for the resolution review (ADR 0005).
/// Computed in-memory via `similar` so the diff is shown *before* the resolved
/// content is written to disk — review must precede `git add`.
fn unified_diff(old: &str, new: &str) -> String {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old, new);
    let mut out = String::new();
    for op in diff.ops() {
        for change in diff.iter_changes(op) {
            let sign = match change.tag() {
                ChangeTag::Delete => '-',
                ChangeTag::Insert => '+',
                ChangeTag::Equal => ' ',
            };
            out.push(sign);
            out.push_str(change.value());
            if !change.value().ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// `aic resolve` — resolve merge conflicts per-file via the LLM, review the
/// combined diff, apply approved files (sticky), and finalize when all are
/// resolved (ADR 0005).
async fn run_resolve_workflow() -> anyhow::Result<()> {
    let display = Display::new();
    let state = Git::state()?;

    if !state.is_conflicted() {
        display.no_conflicts();
        return Ok(());
    }
    if !state.resolvable() {
        // rebase / am — detected but refused in v1.
        display.refused(state);
        anyhow::bail!("aic cannot resolve a {} state in v1", state.label());
    }

    let files = Git::conflicted_files()?;
    if files.is_empty() {
        // Conflicted state but no unmerged index entries — the user resolved
        // every file by hand and only the finalize step remains.
        display.all_resolved_offer_finalize(state);
        if prompt_yes_no("finalize now?")? {
            Git::finalize(state)?;
            display.finalize_done(state);
        }
        return Ok(());
    }

    display.conflict_detected(state, files.len());
    display.conflicted_summary(&files);

    // Per-file resolution. `plans` carries (path, original, resolved) so the
    // review diff can be built without re-reading disk.
    let mut plans: Vec<(String, String, String)> = Vec::new();
    for f in &files {
        if !f.kind.resolvable() {
            display.skipped(&f.path, f.kind.reason());
            continue;
        }
        let original_bytes = Git::read_worktree(&f.path)?;
        let original = String::from_utf8(original_bytes)
            .with_context(|| format!("{} is not valid UTF-8 (should be Content)", f.path))?;

        let resolved = match with_spinner(
            &format!("Resolving {}", f.path),
            generator::Generator::resolve_conflict(&original),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                display.skipped(&f.path, &format!("LLM error: {e:#}"));
                continue;
            }
        };

        // Marker validation — auto-retry once (ADR 0005).
        let resolved = if git::has_conflict_markers(&resolved) {
            match with_spinner(
                &format!("Retrying {}", f.path),
                generator::Generator::resolve_conflict(&original),
            )
            .await
            {
                Ok(retry) if !git::has_conflict_markers(&retry) => retry,
                _ => {
                    display.skipped(&f.path, "markers remain after retry");
                    continue;
                }
            }
        } else {
            resolved
        };

        plans.push((f.path.clone(), original, resolved));
    }

    if plans.is_empty() {
        anyhow::bail!("no files could be resolved; resolve the conflicts manually");
    }

    // Combined review diff, then per-file sticky approval.
    let mut combined = String::new();
    for (path, original, resolved) in &plans {
        combined.push_str(&format!("--- {path} ---\n"));
        combined.push_str(&unified_diff(original, resolved));
        combined.push('\n');
    }
    display.review_section(&combined);

    let mut approved = 0usize;
    for (path, _original, resolved) in &plans {
        if prompt_yes_no(&format!("apply {path}?"))? {
            Git::write_worktree(path, resolved)?;
            Git::add(&[path.as_str()])?;
            display.resolved(path);
            approved += 1;
        } else {
            display.rejected(path);
        }
    }

    let unresolved = files.len() - approved;
    if unresolved == 0 {
        Git::finalize(state)?;
        display.finalize_done(state);
    } else {
        display.handoff(approved, unresolved, state);
    }

    Ok(())
}

async fn run_commit_workflow() -> anyhow::Result<()> {
    let display = Display::new();

    // Auto-detect a conflicted repo and offer `aic resolve` before the normal
    // stage+commit flow (ADR 0005). The commit guard in `Git::commit` is the
    // deeper net; this prompt is the friendly front door.
    let state = Git::state()?;
    if state.is_conflicted() {
        display.resolve_prompt(state);
        if prompt_yes_no("resolve now?")? {
            return run_resolve_workflow().await;
        }
        anyhow::bail!(
            "aborted: repo is mid-{}; resolve conflicts first",
            state.label()
        );
    }

    let status = Git::status()?;
    let staged_files: Vec<_> = status.iter().filter(|f| f.staged).collect();

    if staged_files.is_empty() {
        let unstaged_files: Vec<_> = status.iter().filter(|f| !f.staged).collect();
        let files: Vec<serde_json::Value> = unstaged_files
            .iter()
            .map(|f| {
                let diff = Git::diff_workdir(Some(f.path.as_str()))?;
                let scoped = git::format_diff_scoped(&diff, &f.path);
                Ok(serde_json::json!({ "path": f.path, "status": f.kind, "diff": scoped }))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let diff = serde_json::json!({ "unstaged_files": files });
        let result = with_spinner(
            "Analyzing changes",
            generator::Generator::split_patch(&diff.to_string()),
        )
        .await?;

        let all_unstaged: Vec<String> = unstaged_files.iter().map(|f| f.path.clone()).collect();
        format_rust_files(&all_unstaged, &display);

        let original_paths: Vec<String> = all_unstaged;
        generator::validate_batch_plan(&result, &original_paths)
            .context("batch plan validation failed")?;

        let batch_refs: Vec<BatchSummary<'_>> = result
            .batches
            .iter()
            .map(|b| BatchSummary {
                files: b.files.as_slice(),
                reason: b.reason.as_deref(),
            })
            .collect();
        display.batch_summary(&batch_refs);

        let count = result.batches.len();
        for (i, batch) in result.batches.iter().enumerate() {
            let paths: Vec<&str> = batch.files.iter().map(|s| s.as_str()).collect();
            Git::add(&paths)?;

            let prefix = format!("[{}/{count}]", i + 1);
            if let Err(e) = generate_and_commit(&batch.files, &display, &prefix).await {
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
        format_rust_files(&paths, &display);
        let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
        Git::add(&refs)?;
        generate_and_commit(&paths, &display, "").await?;
    }

    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        Some(Commands::Setup) => config::run_setup(),
        Some(Commands::List) => config::run_list(),
        Some(Commands::Update) => update::run_update(),
        Some(Commands::Resolve) => run_resolve_workflow().await,
        None => run_commit_workflow().await,
    }
}
