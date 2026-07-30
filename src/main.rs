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
use crate::display::Display;
use crate::git::Git;
use anyhow::Context;
use clap::Parser;
use indicatif::ProgressBar;
use std::collections::HashMap;
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

/// How many reasoning lines the "Analyzing changes" spinner keeps on screen.
const THINKING_MAX_LINES: usize = 5;

/// A rolling window over the model's streamed reasoning, kept to the last
/// [`THINKING_MAX_LINES`] non-blank lines. Rendered in place under the spinner
/// so it reads like a scrolling "thinking" feed.
struct ThinkingView {
    lines: Vec<String>,
    cur: String,
}

impl ThinkingView {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            cur: String::new(),
        }
    }

    /// Ingest a reasoning delta (may be a partial line, many lines, or empty).
    /// Blank lines are dropped to keep the window information-dense.
    fn push(&mut self, delta: &str) {
        for ch in delta.chars() {
            if ch == '\n' {
                let line = std::mem::take(&mut self.cur);
                if !line.trim().is_empty() {
                    self.lines.push(line);
                    if self.lines.len() > THINKING_MAX_LINES {
                        self.lines.remove(0);
                    }
                }
            } else {
                self.cur.push(ch);
            }
        }
    }

    /// `title` (e.g. "Analyzing changes") on the first line, then up to
    /// [`THINKING_MAX_LINES`] reasoning lines indented under it — the latest
    /// visible, older ones having scrolled off.
    fn render(&self, title: &str) -> String {
        let width = terminal_width().saturating_sub(6).clamp(20, 200);
        let mut out = String::from(title);
        let mut shown = self.lines.clone();
        if !self.cur.trim().is_empty() {
            shown.push(self.cur.clone());
        }
        let start = shown.len().saturating_sub(THINKING_MAX_LINES);
        for line in &shown[start..] {
            out.push_str("\n  │ ");
            out.push_str(&truncate(line, width));
        }
        out
    }
}

fn terminal_width() -> usize {
    std::env::var("COLUMNS")
        .ok()
        .and_then(|s| s.parse().ok())
        .filter(|&w: &usize| (20..=500).contains(&w))
        .unwrap_or(100)
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut t: String = s.chars().take(max.saturating_sub(1)).collect();
    t.push('…');
    t
}

/// Run the batch-plan analysis behind a spinner that streams the model's
/// reasoning live, keeping the latest [`THINKING_MAX_LINES`] lines on screen.
async fn analyze_changes(diff: &str) -> anyhow::Result<generator::BatchPlanOutput> {
    let pb = ProgressBar::new_spinner();
    pb.set_style(
        indicatif::ProgressStyle::default_spinner()
            .template("{spinner} {msg}")?
            .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"),
    );
    pb.set_message("Analyzing changes");
    pb.enable_steady_tick(Duration::from_millis(80));

    let mut view = ThinkingView::new();
    let result = generator::Generator::split_patch_streaming(diff, |delta| {
        view.push(delta);
        pb.set_message(view.render("Analyzing changes"));
    })
    .await;

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
/// `resolve`, `prompt`, and `display` are seams so the full workflow can be
/// driven end-to-end in tests without a live LLM, a TTY, or capturing real
/// stderr. Production callers use [`run_resolve_workflow`], which wires in
/// `Generator::resolve_conflict`, stdin `prompt_yes_no`, and [`Display::new`].
pub(crate) async fn run_resolve_workflow_impl(
    resolve: Resolver,
    prompt: Prompt,
    display: Display,
) -> anyhow::Result<()> {
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
    run_resolve_workflow_impl(resolver, prompt, Display::new()).await
}

/// Default `aic` run. `resolve`/`prompt`/`display` are seams mirroring
/// [`run_resolve_workflow_impl`]; they only matter on the conflicted-repo
/// auto-detect branch, which hands off to the resolve workflow.
pub(crate) async fn run_commit_workflow_impl(
    resolve: Resolver,
    prompt: Prompt,
    display: Display,
) -> anyhow::Result<()> {
    // Auto-detect a conflicted repo and offer `aic resolve` before the normal
    // stage+commit flow (ADR 0005). The commit guard in `Git::commit` is the
    // deeper net; this prompt is the friendly front door.
    let state = Git::state()?;
    if state.is_conflicted() {
        display.resolve_prompt(state);
        if prompt("resolve now?")? {
            return run_resolve_workflow_impl(resolve, prompt, display).await;
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
        let all_unstaged: Vec<String> = unstaged_files.iter().map(|f| f.path.clone()).collect();

        // Format Rust files FIRST, so the diff the model sees — and the hunk
        // numbering we stage by — reflects the final formatted source. Doing it
        // after capturing the diff (as before) would let `cargo fmt` shift
        // hunks out from under the indices the model returned.
        format_rust_files(&all_unstaged, &display);

        // Capture each file's raw workdir-vs-HEAD diff once. The numbered view
        // goes to the model; the raw hunks are staged per-batch. Numbering is
        // stable because both derive from this same snapshot.
        let mut raw_diffs: HashMap<String, String> = HashMap::new();
        let mut file_hunk_counts: Vec<(String, usize)> = Vec::new();
        let files: Vec<serde_json::Value> = unstaged_files
            .iter()
            .map(|f| {
                let diff = Git::diff_workdir(Some(f.path.as_str()))?;
                let hunk_count = git::parse_file_patch(&diff).hunks.len();
                raw_diffs.insert(f.path.clone(), diff.clone());
                file_hunk_counts.push((f.path.clone(), hunk_count));
                let scoped = git::format_diff_scoped(&diff, &f.path);
                Ok(serde_json::json!({ "path": f.path, "status": f.kind, "diff": scoped }))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let diff = serde_json::json!({ "unstaged_files": files });
        let result = analyze_changes(&diff.to_string()).await?;

        generator::validate_batch_plan(&result, &file_hunk_counts)
            .context("batch plan validation failed")?;

        let count = result.batches.len();
        for (i, batch) in result.batches.iter().enumerate() {
            // Stage only this batch's hunks — `git add -p` style — so a single
            // file can be committed across several batches.
            for change in &batch.changes {
                let raw = raw_diffs
                    .get(&change.file)
                    .with_context(|| format!("no captured diff for {}", change.file))?;
                let hunks: Vec<usize> = if change.hunks.is_empty() {
                    (1..=git::parse_file_patch(raw).hunks.len()).collect()
                } else {
                    change.hunks.clone()
                };
                Git::stage_hunks(raw, &hunks)
                    .with_context(|| format!("staging hunks for {}", change.file))?;
            }

            // Unique files in this batch drive the message (a file may appear
            // in more than one change within a batch).
            let mut paths: Vec<String> = Vec::new();
            for change in &batch.changes {
                if !paths.contains(&change.file) {
                    paths.push(change.file.clone());
                }
            }

            let prefix = format!("[{}/{count}]", i + 1);
            if let Err(e) = generate_and_commit(&paths, &display, &prefix).await {
                anyhow::bail!(
                    "failed after committing {} of {} batches. \
                     Batch {} changes are staged but uncommitted: {e}",
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
    run_commit_workflow_impl(resolver, prompt, Display::new()).await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_view_keeps_last_five_lines_and_drops_blanks() {
        let mut v = ThinkingView::new();
        for i in 1..=8 {
            v.push(&format!("line {i}\n\n"));
        }
        // lines 1-3 scrolled off; lines 4-8 remain (blank lines dropped).
        let rendered = v.render("Analyzing changes");
        assert_eq!(
            rendered.lines().collect::<Vec<_>>(),
            vec![
                "Analyzing changes",
                "  │ line 4",
                "  │ line 5",
                "  │ line 6",
                "  │ line 7",
                "  │ line 8",
            ]
        );
    }

    #[test]
    fn thinking_view_shows_partial_line_and_caps_at_five() {
        let mut v = ThinkingView::new();
        v.push("a\nb\nc\nd\n");
        v.push("in progress"); // no trailing newline → partial current line
        let rendered = v.render("Analyzing changes");
        let visible: Vec<&str> = rendered.lines().collect();
        // 4 complete + 1 partial = 5 reasoning lines under the title.
        assert_eq!(visible.len(), 1 + THINKING_MAX_LINES);
        assert_eq!(visible.last(), Some(&"  │ in progress"));
    }

    #[test]
    fn thinking_view_assembles_split_chunks() {
        let mut v = ThinkingView::new();
        // one logical line delivered across several deltas
        v.push("hel");
        v.push("lo");
        v.push(" world\n");
        assert_eq!(
            v.render("t").lines().collect::<Vec<_>>(),
            vec!["t", "  │ hello world"]
        );
    }
}
