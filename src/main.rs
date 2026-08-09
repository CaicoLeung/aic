pub mod cli;
pub mod cli_agent;
pub mod completion;
pub mod config;
pub mod confirm;
pub mod conflict;
pub mod diff;
pub mod display;
pub mod generator;
pub mod git;
pub mod grouping;
pub mod input;
pub mod layout;
pub mod llm;
pub mod progress;
pub mod prompt;
pub mod retry;
pub mod setup;
pub mod staging;
pub mod types;
pub mod update;

#[cfg(test)]
mod e2e;

use crate::cli::Commands;
use crate::confirm::{CommitDeclined, Confirm, confirm_draft, ensure_confirm_terminal};
use crate::display::Display;
use crate::git::Git;
use anyhow::Context;
use clap::Parser;
use std::future::Future;
use std::io::IsTerminal;
use std::path::Path;
use std::pin::Pin;

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

/// Run the batch-plan analysis behind a spinner that streams the model's
/// reasoning live. The reasoning is shown as a rolling
/// [`progress::REASONING_WINDOW`]-row block that redraws in place as the
/// model thinks — newest rows at the bottom, oldest scrolled out of the
/// window — and is erased when thinking ends, so the reasoning never lingers
/// on screen or in the scrollback. The cap bounds the in-place block while
/// it streams, even when a line wraps long.
///
/// Rendering is hand-rolled via [`progress::ReasoningRenderer`] rather than an
/// indicatif multi-line spinner: indicatif repaints by blanking every row then
/// redrawing them, and its steady tick forced that ~20×/s, so a multi-row
/// window flickered. The renderer clears and rewrites one row at a time (any
/// instant has at most one blank row) and repaints only on a reasoning change,
/// so the window is flicker-free. See [`progress::ReasoningRenderer`] for the
/// redraw contract.
///
/// **Silent-backend fallback.** A backend that emits no reasoning deltas at
/// all (a CLI agent in single-shot print mode, or an API cold start) would
/// leave the renderer's first frame unpainted — a dead silent screen for the
/// whole run. To avoid that, a loading frame
/// ([`progress::ReasoningRenderer::paint_loading`]) is painted on a
/// [`progress::SPINNER_TICK`] cadence between stream start and the first
/// delta: a spinner + elapsed-seconds counter immediately, and after
/// [`progress::LOADING_GRACE`] an explanatory notice that this CLI agent does
/// not stream its thinking process. The first delta cancels loading and hands
/// off to the normal reasoning window; both frames go through the same
/// [`progress::ReasoningRenderer`] so the in-place repaint handles the height
/// transition with no flicker. Reasoning deltas flow through an unbounded
/// channel so the streaming future (which owns the [`progress::ThinkingView`]
/// inside its callback closure) and the repaint loop never borrow-conflict —
/// the future writes windows, the loop paints them.
async fn analyze_changes(diff: &str) -> anyhow::Result<generator::BatchPlanOutput> {
    use std::time::Instant;

    let mut renderer = progress::ReasoningRenderer::new("Analyzing changes");
    let start = Instant::now();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();

    // A streaming-capable backend (claude `stream-json`, pi `--mode json`)
    // that has not yet produced reasoning is in a cold start — SessionStart
    // hooks, MCP handshakes, and a network TTFT often total 6–10 s before
    // the first `thinking_delta`. That is NOT "does not support streaming",
    // so past [`progress::LOADING_GRACE`] its loading frame shows a
    // cold-start notice rather than the non-streaming claim. Plain backends
    // (and opencode/codex, whose reasoning arrives whole at completion) are
    // the case the non-streaming notice was written for.
    //
    // The decision — "is this a streaming-capable cold start, and what name
    // labels the notice" — is decided behind the Backend seam by
    // [`LlmConfig::cold_start_program`], so this frame never branches on
    // backend kind or encoding. `None` ⇒ not streaming-capable → the silent
    // notice. Defaults to `None` on any config-read glitch (safer: never
    // falsely claims a streaming capability).
    let cold_start: Option<String> =
        crate::llm::LlmConfig::load().ok().and_then(|c| c.cold_start_program());

    // The streaming future owns the `ThinkingView` inside its `on_reasoning`
    // closure; windows are forwarded to the channel rather than rendered
    // inline, so the repaint loop below holds the renderer with no overlapping
    // borrow of the view. `tokio::pin!` lets us poll it across `select!`
    // arms without re-creating it each iteration.
    let fut = async {
        let mut view = progress::ThinkingView::new();
        generator::Generator::split_patch_streaming(diff, |delta| {
            let window = view.push(delta);
            // Channel send only fails if the receiver was dropped — which
            // happens after `fut` completes, when pending windows no longer
            // matter — so the error is silently swallowed.
            let _ = tx.send(window);
        })
        .await
    };
    tokio::pin!(fut);

    let mut got_output = false;
    let mut last_window: Vec<String> = Vec::new();
    let mut ticker = tokio::time::interval(progress::SPINNER_TICK);
    // The interval's first tick fires immediately; we want the first painted
    // frame to reflect a real (even if 0 s) elapsed, so the immediate tick is
    // welcome — it gives sub-second feedback before any LLM round-trip.

    let result = loop {
        tokio::select! {
            biased;
            // Completion wins over everything: a fast backend never paints a
            // loading frame at all.
            res = &mut fut => {
                break res;
            }
            // A reasoning delta or startup milestone hands the rolling window
            // to the renderer and latches `got_output` so no later tick can
            // repaint a loading frame over the feed. The same window is
            // retained for the steady-tick repaint below.
            window = rx.recv() => {
                if let Some(window) = window {
                    got_output = true;
                    last_window = window.clone();
                    renderer.paint(&window, Some(start.elapsed()));
                }
            }
            // Steady tick — two roles by mode:
            //  * Before the first line (genuinely silent backend, e.g. codex
            //    plain): the loading frame keeps the screen alive — spinner +
            //    elapsed, plus the silent/cold-start notice once past
            //    [`LOADING_GRACE`].
            //  * After the first line (startup milestones or reasoning are
            //    flowing): repaint the retained window with a fresh elapsed so
            //    the spinner keeps animating and the clock keeps rising while
            //    the model is silent between deltas — e.g. claude's post-init
            //    TTFT gap. The loading grace never trips in this mode, because
            //    startup milestones arrive within ~1 s and each resets
            //    `got_output`; the grace therefore measures only true
            //    no-output-at-all backends, not cold starts.
            _ = ticker.tick() => {
                let elapsed = start.elapsed();
                if !got_output {
                    let notice = if elapsed >= progress::LOADING_GRACE {
                        match &cold_start {
                            Some(program) => {
                                progress::LoadingNotice::ColdStart(program.clone())
                            }
                            None => progress::LoadingNotice::Silent,
                        }
                    } else {
                        progress::LoadingNotice::None
                    };
                    renderer.paint_loading(elapsed, notice);
                } else {
                    renderer.paint(&last_window, Some(elapsed));
                }
            }
        }
    };

    // Thinking is over: `finish` erases the reasoning/loading block (in-place
    // all along, so nothing ever hit the scrollback) and parks the cursor
    // below it. The renderer's Drop is a backstop if the stream aborted first.
    renderer.finish();
    result
}

async fn generate_and_commit(
    git: &Git,
    paths: &[String],
    display: &Display,
    prefix: &str,
    messenger: &CommitMessenger,
    confirm: &Confirm,
) -> anyhow::Result<()> {
    let files: Vec<serde_json::Value> = paths
        .iter()
        .map(|p| {
            let diff = git.diff(Some(p.as_str()))?;
            let scoped = diff::format_diff_scoped(&diff, p);
            Ok(serde_json::json!({ "path": p, "diff": scoped }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let diff = serde_json::json!({ "staged_files": files });
    let diff_str = diff.to_string();

    // Generate initial draft, then run confirmation loop if enabled.
    let result =
        progress::with_spinner("Generating commit message", messenger(diff_str.clone())).await?;
    let (message, body, preview_rows) = confirm_draft(
        (result.message, result.body),
        paths,
        display,
        confirm,
        messenger,
        diff_str,
    )
    .await?;

    // Erase the confirmed preview and commit.
    display.clear_last(preview_rows);
    let hash = git.commit(message.clone(), body.clone())?;
    display.commit_line(&hash, &message, body.as_deref(), prefix);
    Ok(())
}

/// Read a y/n answer from stdin. The label is written to stderr (Display is
/// stderr-only) so piped stdout stays clean. An empty answer (just Enter)
/// defaults to `true` (yes); `n` / `no` / any other input returns `false`.
fn prompt_yes_no(label: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    eprint!("{label} [y/n] ");
    std::io::stderr().flush()?;
    let mut input = String::new();
    std::io::stdin().read_line(&mut input)?;
    let trimmed = input.trim().to_lowercase();
    Ok(matches!(trimmed.as_str(), "" | "y" | "yes"))
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
    git: &Git,
    resolve: Resolver,
    prompt: Prompt,
    display: Display,
) -> anyhow::Result<()> {
    let conflict = git.conflict();
    let state = conflict.state()?;

    if !state.is_conflicted() {
        display.no_conflicts();
        return Ok(());
    }
    if !state.resolvable() {
        // rebase / am — detected but refused in v1.
        display.refused(state);
        anyhow::bail!("aic cannot resolve a {} state in v1", state.label());
    }

    let files = conflict.conflicted_files()?;
    if files.is_empty() {
        // Conflicted state but no unmerged index entries — the user resolved
        // every file by hand and only the finalize step remains.
        display.all_resolved_offer_finalize(state);
        if prompt("finalize now?")? {
            conflict.finalize(state)?;
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
        let original_bytes = conflict.read_worktree(&f.path)?;
        let original = String::from_utf8(original_bytes)
            .with_context(|| format!("{} is not valid UTF-8 (should be Content)", f.path))?;

        // Per-file resolution on the shared retry module (seam C, ADR-0005).
        // The op folds the resolver's two failure shapes onto `RetryReason`:
        // marker-laden output → `Markers` (retryable), and any underlying LLM
        // error → `Fatal` (propagates immediately, never retried). Clean
        // output is `Ok`. `RetryPolicy::once()` is the budget-1 / no-backoff
        // auto-retry. The spinner label still distinguishes the first attempt
        // from the retry so the live UX matches the old hand-written loop.
        let mut first_attempt = true;
        let path = f.path.clone();
        // Capture the resolver by reference so the `async move` block copies the
        // `&Resolver` (Copy) on each call instead of moving the owned `Box` —
        // otherwise the closure would be `FnOnce` and `retry` needs `FnMut`.
        let resolve_ref = &resolve;
        let op = || {
            let label = if std::mem::replace(&mut first_attempt, false) {
                format!("Resolving {path}")
            } else {
                format!("Retrying {path}")
            };
            // `.to_string()` (not `.clone()`) because `original` is borrowed by
            // the closure, so the bare name is a `&String` whose `.clone()` would
            // copy the reference rather than the content.
            let content = original.to_string();
            async move {
                match progress::with_spinner(&label, resolve_ref(content)).await {
                    Ok(resolved) if !conflict::has_conflict_markers(&resolved) => Ok(resolved),
                    Ok(_markers) => Err(retry::RetryReason::Markers),
                    Err(err) => Err(retry::RetryReason::Fatal(err)),
                }
            }
        };
        let resolved = match retry::retry(op, retry::RetryPolicy::once()).await {
            Ok(content) => content,
            // Budget spent with markers still present — the file can't be
            // resolved; soft-skip it for a re-run.
            Err(retry::RetryError::Exhausted(retry::RetryExhausted {
                last_reason: retry::RetryReason::Markers,
                ..
            })) => {
                display.skipped(&f.path, "markers remain after retry");
                skipped_failed += 1;
                continue;
            }
            // The LLM call errored (first attempt or retry) and propagated the
            // original error verbatim.
            Err(retry::RetryError::Fatal(err)) => {
                display.skipped(&f.path, &format!("LLM error: {err:#}"));
                skipped_failed += 1;
                continue;
            }
            // The op only ever yields Ok / Markers / Fatal, so an exhausted
            // budget with any other `last_reason` is unreachable here.
            Err(retry::RetryError::Exhausted(_)) => unreachable!(
                "resolve op only yields Ok / Markers / Fatal, never Empty or Truncated"
            ),
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
            conflict.write_worktree(path, resolved)?;
            git.add(&[path.as_str()])?;
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
        conflict.finalize(state)?;
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
    let git = Git::at(Path::new("."))?;
    run_resolve_workflow_impl(&git, resolver, prompt, Display::new()).await
}

/// Default `aic` run. `resolve`/`prompt`/`display` are seams mirroring
/// [`run_resolve_workflow_impl`]; they only matter on the conflicted-repo
/// auto-detect branch, which hands off to the resolve workflow. `confirm`
/// gates the opt-in pre-commit confirmation (issue #78): when enabled, every
/// drafted message is shown (message + body + file list) and its menu must
/// approve it (Commit) — or Re-generate / Edit it, or Abort — before the
/// commit lands.
pub(crate) async fn run_commit_workflow_impl(
    git: &Git,
    resolve: Resolver,
    prompt: Prompt,
    display: Display,
    planner: BatchPlanner,
    messenger: CommitMessenger,
    confirm: Confirm,
) -> anyhow::Result<()> {
    // Auto-detect a conflicted repo and offer `aic resolve` before the normal
    // stage+commit flow (ADR 0005). The commit guard in `Git::commit` is the
    // deeper net; this prompt is the friendly front door.
    let state = git.conflict().state()?;
    if state.is_conflicted() {
        display.resolve_prompt(state);
        if prompt("resolve now?")? {
            return run_resolve_workflow_impl(git, resolve, prompt, display).await;
        }
        anyhow::bail!(
            "aborted: repo is mid-{}; resolve conflicts first",
            state.label()
        );
    }

    let status = git.status()?;
    let staged_files: Vec<_> = status.iter().filter(|f| f.staged).collect();

    if staged_files.is_empty() {
        let unstaged_files: Vec<_> = status.iter().filter(|f| !f.staged).collect();
        if unstaged_files.is_empty() {
            // Nothing staged *and* nothing unstaged — no work for the LLM.
            display.nothing_to_commit();
            return Ok(());
        }

        // Capture each file's raw workdir-vs-HEAD diff once. This snapshot
        // feeds the two consumers that must agree on hunk numbering: the
        // numbered view sent to the model, and `file_hunk_counts` for
        // `validate_batch_plan`. Staging does NOT read from it — the `staging`
        // module re-reads a fresh diff per batch (so the Run survives
        // pre-commit hooks that re-stage whole files) and remaps the plan-time
        // indices onto the current diff via its internal `committed_hunks`.
        let mut file_hunk_counts: Vec<(String, usize)> = Vec::new();
        let files: Vec<serde_json::Value> = unstaged_files
            .iter()
            .map(|f| {
                let diff = git.diff_workdir(Some(f.path.as_str()))?;
                let hunk_count = diff::parse_file_patch(&diff).hunk_count();
                file_hunk_counts.push((f.path.clone(), hunk_count));
                let scoped = diff::format_diff_scoped(&diff, &f.path);
                Ok(serde_json::json!({ "path": f.path, "status": f.kind, "diff": scoped }))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let diff = serde_json::json!({ "unstaged_files": files });
        let result = planner(diff.to_string()).await?;

        generator::validate_batch_plan(&result, &file_hunk_counts)
            .context("batch plan validation failed")?;

        let count = result.batches.len();
        let mut staging = staging::Staging::new();
        for (i, batch) in result.batches.iter().enumerate() {
            let prefix = format!("[{}/{count}]", i + 1);
            // Stage this batch's hunks, then generate + commit. Either step
            // failing after earlier batches already committed leaves the repo
            // partially committed, so both share one abort message naming how
            // far we got and that the rest is recoverable by re-running `aic`.
            let outcome = async {
                let paths = staging.stage_batch(git, batch, &display)?;
                if paths.is_empty() {
                    // Every file in this batch already landed via an earlier
                    // batch or a pre-commit hook — nothing to commit.
                    return Ok(());
                }
                generate_and_commit(git, &paths, &display, &prefix, &messenger, &confirm).await
            };
            if let Err(e) = outcome.await {
                let batch_word = if i == 1 { "batch" } else { "batches" };
                // Declining the confirmation is a user choice, not an error:
                // report it as a clean abort naming how far the run got and
                // that the rest is recoverable — same shape as the single-
                // commit path's "no commit made". Other failures keep the
                // underlying cause in the message.
                if e.downcast_ref::<CommitDeclined>().is_some() {
                    anyhow::bail!(
                        "declined on batch {} of {} after {} {batch_word} committed.\n\
                         The remaining changes are still staged in the index.\n\
                         re-run `aic` to continue.",
                        i + 1,
                        count,
                        i
                    );
                }
                anyhow::bail!(
                    "aborted on batch {} of {} after {} {batch_word} committed.\n\
                     The remaining changes are still staged in the index.\n\
                     re-run `aic` to continue: {e:#}",
                    i + 1,
                    count,
                    i
                );
            }
        }
    } else {
        let paths: Vec<String> = staged_files.iter().map(|f| f.path.clone()).collect();
        let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
        git.add(&refs)?;
        match generate_and_commit(git, &paths, &display, "", &messenger, &confirm).await {
            Ok(()) => {}
            // Declining the confirmation is a user choice, not an error: report
            // it as a clean abort naming the outcome — nothing committed.
            Err(e) if e.downcast_ref::<CommitDeclined>().is_some() => {
                anyhow::bail!("aborted — no commit made");
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Production entry point for the default `aic` run — wires the real LLM
/// resolver, stdin y/n prompt, terminal confirmation menu, and message editor
/// into [`run_commit_workflow_impl`].
async fn run_commit_workflow() -> anyhow::Result<()> {
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
    let git = Git::at(Path::new("."))?;
    // Absent/malformed config keeps the default (no confirmation) — same
    // tolerance the provider-field resolution uses (config > default).
    let confirm_enabled = config::Config::load()
        .ok()
        .flatten()
        .map(|c| c.confirm_before_commit())
        .unwrap_or(false);
    // The confirmation menu renders on stderr but reads keys from stdin, so a
    // non-interactive stdin makes it unanswerable — and the failure would
    // otherwise surface mid-run, after the first batch is already staged.
    // Refuse to start rather than abort halfway.
    ensure_confirm_terminal(confirm_enabled, std::io::stdin().is_terminal())?;
    let confirm = if confirm_enabled {
        Confirm::interactive()
    } else {
        Confirm::Disabled
    };
    run_commit_workflow_impl(
        &git,
        resolver,
        prompt,
        Display::new(),
        planner,
        messenger,
        confirm,
    )
    .await
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    // One-time location migration (ADR 0012) — move a pre-0012 macOS config
    // from `~/Library/Application Support/aic/` to the fixed `~/.config/aic/`
    // location the docs have always claimed. Must run before preset migration
    // so the file lands at its new path first. Idempotent: copy old → new then
    // delete old; skip silently if the new file already exists; no-op when the
    // paths coincide or the old file is absent. A notice prints only when a
    // file is actually moved; a failure is logged, never blocks the run.
    match config::Config::migrate_location() {
        Ok(notices) => notices.iter().for_each(|n| eprintln!("aic: {n}")),
        Err(e) => eprintln!("aic: config location migration skipped: {e:#}"),
    }

    // Auto-migrate a stale CLI-agent config to the current preset shape before
    // any run that uses it — the fix for configs stranded on an older aic's
    // preset (e.g. claude before `stream-json`). Idempotent and conservative:
    // only configs byte-identical to a known legacy preset snapshot are
    // rewritten; a custom command is never touched. Notices print to stderr so
    // the file rewrite is transparent; a migration failure is logged but
    // never blocks the run (the user can still `aic setup` to refresh).
    match config::Config::migrate_if_stale() {
        Ok(notices) => notices.iter().for_each(|n| eprintln!("aic: {n}")),
        Err(e) => eprintln!("aic: config migration skipped: {e:#}"),
    }

    match cli.command {
        Some(Commands::Setup) => setup::run_setup(),
        Some(Commands::List) => config::run_list(),
        Some(Commands::Update) => update::run_update(),
        Some(Commands::Resolve) => run_resolve_workflow().await,
        Some(Commands::Completion) => {
            // Interactive when stdout is a terminal; fall back to $SHELL
            // detection for scripts and pipes.
            let shell = if std::io::stdout().is_terminal() {
                match completion::prompt_shell(completion::detect_shell())? {
                    Some(shell) => shell,
                    None => {
                        eprintln!("Cancelled.");
                        return Ok(());
                    }
                }
            } else {
                completion::detect_shell().ok_or_else(|| {
                    anyhow::anyhow!(
                        "couldn't detect your shell from $SHELL; run `aic completion` in a \
                         terminal to pick one (bash, zsh, fish, nushell)"
                    )
                })?
            };
            completion::install_completion(shell)
        }
        None => run_commit_workflow().await,
    }
}
