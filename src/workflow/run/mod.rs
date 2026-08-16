//! The Run module — home of the default `aic` commit workflow (CONTEXT.md
//! "Run"). Two entries:
//!
//! - [`default_run`] — the production front door: the conflicted-repo gate
//!   (detect → offer `aic resolve` → hand off to `crate::workflow::resolve::resolve_run`)
//!   and, on a clean repo, dispatch to [`commit_run`].
//! - [`commit_run`] — the Run spine proper: one commit over staged files, or
//!   a validated batch plan over unstaged files with concurrent pre-drafting
//!   (ADR 0014) and per-batch stage+commit.
//!
//! Seams (`planner`, `messenger`, `confirm`, `display`) are bundled in
//! [`RunDeps`]; `git` stays a separate parameter because it is the substrate
//! every phase reads, not a swappable seam. Production wiring lives in
//! [`default_workflow`]; e2e drives `commit_run`/`default_run` directly with
//! stubs.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::Path;

use anyhow::Context;

use crate::core::config;
use crate::core::types::{BatchPlanner, BoxFuture, CommitMessenger};
use crate::git::Git;
use crate::git::StatusKind;
use crate::git::diff;
use crate::git::diff_json;
use crate::git::staging;
use crate::llm::generator;
use crate::render::cursor;
use crate::render::display::Display;
use crate::render::progress;
use crate::render::reasoning_feed;
use crate::workflow::confirm::{CommitDeclined, Confirm, confirm_draft, ensure_confirm_terminal};
use crate::workflow::grouping;
use crate::workflow::input;
use crate::workflow::resolve::{ResolveDeps, resolve_run};

/// Cap on concurrent commit-message drafts during a multi-batch Run (ADR 0014).
/// Each batch's draft fans out after the plan; this bounds in-flight LLM requests
/// so a large split does not trip provider rate limits (HTTP 429). Batches at or
/// below this count draft fully in parallel.
// ponytail: fixed cap, not configurable — promote to a config knob only if a
// provider tier's rate limit bites before splits this large occur in practice.
const MAX_CONCURRENT_DRAFTS: usize = 8;

/// The commit Run's seam bundle: display, batch planner, commit messenger,
/// and the opt-in confirmation gate (issue #78). When confirmation is enabled,
/// every drafted message is shown (message + body + file list) and its menu
/// must approve it (Commit) — or Re-generate / Edit it, or Abort — before the
/// commit lands.
pub(crate) struct RunDeps {
    pub(crate) display: Display,
    pub(crate) planner: BatchPlanner,
    pub(crate) messenger: CommitMessenger,
    pub(crate) confirm: Confirm,
}

/// The Run spine: stage + commit what is staged, or plan + batch-commit what
/// is unstaged. Assumes a non-conflicted repo — [`default_run`] owns the
/// conflicted-repo gate.
pub(crate) async fn commit_run(git: &Git, deps: RunDeps) -> anyhow::Result<()> {
    let RunDeps {
        display,
        planner,
        messenger,
        confirm,
    } = deps;

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
        let mut raw_diffs: HashMap<String, String> = HashMap::new();
        let files: Vec<serde_json::Value> = unstaged_files
            .iter()
            .map(|f| {
                let diff = git.diff_workdir(Some(f.path.as_str()))?;
                raw_diffs.insert(f.path.clone(), diff.clone());
                let hunk_count = diff::parse_file_patch(&diff).hunk_count();
                file_hunk_counts.push((f.path.clone(), hunk_count));
                // A changed file with no textual hunks (binary/mode/rename)
                // would yield an empty scoped diff — the model reads that as
                // "nothing changed" and drops the file. Send an explicit marker
                // instead so it includes the file with an empty hunks array.
                let scoped = if hunk_count == 0 && !diff.trim().is_empty() {
                    crate::llm::prompt::BINARY_MARKER.to_string()
                } else {
                    diff::format_diff_scoped(&diff, &f.path)
                };
                Ok(serde_json::json!({ "path": f.path, "status": f.kind, "diff": scoped }))
            })
            .collect::<anyhow::Result<Vec<_>>>()?;
        let diff = serde_json::json!({ "unstaged_files": files });
        let result = planner(diff.to_string()).await?;

        // An invalid plan is an LLM malfunction, not a user problem: warn and
        // regroup deterministically instead of failing the Run. The engine's
        // output is a valid partition by construction (every hunk of real
        // work lands in exactly one batch), so issue #34's silent no-op
        // cannot recur — real work always yields ≥1 batch. The re-validation
        // is defensive only.
        let result = match generator::validate_batch_plan(&result, &file_hunk_counts) {
            Ok(()) => result,
            Err(plan_err) => {
                display.warn(&format!(
                    "LLM batch plan invalid ({plan_err}); regrouping deterministically"
                ));
                let diffs: Vec<(String, String)> = unstaged_files
                    .iter()
                    .map(|f| Ok((f.path.clone(), raw_diffs[&f.path].clone())))
                    .collect::<anyhow::Result<_>>()?;
                let plan = grouping::plan_from_diffs(&diffs);
                generator::validate_batch_plan(&plan, &file_hunk_counts)
                    .context("deterministic fallback plan failed validation")?;
                plan
            }
        };

        let count = result.batches.len();
        // Pre-draft every batch's message concurrently (ADR 0014): the LLM
        // round-trips dominate a multi-batch Run's wall-clock, and each batch's
        // diff content is fully known at plan time, so fanning the drafts out
        // collapses N sequential waits into one. Each draft is sliced from the
        // plan-time workdir diff (the numbering the model saw) rather than the
        // staged diff; the two diverge only under a pre-commit hook that
        // rewrites bytes — irrelevant to a commit message — and the
        // confirmation Re-generate action still redrafts against the staged
        // diff. Each concurrent draft runs behind its own `[i/N]` bar on one
        // shared MultiProgress (N standalone spinners collide on one line —
        // only one clears); the messenger is a bare LLM call, so the bars are
        // the only spinners in this phase. Order-preserving `buffered` keeps
        // the messenger's call order (and the test messengers' per-call
        // counters) deterministic.
        let batch_diffs: Vec<String> = result
            .batches
            .iter()
            .map(|b| diff_json::plan_batch_diff_json(b, &raw_diffs))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let futs: Vec<BoxFuture<anyhow::Result<generator::CommitOutput>>> =
            batch_diffs.into_iter().map(&messenger).collect();
        let drafts: Vec<anyhow::Result<generator::CommitOutput>> = progress::with_indexed_spinners(
            "Generating commit message",
            MAX_CONCURRENT_DRAFTS,
            futs,
        )
        .await?;

        let mut staging = staging::Staging::new();
        for (i, (batch, draft)) in result.batches.iter().zip(drafts).enumerate() {
            let prefix = format!("[{}/{}]", i + 1, count);
            // Stage this batch's hunks, then commit its pre-drafted message.
            // Either step failing after earlier batches already committed
            // leaves the repo partially committed, so both share one abort
            // message naming how far we got and that the rest is recoverable by
            // re-running `aic`. The draft was produced up front; surfacing its
            // error after staging (not before) keeps the "remaining changes
            // are still staged" contract identical to drafting inline.
            let outcome = async {
                let paths = staging.stage_batch(git, batch, &display)?;
                if paths.is_empty() {
                    // Every file in this batch already landed via an earlier
                    // batch or a pre-commit hook — nothing to commit.
                    return Ok(());
                }
                let draft = draft?;
                generate_and_commit(git, &paths, &display, &prefix, draft, &messenger, &confirm)
                    .await
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
        // Re-stage folds a staged file's workdir changes into the index
        // (`git add` semantics) — but a staged deletion has nothing to fold:
        // its path is neither on disk nor in the index, so `Git::add`'s
        // pathspec guard (built for LLM-plan paths) bails on it. Skip
        // deleted entries; they are already staged and stay in `paths` for
        // the diff, stats, and commit below.
        let re_stage_paths: Vec<&str> = staged_files
            .iter()
            .filter(|f| f.kind != StatusKind::Deleted)
            .map(|f| f.path.as_str())
            .collect();
        // Guard is load-bearing: `Git::add(&[])` falls through to
        // `add_all(["*"])`, which would stage every untracked file — an
        // all-deletions staged set must skip the call instead.
        if !re_stage_paths.is_empty() {
            git.add(&re_stage_paths)?;
        }
        let diff_str = staged_diff_json(git, &paths)?;
        let draft =
            progress::with_spinner("Generating commit message", messenger(diff_str.clone()))
                .await?;
        match generate_and_commit(git, &paths, &display, "", draft, &messenger, &confirm).await {
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

async fn generate_and_commit(
    git: &Git,
    paths: &[String],
    display: &Display,
    prefix: &str,
    draft: generator::CommitOutput,
    messenger: &CommitMessenger,
    confirm: &Confirm,
) -> anyhow::Result<()> {
    // The first draft was produced up front by the caller (the unstaged
    // multi-batch path drafts all batches concurrently — ADR 0014 — and the
    // single-commit path drafts inline before calling this). What remains is
    // the confirmation loop — whose Re-generate action redrafts against the
    // staged diff — then the commit. Staged stats (what the commit would land)
    // feed the preview footer — shown only when confirmation is on; landed
    // stats (what it did land) always feed the ✓ line.
    let diff_str = staged_diff_json(git, paths)?;
    let stats = git.staged_stats(paths)?;
    let (message, body, preview_rows) = confirm_draft(
        (draft.message, draft.body),
        &stats,
        display,
        confirm,
        messenger,
        diff_str,
    )
    .await?;

    // Erase the confirmed preview and commit.
    display.clear_last(preview_rows);
    let hash = git.commit(message.clone(), body.clone())?;
    let landed = git.committed_stats(paths)?;
    display.commit_line(&hash, &message, body.as_deref(), prefix, &landed);
    Ok(())
}

/// The staged diff for one batch's files (read live from the index via
/// `git.diff`) as the commit-message JSON. Feeds the first draft and the
/// confirmation Re-generate action, which must redraft against what landed.
fn staged_diff_json(git: &Git, paths: &[String]) -> anyhow::Result<String> {
    let pairs = paths
        .iter()
        .map(|p| {
            let diff = git.diff(Some(p.as_str()))?;
            Ok((p.clone(), diff::format_diff_scoped(&diff, p)))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    Ok(diff_json::files_json(pairs))
}

/// The default command's front door: gate on a conflicted repo, then dispatch.
/// On a resolvable conflict it offers `aic resolve` (`resolve.prompt`), handing
/// the whole [`ResolveDeps`] to [`resolve_run`] on acceptance; the gate notice
/// renders on the commit display. On decline it aborts — the commit guard in
/// `Git::commit` is the deeper net; this prompt is the friendly front door
/// (ADR 0005). rebase/am states are never offered (resolve refuses them,
/// ADR 0005) — the abort names the manual continuation instead, so the advice
/// never points at a command that would refuse.
pub(crate) async fn default_run(
    git: &Git,
    resolve: ResolveDeps,
    commit: RunDeps,
) -> anyhow::Result<()> {
    let state = git.conflict().state()?;
    if state.is_conflicted() {
        if let Some(args) = state.manual_finalize_command() {
            anyhow::bail!(
                "aborted — repo is mid-{}. finish it with `git {}`, then re-run `aic`",
                state.label(),
                args.join(" ")
            );
        }
        crate::git::conflict::resolve_prompt(&commit.display, state);
        if (resolve.prompt)("resolve now?")? {
            return resolve_run(git, resolve).await;
        }
        anyhow::bail!(
            "aborted — repo is mid-{}. run `aic resolve` when ready, then re-run `aic`",
            state.label()
        );
    }
    commit_run(git, commit).await
}

/// Run an LLM call behind the live reasoning feed: probe the
/// terminal geometry, build the real [`progress::ReasoningRenderer`] sink, and
/// hand the reasoning tap to `make_call` so the caller's streaming generator
/// forwards its thinking deltas into the feed. The production wiring for the
/// batch-planner path (the commit-message path owns its spinner per call site).
async fn run_with_reasoning_feed<F, T>(
    label: &'static str,
    cold_start: Option<String>,
    make_call: F,
) -> anyhow::Result<T>
where
    F: FnOnce(reasoning_feed::ReasoningTap) -> BoxFuture<anyhow::Result<T>>,
    T: Send,
{
    // The DSR cursor-row query does up to ~200 ms of blocking tty I/O (poll
    // + raw-mode byte reads against a deadline). Run it on the blocking pool
    // so it stalls a worker, not the async reactor. A task panic degrades to
    // the no-scroll [`cursor::WindowSizing::fallback`] — decoration must
    // never break the commit.
    let sizing = tokio::task::spawn_blocking(cursor::reasoning_window_rows)
        .await
        .unwrap_or_else(|_| cursor::WindowSizing::fallback());
    let mut sink = progress::reasoning_sink(label, sizing.max_rows, sizing.cursor_row);
    reasoning_feed::run(&mut sink, sizing.max_rows, cold_start.as_deref(), make_call).await
}

/// Production entry point for the default `aic` run — wires the real LLM
/// resolver, stdin y/n prompt, terminal confirmation menu, and message editor
/// into [`default_run`].
pub async fn default_workflow() -> anyhow::Result<()> {
    let resolve = ResolveDeps {
        resolve: Box::new(|content: String| -> BoxFuture<anyhow::Result<String>> {
            Box::pin(async move { generator::Generator::resolve_conflict(&content).await })
        }),
        prompt: Box::new(input::prompt_yes_no),
        display: Display::new(),
    };
    // The planner's streaming-capable backend that has not yet produced
    // reasoning is in a cold start, and past the loading grace its loading
    // frame says so. `None` on any config-read glitch — never falsely claim a
    // streaming capability.
    let planner_cold = crate::llm::LlmConfig::load()
        .ok()
        .and_then(|c| c.cold_start_program());
    let planner: BatchPlanner = Box::new(
        move |diff: String| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
            let cold_start = planner_cold.clone();
            Box::pin(run_with_reasoning_feed(
                "Analyzing changes",
                cold_start,
                move |tap| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
                    Box::pin(async move {
                        generator::Generator::split_patch_streaming(&diff, tap).await
                    })
                },
            ))
        },
    );
    let messenger: CommitMessenger = Box::new(
        move |diff: String| -> BoxFuture<anyhow::Result<generator::CommitOutput>> {
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
    default_run(
        &git,
        resolve,
        RunDeps {
            display: Display::new(),
            planner,
            messenger,
            confirm,
        },
    )
    .await
}
