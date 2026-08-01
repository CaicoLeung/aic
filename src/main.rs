pub mod cli;
pub mod config;
pub mod display;
pub mod generator;
pub mod git;
pub mod llm;
pub mod prompt;
pub mod runstate;
pub mod types;
pub mod update;

#[cfg(test)]
mod e2e;

use crate::cli::Commands;
use crate::display::Display;
use crate::git::Git;
use anyhow::Context;
use clap::{CommandFactory, Parser};
use indicatif::ProgressBar;
use std::collections::{HashMap, HashSet};
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

/// Erased batch planner: takes the combined unstaged diff JSON and returns the
/// per-hunk batch plan. Boxed for the same reason as [`Resolver`] — tests swap
/// in a stub plan without touching the LLM.
pub(crate) type BatchPlanner =
    Box<dyn Fn(String) -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>>>;
/// Erased commit-message writer: takes one batch's staged diff JSON and returns
/// its Conventional-Commits message + body. Boxed for the same reason.
pub(crate) type CommitMessenger =
    Box<dyn Fn(String) -> BoxFuture<anyhow::Result<generator::CommitOutput>>>;

/// Shared indicatif spinner style: a braille tick and a prefix matching
/// [`display::MARGIN`] so the spinner glyph sits at the same 2-column inset as
/// the rest of the run's stderr block — not flush against the edge. One place
/// to change the inset or tick animation for every spinner in the run; the
/// prefix is sourced from `Display`'s margin constant instead of a literal
/// that has to be kept in sync by hand.
fn spinner_style() -> anyhow::Result<indicatif::ProgressStyle> {
    Ok(indicatif::ProgressStyle::default_spinner()
        .template(&format!("{}{{spinner}} {{msg}}", display::MARGIN))?
        .tick_chars("⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏"))
}

async fn with_spinner<F, T>(msg: &str, fut: F) -> anyhow::Result<T>
where
    F: Future<Output = anyhow::Result<T>>,
{
    let pb = ProgressBar::new_spinner();
    pb.set_style(spinner_style()?);
    pb.set_message(msg.to_string());
    pb.enable_steady_tick(Duration::from_millis(80));

    let result = fut.await;
    pb.disable_steady_tick();
    pb.finish_and_clear();
    result
}

/// How many reasoning lines the "Analyzing changes" spinner keeps on screen.
const THINKING_MAX_LINES: usize = 10;

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
    pb.set_style(spinner_style()?);
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
            display.warn(&format!("rustfmt exited with {}", s));
        }
        Err(e) => {
            display.warn(&format!("Failed to run rustfmt: {e}"));
        }
    }
}

async fn generate_and_commit(
    paths: &[String],
    display: &Display,
    prefix: &str,
    messenger: &CommitMessenger,
) -> anyhow::Result<(String, String)> {
    let files: Vec<serde_json::Value> = paths
        .iter()
        .map(|p| {
            let diff = Git::diff(Some(p.as_str()))?;
            let scoped = git::format_diff_scoped(&diff, p);
            Ok(serde_json::json!({ "path": p, "diff": scoped }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let diff = serde_json::json!({ "staged_files": files });
    let result = with_spinner("Generating commit message", messenger(diff.to_string())).await?;
    let hash = Git::commit(result.message.clone(), result.body.clone())?;
    display.commit_line(&hash, &result.message, result.body.as_deref(), prefix);
    Ok((hash, result.message))
}

/// One file's planned hunks for a batch, split into two views that must never
/// be confused:
///
/// - `original`: 1-based indices in the file's **plan-time** diff — the
///   numbering the model saw and the plan refers to. Recorded into `committed`
///   so it stays stable for the rest of the Run.
/// - `current`: matching 1-based positions in the file's **current**
///   index→workdir diff — what `Git::stage_hunks` must index into, because an
///   earlier batch (or a pre-commit hook) may have already landed some hunks
///   and shrunk the diff.
///
/// Hunks already committed are dropped from both. Surviving hunks keep their
/// original relative order, so an uncommitted original hunk `h` lands at
/// current position `h - (#committed original indices < h)`.
struct HunkMapping {
    original: Vec<usize>,
    current: Vec<usize>,
}

/// Resolve a batch's planned hunks for one file against what's already
/// committed. See [`HunkMapping`]. `planned` is the plan's `hunks` array for
/// the file (empty = every hunk); `committed` is the set of **original** hunk
/// indices already landed; `current_count` is how many hunks the current diff
/// still has. When `planned` is empty the original count is reconstructed as
/// `current_count + committed.len()`.
fn map_planned_hunks(
    planned: &[usize],
    committed: &HashSet<usize>,
    current_count: usize,
) -> HunkMapping {
    let wanted: Vec<usize> = if planned.is_empty() {
        (1..=(current_count + committed.len())).collect()
    } else {
        planned.to_vec()
    };
    let mut original = Vec::with_capacity(wanted.len());
    let mut current = Vec::with_capacity(wanted.len());
    for h in wanted {
        if committed.contains(&h) {
            continue;
        }
        let shift = committed.iter().filter(|&&c| c < h).count();
        original.push(h);
        current.push(h - shift);
    }
    HunkMapping { original, current }
}

/// Stage one batch's hunks from the *current* index→workdir diff and return
/// the paths actually staged — deduplicated by file, in first-seen order.
///
/// Staging from a fresh diff (rather than the plan-time snapshot) is what keeps
/// a live Run alive when a pre-commit hook (lint-staged/prettier) commits more
/// than the batch staged: such a hook re-adds whole files, so the first batch
/// to touch a file can land *all* of its hunks, leaving later batches nothing
/// to stage. Replaying the stale snapshot would die with `git apply`'s "patch
/// does not apply". Files whose changes already landed are skipped (with a
/// notice) and the run continues. Plan-time indices are remapped onto the
/// current diff via `committed_hunks`.
fn stage_batch_hunks(
    batch: &generator::BatchPlanBatch,
    committed_hunks: &mut HashMap<String, HashSet<usize>>,
    display: &Display,
) -> anyhow::Result<Vec<String>> {
    let mut files: Vec<String> = Vec::new();
    let mut hunks_by_file: HashMap<String, Vec<usize>> = HashMap::new();
    for change in &batch.changes {
        if !hunks_by_file.contains_key(&change.file) {
            files.push(change.file.clone());
        }
        hunks_by_file
            .entry(change.file.clone())
            .or_default()
            .extend_from_slice(&change.hunks);
    }

    let mut staged_paths: Vec<String> = Vec::new();
    for file in &files {
        let planned = hunks_by_file
            .get(file)
            .expect("every file in `files` has an entry in `hunks_by_file`");
        let current = Git::diff_workdir(Some(file.as_str()))?;
        if current.trim().is_empty() {
            display.warn(&format!(
                "{}: all its changes were already committed (a pre-commit hook may have \
                 staged the whole file) — nothing left in this batch",
                file
            ));
            continue;
        }
        let patch = git::parse_file_patch(&current);
        let committed = committed_hunks.entry(file.clone()).or_default();
        let mapping = map_planned_hunks(planned, committed, patch.hunks.len());
        if mapping.current.is_empty() {
            continue;
        }
        Git::stage_hunks(&current, &mapping.current)
            .with_context(|| format!("staging hunks for {}", file))?;
        committed.extend(mapping.original);
        staged_paths.push(file.clone());
    }
    Ok(staged_paths)
}

/// Replay one batch's hunks from the **frozen** plan-time snapshot captured at
/// plan time. Used only by the resume path, which recovers an interrupted Run
/// by replaying exactly what was captured — never re-formatting or re-capturing
/// diffs. Unlike the live [`stage_batch_hunks`], this stages the snapshot's
/// plan-time hunk numbering verbatim (after a `reset_index_to_head`), so it
/// does not remap against `committed_hunks`.
fn stage_batch_hunks_from_snapshot(
    batch: &generator::BatchPlanBatch,
    raw_diffs: &HashMap<String, String>,
) -> anyhow::Result<()> {
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
    Ok(())
}

/// De-duplicated file paths in a batch, in first-seen order. A file listed in
/// several `changes` entries of one batch still produces one commit message.
fn unique_batch_files(batch: &generator::BatchPlanBatch) -> Vec<String> {
    let mut paths: Vec<String> = Vec::new();
    for change in &batch.changes {
        if !paths.contains(&change.file) {
            paths.push(change.file.clone());
        }
    }
    paths
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
    planner: BatchPlanner,
    messenger: CommitMessenger,
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

    // Hold the advisory lock for the whole Run so two concurrent `aic` in one
    // worktree cannot corrupt the shared `.aic/active.json`. Dropped on return.
    let _lock = runstate::RunLock::acquire()?;

    // Resume offer: an interrupted Run's frozen plan may still be recoverable.
    // `--no-resume` is handled by the production wrapper (state cleared first);
    // `--resume` short-circuits to the replay path there and never reaches here.
    if let Some(prev) = runstate::RunState::load()? {
        display.resume_offer(
            prev.count_committed(),
            prev.batches.len(),
            prev.count_skipped(),
        );
        if prompt("resume this run?")? {
            return run_resume_workflow_impl(display, messenger, prev).await;
        }
        runstate::RunState::clear()?;
        runstate::log(&format!(
            "discarded previous run state ({} batch(es)) — starting fresh",
            prev.batches.len()
        ));
        display.resume_discarded();
    }

    let status = Git::status()?;
    let staged_files: Vec<_> = status.iter().filter(|f| f.staged).collect();

    if staged_files.is_empty() {
        let unstaged_files: Vec<_> = status.iter().filter(|f| !f.staged).collect();
        if unstaged_files.is_empty() {
            // Nothing staged *and* nothing unstaged — no work for the LLM.
            display.nothing_to_commit();
            return Ok(());
        }
        let all_unstaged: Vec<String> = unstaged_files.iter().map(|f| f.path.clone()).collect();

        // Format Rust files FIRST, so the diff the model sees — and the hunk
        // numbering we stage by — reflects the final formatted source. Doing it
        // after capturing the diff (as before) would let `cargo fmt` shift
        // hunks out from under the indices the model returned.
        format_rust_files(&all_unstaged, &display);

        // Capture each file's raw workdir-vs-HEAD diff once. This snapshot
        // feeds the model's numbered view, `file_hunk_counts` for plan
        // validation, and — persisted into the run state — the pure-snapshot
        // replay resume uses. Staging does NOT read from it: the live loop
        // re-reads a fresh diff per batch (so it survives pre-commit hooks that
        // re-stage whole files) via `stage_batch_hunks` + `committed_hunks`.
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
        let result = planner(diff.to_string()).await?;

        generator::validate_batch_plan(&result, &file_hunk_counts)
            .context("batch plan validation failed")?;

        let count = result.batches.len();

        // Persist the frozen plan so an interrupted Run can be replayed. The
        // fingerprint of every planned file is captured now and rechecked on
        // resume; a file the user mutates since plan time defers its batch.
        let created_at = runstate::epoch_now();
        let head = Git::head_short().unwrap_or_default();
        let mut file_hashes: HashMap<String, Option<String>> = HashMap::new();
        for batch in &result.batches {
            for f in batch.unique_files() {
                file_hashes
                    .entry(f.clone())
                    .or_insert_with(|| runstate::fingerprint(&f));
            }
        }
        let file_count = file_hashes.len();
        let mut rs = runstate::RunState {
            created_at,
            head_at_plan: head.clone(),
            plan: result.clone(),
            raw_diffs: raw_diffs.clone(),
            file_hashes,
            batches: vec![runstate::BatchEntry::Pending; count],
        };
        rs.save()?;
        runstate::log(&format!(
            "plan captured: {count} batch(es) over {file_count} file(s) (head {head})"
        ));
        let mut committed_hunks: HashMap<String, HashSet<usize>> = HashMap::new();

        for (i, batch) in result.batches.iter().enumerate() {
            let prefix = format!("[{}/{count}]", i + 1);
            // Stage this batch's hunks, then generate + commit. Either step
            // failing after earlier batches already committed leaves the repo
            // partially committed; the persisted state lets the Run resume.
            let outcome = async {
                let paths = stage_batch_hunks(batch, &mut committed_hunks, &display)?;
                if paths.is_empty() {
                    // Every file in this batch already landed via an earlier
                    // batch or a pre-commit hook — nothing to commit.
                    return Ok::<Option<(String, String)>, anyhow::Error>(None);
                }
                let committed = generate_and_commit(&paths, &display, &prefix, &messenger).await?;
                Ok(Some(committed))
            };
            match outcome.await {
                Ok(Some((sha, msg))) => {
                    rs.batches[i] = runstate::BatchEntry::Committed { sha: sha.clone() };
                    let _ = rs.save();
                    runstate::log(&format!(
                        "batch {}/{} committed {sha}: {}",
                        i + 1,
                        count,
                        msg.lines().next().unwrap_or("")
                    ));
                }
                Ok(None) => {
                    // Nothing staged (pre-commit hook landed it earlier) —
                    // record the batch as done so resume doesn't re-attempt it.
                    rs.batches[i] = runstate::BatchEntry::Committed { sha: String::new() };
                    let _ = rs.save();
                    runstate::log(&format!(
                        "batch {}/{} had nothing left to stage — recorded as done",
                        i + 1,
                        count
                    ));
                }
                Err(e) => {
                    runstate::log(&format!("batch {}/{} failed: {e:#}", i + 1, count));
                    anyhow::bail!(
                        "aborted on batch {} of {} after {} batch(es) committed. \
                         Re-run `aic` to resume the remaining batches, or `aic --no-resume` \
                         to discard the plan and start fresh: {e:#}",
                        i + 1,
                        count,
                        i
                    );
                }
            }
        }

        let _ = runstate::RunState::clear();
        runstate::log(&format!("run completed: {count} batch(es) committed"));
    } else {
        let paths: Vec<String> = staged_files.iter().map(|f| f.path.clone()).collect();
        format_rust_files(&paths, &display);
        let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
        Git::add(&refs)?;
        generate_and_commit(&paths, &display, "", &messenger).await?;
    }

    Ok(())
}

/// Replay the pending batches of an interrupted Run from its frozen snapshot.
/// Already-committed batches are skipped; batches whose files changed since plan
/// time are deferred (left unstaged, never lost). Never re-plans or re-captures
/// diffs — pure replay of the captured hunks via the proven staging path.
pub(crate) async fn run_resume_workflow_impl(
    display: Display,
    messenger: CommitMessenger,
    mut rs: runstate::RunState,
) -> anyhow::Result<()> {
    let count = rs.batches.len();
    let committed_before = rs.count_committed();
    runstate::log(&format!(
        "resume started: {committed_before}/{count} batch(es) already committed"
    ));
    display.resume_start(committed_before, count);

    // Integrity: defer pending batches whose files drifted since plan time.
    for (i, files) in rs.integrity_violations() {
        rs.batches[i] = runstate::BatchEntry::Skipped {
            reason: format!("files changed since plan: {}", files.join(", ")),
        };
        display.resume_skipped(i + 1, &files);
        runstate::log(&format!(
            "batch {}/{} deferred: files changed since plan: {}",
            i + 1,
            count,
            files.join(", ")
        ));
    }
    let _ = rs.save();

    // Replay every still-pending batch from the frozen diffs.
    for i in rs.pending_indices() {
        let batch = &rs.plan.batches[i];
        let prefix = format!("[{}/{count}]", i + 1);
        let paths = unique_batch_files(batch);
        // A prior failed attempt may have staged this batch's hunks without
        // committing; reset to HEAD so re-staging starts from a clean index.
        Git::reset_index_to_head()?;
        stage_batch_hunks_from_snapshot(batch, &rs.raw_diffs)?;
        let (sha, msg) = generate_and_commit(&paths, &display, &prefix, &messenger).await?;
        rs.batches[i] = runstate::BatchEntry::Committed { sha: sha.clone() };
        let _ = rs.save();
        runstate::log(&format!(
            "batch {}/{} committed {sha}: {}",
            i + 1,
            count,
            msg.lines().next().unwrap_or("")
        ));
    }

    let committed = rs.count_committed();
    let skipped = rs.count_skipped();
    let _ = runstate::RunState::clear();
    if skipped == 0 {
        runstate::log(&format!(
            "resume completed: {committed}/{count} batch(es) committed"
        ));
        display.resume_completed(committed, count);
    } else {
        runstate::log(&format!(
            "resume completed: {committed}/{count} committed, {skipped} deferred"
        ));
        display.resume_completed_with_skipped(committed, count, skipped);
    }
    Ok(())
}

/// Production entry point for the default `aic` run — wires the real LLM
/// resolver and stdin y/n prompt into [`run_commit_workflow_impl`].
///
/// `resume` mirrors the CLI flags:
/// - `Some(true)`  → `--resume`: short-circuit straight to the replay path.
/// - `Some(false)` → `--no-resume`: clear any in-flight state, then run fresh.
/// - `None`        → auto: the resume offer surfaces inside the workflow.
async fn run_commit_workflow(resume: Option<bool>) -> anyhow::Result<()> {
    let resolver: Resolver = Box::new(|content: String| -> BoxFuture<anyhow::Result<String>> {
        Box::pin(async move { generator::Generator::resolve_conflict(&content).await })
    });
    let prompt: Prompt = Box::new(prompt_yes_no);
    let planner: BatchPlanner = Box::new(
        |diff: String| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
            Box::pin(async move { analyze_changes(&diff).await })
        },
    );
    let messenger: CommitMessenger = Box::new(
        |diff: String| -> BoxFuture<anyhow::Result<generator::CommitOutput>> {
            Box::pin(async move { generator::Generator::generate_commit_message(&diff).await })
        },
    );

    match resume {
        // `--resume`: jump straight to replay. No state → nothing to resume.
        Some(true) => match runstate::RunState::load()? {
            Some(rs) => {
                // Hold the advisory lock for the replay just as the live Run
                // does, so two concurrent `aic --resume` cannot both replay.
                let _lock = runstate::RunLock::acquire()?;
                return run_resume_workflow_impl(Display::new(), messenger, rs).await;
            }
            None => {
                eprintln!("no interrupted run to resume");
                return Ok(());
            }
        },
        // `--no-resume`: discard any in-flight plan before a fresh run.
        Some(false) => {
            let _ = runstate::RunState::clear();
        }
        None => {}
    }

    run_commit_workflow_impl(resolver, prompt, Display::new(), planner, messenger).await
}

/// Writes the completion script for `shell` to `out`.
///
/// Factored out of `main` so the code path can be exercised from tests with
/// an in-memory buffer instead of stdout. `clap_complete::generate` itself
/// returns `()` and does not surface write errors, so this helper mirrors that
/// contract rather than silently swallowing a `Result`.
fn write_completion(shell: cli::CompletionShell, out: &mut dyn std::io::Write) {
    use carapace_spec_clap::Spec;
    use clap_complete::{Shell, generate};
    use clap_complete_nushell::Nushell;

    // Build a fresh `Command` from the derive. `bin_name` is owned so it does
    // not alias `cmd`, which `generate` needs mutably.
    let mut cmd = cli::Cli::command();
    let bin_name = cmd.get_name().to_owned();

    match shell {
        cli::CompletionShell::Bash => generate(Shell::Bash, &mut cmd, &bin_name, out),
        cli::CompletionShell::Elvish => generate(Shell::Elvish, &mut cmd, &bin_name, out),
        cli::CompletionShell::Fish => generate(Shell::Fish, &mut cmd, &bin_name, out),
        cli::CompletionShell::PowerShell => generate(Shell::PowerShell, &mut cmd, &bin_name, out),
        cli::CompletionShell::Zsh => generate(Shell::Zsh, &mut cmd, &bin_name, out),
        cli::CompletionShell::Nushell => generate(Nushell, &mut cmd, &bin_name, out),
        cli::CompletionShell::Spec => generate(Spec, &mut cmd, &bin_name, out),
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();
    let resume = if cli.resume {
        Some(true)
    } else if cli.no_resume {
        Some(false)
    } else {
        None
    };

    match cli.command {
        Some(Commands::Setup) => config::run_setup(),
        Some(Commands::List) => config::run_list(),
        Some(Commands::Update) => update::run_update(),
        Some(Commands::Resolve) => run_resolve_workflow().await,
        Some(Commands::GenerateCompletion { shell }) => {
            write_completion(shell, &mut std::io::stdout());
            Ok(())
        }
        None => run_commit_workflow(resume).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_completion_emits_nonempty_script_naming_aic_for_every_shell() {
        use crate::cli::CompletionShell;

        for shell in [
            CompletionShell::Bash,
            CompletionShell::Elvish,
            CompletionShell::Fish,
            CompletionShell::Nushell,
            CompletionShell::PowerShell,
            CompletionShell::Zsh,
            CompletionShell::Spec,
        ] {
            let mut buf = Vec::new();
            write_completion(shell, &mut buf);
            let script = String::from_utf8(buf).expect("completion output must be valid UTF-8");
            assert!(!script.is_empty(), "{shell:?}: completion script was empty");
            assert!(
                script.contains("aic"),
                "{shell:?}: completion script did not reference the `aic` binary"
            );
        }
    }

    #[test]
    fn thinking_view_keeps_last_n_lines_and_drops_blanks() {
        let mut v = ThinkingView::new();
        for i in 1..=12 {
            v.push(&format!("line {i}\n\n"));
        }
        // lines 1-2 scrolled off; lines 3-12 remain (blank lines dropped).
        let mut expected = vec!["Analyzing changes".to_string()];
        for i in 3..=12 {
            expected.push(format!("  │ line {i}"));
        }
        let rendered = v.render("Analyzing changes");
        assert_eq!(rendered.lines().collect::<Vec<_>>(), expected);
    }

    #[test]
    fn thinking_view_shows_partial_line_and_caps_at_max() {
        let mut v = ThinkingView::new();
        for i in 1..=THINKING_MAX_LINES {
            v.push(&format!("line {i}\n"));
        }
        v.push("in progress"); // partial current line (no trailing newline)
        let rendered = v.render("Analyzing changes");
        let visible: Vec<&str> = rendered.lines().collect();
        // max complete + 1 partial → render caps at THINKING_MAX_LINES lines.
        assert_eq!(visible.len(), 1 + THINKING_MAX_LINES);
        assert_eq!(visible.last(), Some(&"  │ in progress"));
        // "line 1" scrolled off; the partial takes the 10th slot.
        assert!(!visible.iter().any(|l| l.ends_with("line 1")));
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

    #[test]
    fn map_planned_hunks_splits_original_and_current_indices() {
        use std::collections::HashSet;
        // Hunks 1-2 already committed → plan's 3-7 survive as original 3-7 and
        // map onto the current diff's 1-5. The two views differ precisely when
        // something is committed; conflating them is the bug this struct exists
        // to prevent, so assert both.
        let committed: HashSet<usize> = [1usize, 2].into_iter().collect();
        let m = map_planned_hunks(&[3, 4, 5, 6, 7], &committed, 5);
        assert_eq!(m.original, vec![3, 4, 5, 6, 7]);
        assert_eq!(m.current, vec![1, 2, 3, 4, 5]);

        // Empty planned hunks = every hunk; original count = current + committed.
        let m = map_planned_hunks(&[], &committed, 5);
        assert_eq!(m.original, vec![3, 4, 5, 6, 7]);
        assert_eq!(m.current, vec![1, 2, 3, 4, 5]);

        // Nothing committed yet → both views pass through unchanged.
        let m = map_planned_hunks(&[1, 2], &HashSet::new(), 2);
        assert_eq!(m.original, vec![1, 2]);
        assert_eq!(m.current, vec![1, 2]);

        // Three hunks of one file across three batches, no hook: storing the
        // remapped position instead of the original used to corrupt `committed`
        // and make batch 3 address a non-existent hunk.
        let mut committed: HashSet<usize> = [1usize].into_iter().collect();
        let m = map_planned_hunks(&[2], &committed, 2);
        assert_eq!(m.original, vec![2]);
        assert_eq!(m.current, vec![1]);
        committed.extend(m.original);
        let m = map_planned_hunks(&[3], &committed, 1);
        assert_eq!(m.original, vec![3]);
        assert_eq!(m.current, vec![1]);
    }
}
