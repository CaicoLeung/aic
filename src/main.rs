pub mod cli;
pub mod config;
pub mod display;
pub mod generator;
pub mod git;
pub mod llm;
pub mod prompt;
pub mod update;

#[cfg(test)]
mod e2e;

use crate::cli::Commands;
use crate::display::{BatchSummary, Display};
use crate::git::Git;
use anyhow::Context;
use clap::Parser;
use indicatif::ProgressBar;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// A boxed, `Send` future — the return type of the resolver seam.
pub(crate) type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// Erased resolver: a closure that takes the conflicted file content and
/// returns a future yielding the resolved (marker-free) content. Boxed so the
/// workflow signature stays concrete — no generic `where` clauses — while tests
/// can swap in stubs without touching the LLM.
pub(crate) type Resolver = Box<dyn Fn(String) -> BoxFuture<anyhow::Result<String>>>;
/// Erased y/n prompt: answers a labeled question. Boxed for the same reason.
pub(crate) type Prompt = Box<dyn Fn(&str) -> anyhow::Result<bool>>;

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
///
/// `resolve` and `prompt` are seams so the full workflow can be driven
/// end-to-end in tests without a live LLM or a TTY. Production callers use
/// [`run_resolve_workflow`], which wires in `Generator::resolve_conflict` and
/// stdin `prompt_yes_no`.
pub(crate) async fn run_resolve_workflow_impl(
    resolve: Resolver,
    prompt: Prompt,
) -> anyhow::Result<()> {
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
        if prompt("finalize now?")? {
            Git::finalize(state)?;
            display.finalize_done(state);
        }
        return Ok(());
    }

    display.conflict_detected(state, files.len());
    display.conflicted_summary(&files);

    // Per-file resolution. `plans` carries (path, original, resolved) so the
    // review diff can be built without re-reading disk. Track why each
    // non-approved file didn't make it into `plans` so the hand-off message
    // can distinguish structural skips (binary/oversized — user must resolve
    // by hand) from transient failures (LLM error / markers after retry —
    // user can re-run `aic resolve`).
    let mut plans: Vec<(String, String, String)> = Vec::new();
    let mut skipped_unresolvable = 0usize;
    let mut skipped_failed = 0usize;
    for f in &files {
        if !f.kind.resolvable() {
            display.skipped(&f.path, f.kind.reason());
            skipped_unresolvable += 1;
            continue;
        }
        let original_bytes = Git::read_worktree(&f.path)?;
        let original = String::from_utf8(original_bytes)
            .with_context(|| format!("{} is not valid UTF-8 (should be Content)", f.path))?;

        let resolved =
            match with_spinner(&format!("Resolving {}", f.path), resolve(original.clone())).await {
                Ok(r) => r,
                Err(e) => {
                    display.skipped(&f.path, &format!("LLM error: {e:#}"));
                    skipped_failed += 1;
                    continue;
                }
            };

        // Marker validation — auto-retry once (ADR 0005).
        let resolved = if git::has_conflict_markers(&resolved) {
            match with_spinner(&format!("Retrying {}", f.path), resolve(original.clone())).await {
                Ok(retry) if !git::has_conflict_markers(&retry) => retry,
                _ => {
                    display.skipped(&f.path, "markers remain after retry");
                    skipped_failed += 1;
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

    // Combined review diff, then per-file sticky approval. Each file's path
    // is emitted bare on its own line so `review_section` can render it as a
    // header — unified-diff bodies only ever start with `+`/`-`/` `, so a
    // bare path is unambiguously a boundary (a `--- path ---` prefix would
    // tint it red as a deletion line).
    let mut combined = String::new();
    for (path, original, resolved) in &plans {
        combined.push_str(path);
        combined.push('\n');
        combined.push_str(&unified_diff(original, resolved));
        combined.push('\n');
    }
    display.review_section(&combined);

    let mut approved = 0usize;
    let mut rejected = 0usize;
    for (path, _original, resolved) in &plans {
        if prompt(&format!("apply {path}?"))? {
            Git::write_worktree(path, resolved)?;
            Git::add(&[path.as_str()])?;
            display.resolved(path);
            approved += 1;
        } else {
            display.rejected(path);
            rejected += 1;
        }
    }

    // Finalize is all-or-nothing: git's `--continue` blocks on any unmerged
    // path regardless (ADR 0005), so a single remaining blocker stops it.
    // Three blocker kinds, tracked separately so the hand-off message tells
    // the user what to do next instead of an opaque count:
    //   - rejected             resolvable file the user declined to apply
    //   - skipped_failed       resolvable kind, but the LLM/markers step failed
    //   - skipped_unresolvable binary / oversized / delete-modify — aic can't
    // Equivalent to the old `files.len() - approved == 0` test (every file
    // lands in exactly one of: skipped_unresolvable, skipped_failed, plans).
    let needs_manual = rejected + skipped_failed + skipped_unresolvable;
    if needs_manual == 0 {
        Git::finalize(state)?;
        display.finalize_done(state);
    } else {
        display.handoff(
            approved,
            rejected,
            skipped_failed,
            skipped_unresolvable,
            state,
        );
    }

    Ok(())
}

/// Production entry point for `aic resolve` — wires the real LLM resolver and
/// stdin y/n prompt into [`run_resolve_workflow_impl`].
async fn run_resolve_workflow() -> anyhow::Result<()> {
    let resolver: Resolver = Box::new(|content: String| -> BoxFuture<anyhow::Result<String>> {
        Box::pin(async move { generator::Generator::resolve_conflict(&content).await })
    });
    let prompt: Prompt = Box::new(prompt_yes_no);
    run_resolve_workflow_impl(resolver, prompt).await
}

/// Default `aic` run. `resolve`/`prompt` are seams mirroring
/// [`run_resolve_workflow_impl`]; they only matter on the conflicted-repo
/// auto-detect branch, which hands off to the resolve workflow.
pub(crate) async fn run_commit_workflow_impl(
    resolve: Resolver,
    prompt: Prompt,
) -> anyhow::Result<()> {
    let display = Display::new();

    // Auto-detect a conflicted repo and offer `aic resolve` before the normal
    // stage+commit flow (ADR 0005). The commit guard in `Git::commit` is the
    // deeper net; this prompt is the friendly front door.
    let state = Git::state()?;
    if state.is_conflicted() {
        display.resolve_prompt(state);
        if prompt("resolve now?")? {
            return run_resolve_workflow_impl(resolve, prompt).await;
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

/// Production entry point for the default `aic` run — wires the real LLM
/// resolver and stdin y/n prompt into [`run_commit_workflow_impl`].
async fn run_commit_workflow() -> anyhow::Result<()> {
    let resolver: Resolver = Box::new(|content: String| -> BoxFuture<anyhow::Result<String>> {
        Box::pin(async move { generator::Generator::resolve_conflict(&content).await })
    });
    let prompt: Prompt = Box::new(prompt_yes_no);
    run_commit_workflow_impl(resolver, prompt).await
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
