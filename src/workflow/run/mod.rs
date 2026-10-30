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
//! Seams (`planner`, `messenger`, `confirm`, `review`, `display`) are bundled
//! [`RunDeps`]; `git` stays a separate parameter because it is the substrate
//! every phase reads, not a swappable seam. Production wiring lives in
//! [`default_workflow`]; e2e drives `commit_run`/`default_run` directly with
//! stubs.

use std::collections::HashMap;
use std::io::IsTerminal;
use std::path::Path;
use std::time::Instant;

use anyhow::Context;
use futures::stream::{self, StreamExt};

use crate::core::config;
use crate::core::types::{BatchPlanner, BoxFuture, CommitMessenger};
use crate::git::Git;
use crate::git::diff;
use crate::git::diff_json;
use crate::git::staging;
use crate::llm::generator;
use crate::llm::redact::{self, Redact};
use crate::render::display::CountsGeometry;
use crate::render::display::Display;
use crate::render::display::counts_geometry;
use crate::render::timeline::driver::{self, RunFrame};
use crate::render::timeline::{Failure, StepIdx, StepIx, SubBody, delta_suffix, plural};
use crate::workflow::confirm::{CommitDeclined, Confirm, confirm_draft};
use crate::workflow::grouping;
use crate::workflow::input;
use crate::workflow::resolve::{ResolveDeps, resolve_run};
use crate::workflow::review::{BatchReview, review_plan};
use crate::workflow::undo;

mod reasoning;

use reasoning::run_with_reasoning_feed;

/// Cap on concurrent commit-message drafts during a multi-batch Run (ADR 0014).
/// Each batch's draft fans out after the plan; this bounds in-flight LLM requests
/// so a large split does not trip provider rate limits (HTTP 429). Batches at or
/// below this count draft fully in parallel.
// ponytail: fixed cap, not configurable — promote to a config knob only if a
// provider tier's rate limit bites before splits this large occur in practice.
const MAX_CONCURRENT_DRAFTS: usize = 8;

/// The commit Run's seam bundle: display, batch planner, commit messenger,
/// the opt-in confirmation gate (issue #78), the opt-in Batch Review gate
/// (issue #157), and the Redaction Gate policy (issue #155, ADR 0017).
/// When confirmation is enabled, every drafted message is shown (message +
/// body + file list) and its menu must approve it (Commit) — or
/// Re-generate / Edit it, or Abort — before the commit lands. When review
/// is enabled, the validated plan is shown first and its Batches can be
/// merged before any drafting starts. `redact` mirrors `confirm`/`review`
/// as run policy carried with the seams: `Off` is the `--no-redact`
/// one-off override.
pub(crate) struct RunDeps {
    pub(crate) display: Display,
    pub(crate) planner: BatchPlanner,
    pub(crate) messenger: CommitMessenger,
    pub(crate) confirm: Confirm,
    pub(crate) review: BatchReview,
    pub(crate) redact: Redact,
}

/// The Run spine: stage + commit what is staged, or plan + batch-commit what
/// is unstaged. Assumes a non-conflicted repo — [`default_run`] owns the
/// conflicted-repo gate. `deps.redact` = [`Redact::Off`] skips the secrets
/// Redaction Gate (issue #155, ADR 0017) — the `--no-redact` one-off override.
///
/// Both paths paint the Run timeline (ADR 0018): the step skeleton up front
/// (config-conditional steps included exactly when they will run), the
/// whole frame repainting on a terminal, the plain ledger appending off
/// one. The frame starts only after the status check confirms there is
/// work — `nothing to commit` still renders alone, with no timeline.
pub(crate) async fn commit_run(git: &Git, deps: RunDeps) -> anyhow::Result<()> {
    let status = git.status()?;
    // The Run-start HEAD snapshot `aic undo` resets back to — captured before
    // any planning or staging, and recorded just before the Run's first
    // commit lands (see `generate_and_commit`). `None` on an unborn repo
    // (no HEAD commit yet): there is nothing earlier to reset back to, so
    // the Run simply isn't undoable.
    let undo_start = git.head_sha().ok();
    let staged_files: Vec<_> = status.iter().filter(|f| f.staged).collect();

    if staged_files.is_empty() {
        let unstaged_files: Vec<_> = status.iter().filter(|f| !f.staged).collect();
        if unstaged_files.is_empty() {
            // Nothing staged *and* nothing unstaged — no work for the LLM.
            deps.display.nothing_to_commit();
            return Ok(());
        }
        unstaged_run(git, &deps, undo_start, &unstaged_files).await
    } else {
        staged_run(git, &deps, undo_start, &staged_files).await
    }
}

/// The plan-time workdir snapshot: one Scan-phase traversal capturing
/// each unstaged file's raw workdir-vs-HEAD diff plus every derived
/// view. It exists so the consumers that must agree on hunk numbering —
/// the numbered view sent to the model, the counts behind
/// `validate_batch_plan`, and each Batch's sliced draft input — all
/// read from one capture. Staging does NOT read from it: the `staging`
/// module re-reads a fresh diff per batch (so the Run survives
/// pre-commit hooks that re-stage whole files) and remaps the plan-time
/// indices onto the current diff via its internal `committed_hunks`.
pub(crate) struct WorkdirSnapshot {
    /// Raw per-file diffs keyed by path — the slice source behind
    /// [`Self::batch_diff`].
    raw_diffs: HashMap<String, String>,
    /// Per-file hunk counts in scan order — the numbering the model saw.
    hunk_counts: Vec<(String, usize)>,
    /// The planner's envelope (`{"unstaged_files":[…]}`), built once
    /// during the scan.
    payload: String,
    /// `(path, raw diff)` pairs in scan order — the Redaction Gate's
    /// input and the deterministic fallback's regrouping source, so
    /// both consume exactly what the model sees.
    raw_pairs: Vec<(String, String)>,
    /// Scan-order paths (the stats query's input).
    paths: Vec<String>,
}

impl WorkdirSnapshot {
    /// Capture the workdir diff of every file once. A changed file with
    /// no textual hunks (binary/mode/rename) would yield an empty
    /// scoped diff — the model reads that as "nothing changed" and
    /// drops the file — so its envelope entry carries the explicit
    /// binary marker instead, keeping the file plannable with an empty
    /// hunks array.
    pub(crate) fn scan(git: &Git, files: &[&crate::git::FileStatus]) -> anyhow::Result<Self> {
        let mut raw_diffs = HashMap::new();
        let mut hunk_counts = Vec::new();
        let mut payload_files = Vec::new();
        for f in files {
            let diff = git.diff_workdir(Some(f.path.as_str()))?;
            raw_diffs.insert(f.path.clone(), diff.clone());
            let patch = diff::parse_file_patch(&diff);
            hunk_counts.push((f.path.clone(), patch.hunk_count()));
            let scoped = if patch.is_non_textual(&diff) {
                crate::llm::prompt::BINARY_MARKER.to_string()
            } else {
                diff::format_diff_scoped(&diff, &f.path)
            };
            payload_files.push(serde_json::json!({
                "path": f.path,
                "status": f.kind,
                "diff": scoped
            }));
        }
        let raw_pairs = files
            .iter()
            .map(|f| (f.path.clone(), raw_diffs[&f.path].clone()))
            .collect();
        let paths = files.iter().map(|f| f.path.clone()).collect();
        Ok(Self {
            payload: serde_json::json!({ "unstaged_files": payload_files }).to_string(),
            raw_diffs,
            hunk_counts,
            raw_pairs,
            paths,
        })
    }

    /// The planner's envelope over the snapshot's numbered views.
    pub(crate) fn planner_payload(&self) -> &str {
        &self.payload
    }

    /// Per-file hunk counts — the plan's index space. This is the only
    /// numbering `validate_batch_plan` and the Review gate ever see.
    pub(crate) fn hunk_counts(&self) -> &[(String, usize)] {
        &self.hunk_counts
    }

    /// `(path, raw diff)` pairs in scan order.
    pub(crate) fn raw_pairs(&self) -> &[(String, String)] {
        &self.raw_pairs
    }

    /// Scan-order paths.
    pub(crate) fn paths(&self) -> &[String] {
        &self.paths
    }

    /// One Batch's Drafted-Message input: its planned hunks sliced out
    /// of the plan-time diffs — the numbering the model saw and the
    /// plan refers to (ADR 0014).
    pub(crate) fn batch_diff(&self, batch: &generator::BatchPlanBatch) -> anyhow::Result<String> {
        diff_json::plan_batch_diff_json(batch, &self.raw_diffs)
    }

    /// The deterministic regrouping of the raw diffs (the fallback plan).
    pub(crate) fn fallback_plan(&self) -> generator::BatchPlanOutput {
        grouping::plan_from_diffs(&self.raw_pairs)
    }

    /// Counts-only construction for tests that drive phases without a
    /// repo (the Review gate's unit tests); phases that need diffs go
    /// through [`Self::scan`].
    #[cfg(test)]
    pub(crate) fn from_hunk_counts(counts: Vec<(String, usize)>) -> Self {
        Self {
            raw_diffs: HashMap::new(),
            hunk_counts: counts,
            payload: String::new(),
            raw_pairs: Vec::new(),
            paths: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests;

/// The unstaged branch of the Run: scan the workdir, plan Batches (with the
/// deterministic fallback), review, pre-draft every Batch concurrently
/// (ADR 0014), then stage + commit Batch by Batch. The spine owns the step
/// skeleton and threads the phase results; each phase below is a named fn.
async fn unstaged_run(
    git: &Git,
    deps: &RunDeps,
    undo_start: Option<String>,
    unstaged_files: &[&crate::git::FileStatus],
) -> anyhow::Result<()> {
    let RunDeps {
        display,
        planner,
        messenger,
        review,
        redact,
        ..
    } = deps;

    // Step skeleton (ADR 0018): Scan, the Redaction Gate (iff `--no-redact`
    // did not turn it off), Plan, Review (iff configured), Draft, Commit.
    let redact_on = *redact == Redact::On;
    let review_on = matches!(review, BatchReview::Interactive { .. });
    let mut labels: Vec<String> = vec!["Scan changes".into()];
    if redact_on {
        labels.push("Redaction gate".into());
    }
    labels.push("Plan batches".into());
    if review_on {
        labels.push("Review plan".into());
    }
    labels.push("Draft messages".into());
    labels.push("Commit batches".into());
    let frame = RunFrame::new(labels).await;
    let _frame_guard = driver::install(frame.clone());
    const SCAN: StepIdx = StepIdx::FIRST;
    let mut ix = StepIx::AFTER_FIRST;
    let redact_step = ix.take_if(redact_on);
    let plan_step = ix.take();
    let review_step = ix.take_if(review_on);
    let draft_step = ix.take();
    let commit_step = ix.take();

    frame.activate(SCAN);
    let snapshot = frame.fail_on_err(display, SCAN, WorkdirSnapshot::scan(git, unstaged_files))?;
    let scan_stats = scanned(
        &frame,
        display,
        SCAN,
        git.workdir_stats(snapshot.paths()),
        unstaged_files.len(),
    )?;
    // Run-wide counts-grid geometry: every Commit entry footer of this
    // Run floors its `+N`/`−M` columns with it — computed over the
    // WHOLE unstaged diff up front — so per-batch digit counts can't
    // drift the flushed table's columns.
    let counts_floor = counts_geometry(&scan_stats);

    // Redaction Gate, first payload site (issue #155, ADR 0017): the
    // snapshot's raw per-file diffs feed every LLM payload of this
    // branch — the plan envelope above and each batch's Drafted Message
    // input (sliced from the snapshot) — so gating here once refuses
    // before any Backend call. The confirmation menu's Re-generate
    // redraft stays ungated by design: it re-sends content this gate
    // already cleared. A refusal is the step's failed screen: the
    // headline names the gate, the detail row carries the full files +
    // patterns refusal (ADR 0018).
    super::redact_gate(&frame, display, redact_step, redact::Override::Run, |_| {
        Ok(snapshot.raw_pairs())
    })?;

    frame.activate(plan_step);
    let planned = frame.fail_on_err(
        display,
        plan_step,
        planner(snapshot.planner_payload().to_string()).await,
    )?;
    let mut result = plan_with_fallback(&frame, display, plan_step, planned, &snapshot)?;

    // Batch Review (issue #157): the opt-in pre-draft gate — applies to
    // whichever plan proceeds (the LLM's or the deterministic
    // fallback's) and merges Batches before any Drafted Message is paid
    // for. Abort here is the cheapest exit: nothing staged, nothing
    // drafted.
    if let Some(rs) = review_step {
        result = review_plan(result, &snapshot, display, review, &frame, rs)?;
    }
    let drafts = draft_all(&frame, display, draft_step, messenger, &result, &snapshot).await?;

    frame.activate(commit_step);
    commit_batches(
        git,
        deps,
        &frame,
        commit_step,
        PlannedCommits {
            plan: result,
            drafts,
            undo_start,
            counts_floor,
        },
    )
    .await
}

/// The Plan phase: validate the planner's output against the scan's hunk
/// counts and hand back the plan to proceed with — the LLM's, or the
/// deterministic regrouping when the plan is invalid.
///
/// An invalid plan is an LLM malfunction, not a user problem: warn and
/// regroup deterministically instead of failing the Run. The engine's
/// output is a valid partition by construction (every hunk of real
/// work lands in exactly one batch), so issue #34's silent no-op
/// cannot recur — real work always yields ≥1 batch. The re-validation
/// is defensive only. The summary reads "Grouped" on this fallback
/// path (ADR 0018) and the caution row rides right under it.
fn plan_with_fallback(
    frame: &RunFrame,
    display: &Display,
    plan_step: StepIdx,
    planned: generator::BatchPlanOutput,
    snapshot: &WorkdirSnapshot,
) -> anyhow::Result<generator::BatchPlanOutput> {
    let mut fallback_err: Option<String> = None;
    let result = match generator::validate_batch_plan(&planned, snapshot.hunk_counts()) {
        Ok(()) => planned,
        Err(plan_err) => {
            fallback_err = Some(plan_err.to_string());
            let plan = snapshot.fallback_plan();
            generator::validate_batch_plan(&plan, snapshot.hunk_counts())
                .context("deterministic fallback plan failed validation")?;
            plan
        }
    };
    let count = result.batches.len();
    // The caution row must ride the step's live rows BEFORE completion:
    // under chronological collapse (ADR 0018 amendment 3) `complete`
    // peels the finished block to scrollback at once, so a row emitted
    // after it would land on a step the frame no longer renders.
    if let Some(err) = &fallback_err {
        // Outcome first: the row clamps at the text budget, so the
        // actionable half ("Regrouping deterministically") must lead —
        // the e2e suite widens to 120 cols so its causes survive the
        // clamp whole (at the default 80 the cause text is cut).
        frame.step_display(display).warn(&format!(
            "Regrouping deterministically — invalid batch plan ({err})"
        ));
    }
    // One verb for the step in every state: the skeleton promises
    // "Plan batches", so the summary keeps "Planned" even on the
    // deterministic fallback — the ⚠ row above already tells that story.
    frame.complete(
        display,
        plan_step,
        format!("Planned {}", plural(count, "batch")),
    );
    Ok(result)
}

/// The Draft phase: slice each Batch's diff from the plan-time snapshot
/// and pre-draft every message concurrently (ADR 0014): the LLM
/// round-trips dominate a multi-batch Run's wall-clock, and each batch's
/// diff content is fully known at plan time, so fanning the drafts out
/// collapses N sequential waits into one. Each draft is sliced from the
/// plan-time workdir diff (the numbering the model saw) rather than the
/// staged diff; the two diverge only under a pre-commit hook that
/// rewrites bytes — irrelevant to a commit message — and the
/// confirmation Re-generate action still redrafts against the staged
/// diff. Each concurrent draft reveals its own sub-row as it
/// starts and flips it to a `✓` on finish (ADR 0018); all sub-rows
/// erase together at step completion. Order-preserving `buffered`
/// keeps the messenger's call order (and the test messengers'
/// per-call counters) deterministic. The fixed MAX_CONCURRENT_DRAFTS
/// is the committed per-batch concurrency cap of this program.
async fn draft_all(
    frame: &RunFrame,
    display: &Display,
    draft_step: StepIdx,
    messenger: &CommitMessenger,
    plan: &generator::BatchPlanOutput,
    snapshot: &WorkdirSnapshot,
) -> anyhow::Result<Vec<anyhow::Result<generator::CommitOutput>>> {
    let count = plan.batches.len();
    let batch_diffs = frame.fail_on_err(
        display,
        draft_step,
        plan.batches
            .iter()
            .map(|b| snapshot.batch_diff(b))
            .collect::<anyhow::Result<Vec<_>>>(),
    )?;
    frame.activate(draft_step);
    // Each draft's sub-row carries its batch's lead file (plus a
    // "+N More" count) — an anonymous `Generating commit message` column
    // gives the user no way to map an outcome to a batch (ADR 0018).
    let draft_labels: Vec<String> = plan
        .batches
        .iter()
        .map(|b| files_label(&b.changes))
        .collect();
    let drafts: Vec<anyhow::Result<generator::CommitOutput>> = {
        let futs = batch_diffs.into_iter().enumerate().map(|(i, batch_diff)| {
            let frame = frame.clone();
            let messenger = &messenger;
            let label = draft_labels[i].clone();
            async move {
                frame.set_sub_row(
                    draft_step,
                    i + 1,
                    &SubBody::InFlight(format!("Drafting {label}")),
                );
                let started = Instant::now();
                let outcome = messenger(batch_diff).await;
                frame.set_sub_row(
                    draft_step,
                    i + 1,
                    &SubBody::Done(label, Some(started.elapsed())),
                );
                outcome
            }
        });
        stream::iter(futs)
            .buffered(MAX_CONCURRENT_DRAFTS)
            .collect()
            .await
    };
    frame.complete(
        display,
        draft_step,
        format!("Drafted {}", plural(count, "message")),
    );
    Ok(drafts)
}

/// The Commit phase's inputs: the validated plan, its pre-drafted messages
/// (parallel to `plan.batches`), and the two Run-wide values every entry
/// needs. Bundled so [`commit_batches`] stays under clippy's argument
/// budget (same precedent as [`DraftedCommit`]).
struct PlannedCommits {
    plan: generator::BatchPlanOutput,
    drafts: Vec<anyhow::Result<generator::CommitOutput>>,
    undo_start: Option<String>,
    counts_floor: CountsGeometry,
}

/// The Commit phase: stage each Batch's hunks and commit its pre-drafted
/// message, Batch by Batch.
async fn commit_batches(
    git: &Git,
    deps: &RunDeps,
    frame: &RunFrame,
    commit_step: StepIdx,
    planned: PlannedCommits,
) -> anyhow::Result<()> {
    let PlannedCommits {
        plan,
        drafts,
        undo_start,
        counts_floor,
    } = planned;
    let count = plan.batches.len();
    let step_display = frame.step_display(&deps.display);
    let mut staging = staging::Staging::new();
    for (i, (batch, draft)) in plan.batches.iter().zip(drafts).enumerate() {
        frame.set_label(
            commit_step,
            // Keep the skeleton's verb — the pending row promised
            // "Commit batches", and `· i/N` carries the position.
            format!("Commit batches · {}/{}", i + 1, count),
        );
        // Stage this batch's hunks, then commit its pre-drafted message.
        // Either step failing after earlier batches already committed
        // leaves the repo partially committed, so both share one abort
        // message naming how far we got and that the rest is recoverable by
        // re-running `aic`. The draft was produced up front; surfacing its
        // error after staging (not before) keeps the "remaining changes
        // are still staged" contract identical to drafting inline.
        let outcome = async {
            let paths = staging.stage_batch(git, &batch.files(), &step_display)?;
            if paths.is_empty() {
                // Every file in this batch already landed via an earlier
                // batch or a pre-commit hook — nothing to commit.
                return Ok(());
            }
            let draft = draft?;
            generate_and_commit(
                git,
                &paths,
                &step_display,
                frame,
                DraftedCommit {
                    undo_start: undo_start.clone(),
                    sub: (count > 1).then_some(i + 1),
                    draft,
                    counts_floor,
                },
                &deps.messenger,
                &deps.confirm,
            )
            .await
        };
        if let Err(e) = outcome.await {
            let batch_word = if i == 1 { "batch" } else { "batches" };
            let remediation = [
                format!(
                    "{i} {batch_word} committed; the remaining changes are still staged in the index"
                ),
                "re-run `aic` to continue".to_string(),
            ];
            // Declining the confirmation is a user choice, not an error:
            // report it as a clean abort naming how far the run got and
            // that the rest is recoverable — same shape as the single-
            // commit path's "no commit made". Other failures keep the
            // underlying cause in the message. Either way the step's
            // failed screen names the batch and the remediation rows
            // carry the recovery contract (ADR 0018).
            if e.downcast_ref::<CommitDeclined>().is_some() {
                frame.fail(
                    &deps.display,
                    commit_step,
                    Failure {
                        // The step label already carries `· i/N`; the
                        // error chain below keeps the full position.
                        reason: "declined".to_string(),
                        detail: remediation.to_vec(),
                    },
                );
                anyhow::bail!(
                    "declined on batch {} of {} after {} {batch_word} committed.\n\
                     The remaining changes are still staged in the index.\n\
                     re-run `aic` to continue.",
                    i + 1,
                    count,
                    i
                );
            }
            // Headline + remediation only: the error chain prints once,
            // in full, after the frame ends (main's boundary report) —
            // an in-frame copy is width-capped to one row and would only
            // duplicate it.
            frame.fail(
                &deps.display,
                commit_step,
                Failure {
                    reason: format!("failed on batch {} of {count}", i + 1),
                    detail: remediation.to_vec(),
                },
            );
            anyhow::bail!(
                "aborted on batch {} of {} after {} {batch_word} committed: {e:#}",
                i + 1,
                count,
                i
            );
        }
    }
    frame.complete(
        &deps.display,
        commit_step,
        format!("Committed {}", plural(count, "batch")),
    );
    Ok(())
}

/// The staged branch of the Run: the user did the partitioning — no plan,
/// no review; draft one message over the index and commit it as staged.
async fn staged_run(
    git: &Git,
    deps: &RunDeps,
    undo_start: Option<String>,
    staged_files: &[&crate::git::FileStatus],
) -> anyhow::Result<()> {
    let display = &deps.display;
    let paths: Vec<String> = staged_files.iter().map(|f| f.path.clone()).collect();
    // Step skeleton (ADR 0018): the staged branch's three steps (four
    // with the gate) — no plan or review; the user did the partitioning.
    let redact_on = deps.redact == Redact::On;
    let mut labels: Vec<String> = vec!["Scan changes".into()];
    if redact_on {
        labels.push("Redaction gate".into());
    }
    labels.push("Draft message".into());
    labels.push("Commit".into());
    let frame = RunFrame::new(labels).await;
    let _frame_guard = driver::install(frame.clone());
    const SCAN: StepIdx = StepIdx::FIRST;
    let mut ix = StepIx::AFTER_FIRST;
    let redact_step = ix.take_if(redact_on);
    let draft_step = ix.take();
    let commit_step = ix.take();

    frame.activate(SCAN);
    let staged_paths: Vec<String> = staged_files.iter().map(|f| f.path.clone()).collect();
    let scan_stats = scanned(
        &frame,
        display,
        SCAN,
        git.staged_stats(&staged_paths),
        staged_files.len(),
    )?;
    // Run-wide counts-grid geometry (see the unstaged branch): the
    // staged branch lands one entry, so the floor equals its own
    // geometry — kept for symmetry with the shared `commit_line`
    // contract.
    let counts_floor = counts_geometry(&scan_stats);

    // Redaction Gate, second payload site (issue #155, ADR 0017): the
    // staged branch's one LLM call sends `staged_diff_json`'s content.
    // Gate reads the same per-file diffs itself rather than scanning
    // inside `staged_diff_json` — that fn is shared with the confirm
    // menu's Re-generate redraft, which stays ungated by design (the
    // cheap second libgit2 read buys that boundary).
    super::redact_gate(&frame, display, redact_step, redact::Override::Run, |rs| {
        frame.fail_on_err(
            display,
            rs,
            paths
                .iter()
                .map(|p| Ok((p.clone(), git.diff(Some(p.as_str()))?)))
                .collect::<anyhow::Result<Vec<(String, String)>>>(),
        )
    })?;
    // Commit the index exactly as the user staged it — no re-stage
    // (issue #150). The historic `git add` over these paths folded each
    // staged file's full workdir state into the index, silently
    // swallowing hunks the user had deliberately left out of hunk-level
    // staging (`git add -p` / lazygit). That fold existed to capture
    // automatic rustfmt output before committing; the formatting step
    // was removed (#87), leaving it vestigial. `paths` still feeds the
    // diff, stats, and commit below; `Git::commit` commits the index
    // as-is, so unstaged hunks survive for a follow-up Run.
    frame.activate(draft_step);
    let diff_str = frame.fail_on_err(display, draft_step, staged_diff_json(git, &paths))?;
    let draft = frame.fail_on_err(
        display,
        draft_step,
        (deps.messenger)(diff_str.clone()).await,
    )?;
    frame.complete(display, draft_step, "Drafted message");

    frame.activate(commit_step);
    let step_display = frame.step_display(display);
    match generate_and_commit(
        git,
        &paths,
        &step_display,
        &frame,
        DraftedCommit {
            undo_start: undo_start.clone(),
            sub: None,
            draft,
            counts_floor,
        },
        &deps.messenger,
        &deps.confirm,
    )
    .await
    {
        Ok(()) => {}
        // Declining the confirmation is a user choice, not an error: report
        // it as a clean abort naming the outcome — nothing committed.
        Err(e) if e.downcast_ref::<CommitDeclined>().is_some() => {
            frame.fail(
                display,
                commit_step,
                Failure {
                    reason: "aborted by user".into(),
                    detail: vec!["no commit made".into()],
                },
            );
            anyhow::bail!("aborted — no commit made");
        }
        Err(e) => {
            // Headline only: the chain prints once, in full, after the
            // frame ends (main's boundary report) — not truncated
            // in-frame.
            frame.fail(
                display,
                commit_step,
                Failure {
                    reason: "failed".into(),
                    detail: vec![],
                },
            );
            return Err(e);
        }
    }
    frame.complete(display, commit_step, "Committed 1 batch");
    Ok(())
}

/// One commit's payload into [`generate_and_commit`]: the pre-drafted
/// message plus the Run's undo anchor. Bundled so the commit step's
/// signature stays under clippy's argument budget as it accretes state.
struct DraftedCommit {
    undo_start: Option<String>,
    /// 1-based batch index on a multi-batch Run (None single-commit): each
    /// landed batch reveals its `✓ hash subject` live sub-row on the Commit
    /// step (ADR 0018 amendment 3) — the in-frame echo of the entry that
    /// just flushed above it, appended in landing order.
    sub: Option<usize>,
    /// Run-wide counts geometry ([`counts_geometry`] over the Run's whole
    /// diff): floors this entry's footer so every entry of a multi-batch
    /// Run shares identical `+N`/`−M` columns.
    counts_floor: CountsGeometry,
    draft: generator::CommitOutput,
}

async fn generate_and_commit(
    git: &Git,
    paths: &[String],
    display: &Display,
    frame: &RunFrame,
    drafted: DraftedCommit,
    messenger: &CommitMessenger,
    confirm: &Confirm,
) -> anyhow::Result<()> {
    let DraftedCommit {
        undo_start,
        sub,
        counts_floor,
        draft,
    } = drafted;
    // The first draft was produced up front by the caller (the unstaged
    // multi-batch path drafts all batches concurrently — ADR 0014 — and the
    // single-commit path drafts inline before calling this). What remains is
    // the confirmation loop — whose Re-generate action redrafts against the
    // staged diff — then the commit. Staged stats (what the commit would land)
    // feed the preview footer — shown only when confirmation is on; landed
    // stats (what it did land) always feed the ✓ line. `display` is the
    // frame's step display (ADR 0018): on a terminal the preview, the
    // confirmation menu, and the staging warning ride the active step's live
    // rows, while the Commit entry flushes to permanent scrollback (see
    // [`RunFrame::flush`]); off a terminal it is the Run's own sink.
    let diff_str = staged_diff_json(git, paths)?;
    let stats = git.staged_stats(paths)?;
    let (message, body, preview_rows) = confirm_draft(
        (draft.message, draft.body),
        &stats,
        display,
        confirm,
        messenger,
        diff_str,
        frame.is_tty().then_some(frame),
    )
    .await?;

    // Erase the confirmed preview and commit. The undo anchors are written
    // around the commit itself: the start OID just before it lands (a Run
    // that aborts after earlier batches committed is still undoable for
    // everything that landed, and rewriting the same Run-start OID per batch
    // is idempotent), the tip OID just after — so the undo confirmation can
    // split the Run's commits from the user's own later ones.
    let undoable = undo_start.as_deref();
    if let Some(start) = undoable {
        undo::record(git, start)?;
    }
    display.clear_last(preview_rows);
    let hash = git.commit(message.clone(), body.clone())?;
    if undoable.is_some() {
        undo::record_tip(git)?;
    }
    let landed = git.committed_stats(paths)?;
    // The Commit entry lands as permanent scrollback (ADR 0018 bounded
    // frame): on a terminal `flush` erases the frame, prints the entry, and
    // repaints the frame below it — entries never ride the frame, so the
    // frame stays shorter than the terminal and whole-frame repaint never
    // scrolls. Off a terminal the rows go straight through the step display.
    frame.flush(display, |d| {
        d.commit_line(&hash, &message, &landed, counts_floor)
    });
    if let Some(index) = sub
        && let Some(step) = frame.active_index()
    {
        // Echo the landed commit as a `Landed` sub-row — hash + subject
        // through the shared subject grammar, matching the flushed entry
        // below it (ADR 0018; the old plain-text echo lost every color).
        frame.set_sub_row(
            step,
            index,
            &SubBody::Landed {
                hash: hash.clone(),
                subject: message.lines().next().unwrap_or_default().to_string(),
            },
        );
    }
    Ok(())
}

/// The Scan step shared by both branches: run the diff-stats query through
/// [`RunFrame::fail_on_err`], then complete the step with the pluralized
/// file count and the aggregate `+A −D` delta. Returns the stats for the
/// run-wide counts-grid geometry.
fn scanned(
    frame: &RunFrame,
    display: &Display,
    step: StepIdx,
    stats: anyhow::Result<Vec<crate::git::FileStats>>,
    files: usize,
) -> anyhow::Result<Vec<crate::git::FileStats>> {
    let stats = frame.fail_on_err(display, step, stats)?;
    frame.complete(
        display,
        step,
        format!("Scanned {}{}", plural(files, "file"), delta_suffix(&stats)),
    );
    Ok(stats)
}

/// One batch's file label for its draft sub-row: the lead file, plus
/// `+N` more` when the batch spans several — the row clamps at the text
/// budget, so a many-file label stays short (ADR 0018).
fn files_label(changes: &[generator::BatchChange]) -> String {
    let Some(lead) = changes.first() else {
        return "batch".to_string();
    };
    if changes.len() == 1 {
        lead.file.clone()
    } else {
        format!("{} +{} more", lead.file, changes.len() - 1)
    }
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

/// Production entry point for the default `aic` run — wires the real LLM
/// resolver, stdin y/n prompt, terminal confirmation menu, and message editor
/// into [`default_run`]. `no_redact` is the CLI's `--no-redact` bool, mapped
/// via [`Redact::from_flag`] — the only production site that may bypass the
/// Redaction Gate.
pub async fn default_workflow(hint: Option<&str>, no_redact: bool) -> anyhow::Result<()> {
    // Neon palette on dark terminals (ADR 0018 redesign): probe the
    // background once, before any styled output. No-op on pipes/NO_COLOR;
    // bounded ~400 ms on an unresponsive terminal.
    crate::render::palette::init_terminal_theme();
    // The resolve handoff shares the invocation's `no_redact` intent (issue
    // #159): `aic --no-redact` on a conflicted repo must not re-arm the gate
    // the user just disarmed — one invocation, one decision.
    let resolve = ResolveDeps {
        resolve: Box::new(|content: String| -> BoxFuture<anyhow::Result<String>> {
            Box::pin(async move { generator::Generator::resolve_conflict(&content).await })
        }),
        prompt: Box::new(input::prompt_yes_no),
        display: Display::new(),
        redact: Redact::from_flag(no_redact),
    };
    // The planner's streaming-capable backend that has not yet produced
    // reasoning is in a cold start, and past the loading grace its loading
    // frame says so. `None` on any config-read glitch — never falsely claim a
    // streaming capability.
    let planner_cold = crate::llm::LlmConfig::load()
        .ok()
        .and_then(|c| c.cold_start_program());
    // The `--hint` directive rides along with every planner and messenger
    // call of this Run — including the confirmation menu's Re-generate, which
    // re-enters the same messenger closure — so a re-roll honors the same
    // user intent as the first draft. Cloned per call like `planner_cold`.
    let hint_planner = hint.map(str::to_string);
    let hint_messenger = hint.map(str::to_string);
    let planner: BatchPlanner = Box::new(
        move |diff: String| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
            let cold_start = planner_cold.clone();
            let hint = hint_planner.clone();
            Box::pin(run_with_reasoning_feed(
                "Analyzing changes",
                cold_start,
                move |tap| -> BoxFuture<anyhow::Result<generator::BatchPlanOutput>> {
                    Box::pin(async move {
                        generator::Generator::split_patch_streaming(&diff, tap, hint.as_deref())
                            .await
                    })
                },
            ))
        },
    );
    let messenger: CommitMessenger = Box::new(
        move |diff: String| -> BoxFuture<anyhow::Result<generator::CommitOutput>> {
            let hint = hint_messenger.clone();
            Box::pin(async move {
                generator::Generator::generate_commit_message(&diff, hint.as_deref()).await
            })
        },
    );
    let git = Git::at(Path::new("."))?;
    // Absent/malformed config keeps the defaults (no menus) — same
    // tolerance the provider-field resolution uses (config > default).
    let config = config::Config::load().ok().flatten();
    let confirm_enabled = config
        .as_ref()
        .map(|c| c.confirm_before_commit())
        .unwrap_or(false);
    let review_enabled = config
        .as_ref()
        .map(|c| c.review_batch_plan())
        .unwrap_or(false);
    // The interactive menus render on stderr but read keys from stdin, so a
    // non-interactive stdin makes them unanswerable — and the failure would
    // otherwise surface mid-run, after the first batch is already staged.
    // Refuse to start rather than abort halfway.
    input::ensure_interactive_terminal(
        confirm_enabled,
        review_enabled,
        std::io::stdin().is_terminal(),
    )?;
    let confirm = if confirm_enabled {
        Confirm::interactive()
    } else {
        Confirm::Disabled
    };
    let review = if review_enabled {
        BatchReview::interactive()
    } else {
        BatchReview::Disabled
    };
    default_run(
        &git,
        resolve,
        RunDeps {
            display: Display::new(),
            planner,
            messenger,
            confirm,
            review,
            redact: Redact::from_flag(no_redact),
        },
    )
    .await
}
