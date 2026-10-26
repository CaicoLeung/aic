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
use std::time::{Duration, Instant};

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
use crate::render::cursor;
use crate::render::display::Display;
use crate::render::markdown::LoadingNotice;
use crate::render::markdown::loading_rows;
use crate::render::progress;
use crate::render::reasoning_feed;
use crate::render::timeline::driver::{self, RunFrame};
use crate::render::timeline::{Failure, SubBody};
use crate::workflow::confirm::{CommitDeclined, Confirm, confirm_draft};
use crate::workflow::grouping;
use crate::workflow::input;
use crate::workflow::resolve::{ResolveDeps, resolve_run};
use crate::workflow::review::{BatchReview, review_plan};
use crate::workflow::undo;

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
/// (config-conditional steps included exactly when they will run, spec
/// §2.2/§2.3), the whole frame repainting on a terminal, the plain ledger
/// appending off one. The frame starts only after the status check confirms
/// there is work — `nothing to commit` still renders alone, with no timeline.
/// `"{n} {word}"` with English pluralization — the ledger's step summaries
/// (`✓ Scanned 1 file`, `✓ Planned 2 batches`).
pub(crate) fn plural(n: usize, word: &str) -> String {
    let plural = if word.ends_with("ch") || word.ends_with("sh") || word.ends_with(['s', 'x', 'z'])
    {
        format!("{word}es")
    } else {
        format!("{word}s")
    };
    if n == 1 {
        format!("{n} {word}")
    } else {
        format!("{n} {plural}")
    }
}

/// Compact count for delta summaries (`2.1k`, `15k`, `1.2m`) — above a
/// thousand the exact number no longer earns its columns (ADR 0018
/// redesign). Below it, the plain number.
pub(crate) fn compact_count(n: usize) -> String {
    let unit = |n: usize, divisor: usize, suffix: &str| {
        let tenths = (n * 10 + divisor / 2) / divisor; // round half up
        let (whole, frac) = (tenths / 10, tenths % 10);
        if frac == 0 || whole >= 10 {
            format!("{whole}{suffix}")
        } else {
            format!("{whole}.{frac}{suffix}")
        }
    };
    match n {
        0..=999 => n.to_string(),
        1_000..=999_999 => unit(n, 1_000, "k"),
        _ => unit(n, 1_000_000, "m"),
    }
}
/// The Scan summary's aggregate text-line delta (` · +12 −3`), summed from
/// per-file stats (unstaged Runs read the workdir diff, staged Runs the
/// index). Always present: a zero delta still tells the reader the scan saw
/// no text-line churn (binary-only or mode-only changes).
pub(crate) fn delta_suffix(stats: &[crate::git::FileStats]) -> String {
    let added: usize = stats.iter().map(|s| s.added).sum();
    let deleted: usize = stats.iter().map(|s| s.deleted).sum();
    format!(" · +{} −{}", compact_count(added), compact_count(deleted))
}

pub(crate) async fn commit_run(git: &Git, deps: RunDeps) -> anyhow::Result<()> {
    let RunDeps {
        display,
        planner,
        messenger,
        confirm,
        review,
        redact,
    } = deps;

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
            display.nothing_to_commit();
            return Ok(());
        }

        // Step skeleton (spec §2.3): Scan, the Redaction Gate (iff `--no-redact`
        // did not turn it off), Plan, Review (iff configured), Draft, Commit.
        let redact_on = redact == Redact::On;
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
        const SCAN: usize = 0;
        let mut ix = 1;
        let redact_step = redact_on.then(|| {
            let s = ix;
            ix += 1;
            s
        });
        let plan_step = ix;
        ix += 1;
        let review_step = review_on.then(|| {
            let s = ix;
            ix += 1;
            s
        });
        let draft_step = ix;
        ix += 1;
        let commit_step = ix;

        frame.activate(SCAN);
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
        // (path, raw diff) pairs from the snapshot above — the single
        // source for the Redaction Gate below and the deterministic
        // fallback's regrouping, so both consume exactly what the model
        // sees.
        let raw_pairs: Vec<(String, String)> = unstaged_files
            .iter()
            .map(|f| (f.path.clone(), raw_diffs[&f.path].clone()))
            .collect();
        let scan_paths: Vec<String> = unstaged_files.iter().map(|f| f.path.clone()).collect();
        let scan_delta = delta_suffix(&git.workdir_stats(&scan_paths)?);
        frame.complete(
            &display,
            SCAN,
            format!(
                "Scanned {}{}",
                plural(unstaged_files.len(), "file"),
                scan_delta
            ),
        );

        // Redaction Gate, first payload site (issue #155, ADR 0017): the
        // raw per-file diffs feed every LLM payload of this branch — the
        // plan envelope above and each batch's Drafted Message input (sliced
        // from `raw_diffs`) — so gating here once refuses before any Backend
        // call. The confirmation menu's Re-generate redraft stays ungated by
        // design: it re-sends content this gate already cleared. A refusal
        // is the step's failed screen: the headline names the gate, the
        // detail row carries the full files + patterns refusal (spec §2.9).
        if let Some(rs) = redact_step {
            frame.activate(rs);
            if let Err(e) = redact::gate(&raw_pairs, redact::Override::Run) {
                frame.fail(
                    &display,
                    rs,
                    Failure {
                        reason: "secret-like content detected".into(),
                        detail: vec![format!("{e:#}")],
                    },
                );
                return Err(e);
            }
            frame.complete(&display, rs, "Redaction gate · clean");
        }

        frame.activate(plan_step);
        let result = match planner(diff.to_string()).await {
            Ok(result) => result,
            Err(e) => {
                frame.fail(
                    &display,
                    plan_step,
                    Failure {
                        reason: "failed".into(),
                        detail: vec![format!("{e:#}")],
                    },
                );
                return Err(e);
            }
        };
        // An invalid plan is an LLM malfunction, not a user problem: warn and
        // regroup deterministically instead of failing the Run. The engine's
        // output is a valid partition by construction (every hunk of real
        // work lands in exactly one batch), so issue #34's silent no-op
        // cannot recur — real work always yields ≥1 batch. The re-validation
        // is defensive only. The summary reads "Grouped" on this fallback
        // path (spec §2.4) and the caution row rides right under it.
        let mut fallback_err: Option<String> = None;
        let mut result = match generator::validate_batch_plan(&result, &file_hunk_counts) {
            Ok(()) => result,
            Err(plan_err) => {
                fallback_err = Some(plan_err.to_string());
                let plan = grouping::plan_from_diffs(&raw_pairs);
                generator::validate_batch_plan(&plan, &file_hunk_counts)
                    .context("deterministic fallback plan failed validation")?;
                plan
            }
        };
        let count = result.batches.len();
        frame.complete(
            &display,
            plan_step,
            match &fallback_err {
                Some(_) => format!("Grouped {}", plural(count, "batch")),
                None => format!("Planned {}", plural(count, "batch")),
            },
        );
        if let Some(err) = fallback_err {
            frame.step_display(&display).warn(&format!(
                "LLM batch plan invalid ({err}); regrouping deterministically"
            ));
        }

        // Batch Review (issue #157): the opt-in pre-draft gate — applies to
        // whichever plan proceeds (the LLM's or the deterministic
        // fallback's) and merges Batches before any Drafted Message is paid
        // for. Abort here is the cheapest exit: nothing staged, nothing
        // drafted.
        if let Some(rs) = review_step {
            frame.activate(rs);
            result = review_plan(result, &file_hunk_counts, &display, &review, &frame, rs)?;
        }
        let count = result.batches.len();

        // Pre-draft every batch's message concurrently (ADR 0014): the LLM
        // round-trips dominate a multi-batch Run's wall-clock, and each batch's
        // diff content is fully known at plan time, so fanning the drafts out
        // collapses N sequential waits into one. Each draft is sliced from the
        // plan-time workdir diff (the numbering the model saw) rather than the
        // staged diff; the two diverge only under a pre-commit hook that
        // rewrites bytes — irrelevant to a commit message — and the
        // confirmation Re-generate action still redrafts against the staged
        // diff. Each concurrent draft reveals its own `[i/N]` sub-row as it
        // starts and flips it to a `✓` on finish (spec §2.6); all sub-rows
        // erase together at step completion. Order-preserving `buffered`
        // keeps the messenger's call order (and the test messengers'
        // per-call counters) deterministic. The fixed MAX_CONCURRENT_DRAFTS
        // is the committed per-batch concurrency cap of this program.
        let batch_diffs: Vec<String> = result
            .batches
            .iter()
            .map(|b| diff_json::plan_batch_diff_json(b, &raw_diffs))
            .collect::<anyhow::Result<Vec<_>>>()?;
        frame.activate(draft_step);
        let drafts: Vec<anyhow::Result<generator::CommitOutput>> = {
            let futs = batch_diffs.into_iter().enumerate().map(|(i, batch_diff)| {
                let frame = frame.clone();
                let messenger = &messenger;
                async move {
                    frame.set_sub_row(
                        draft_step,
                        i + 1,
                        count,
                        &SubBody::InFlight("Generating commit message".to_string()),
                    );
                    let started = Instant::now();
                    let outcome = messenger(batch_diff).await;
                    frame.set_sub_row(
                        draft_step,
                        i + 1,
                        count,
                        &SubBody::Done(String::new(), Some(started.elapsed())),
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
            &display,
            draft_step,
            format!("Drafted {}", plural(count, "message")),
        );

        frame.activate(commit_step);
        let step_display = frame.step_display(&display);
        let mut staging = staging::Staging::new();
        for (i, (batch, draft)) in result.batches.iter().zip(drafts).enumerate() {
            let prefix = format!("[{}/{}]", i + 1, count);
            frame.set_label(
                commit_step,
                format!("Committing batch {} of {count}", i + 1),
            );
            // Stage this batch's hunks, then commit its pre-drafted message.
            // Either step failing after earlier batches already committed
            // leaves the repo partially committed, so both share one abort
            // message naming how far we got and that the rest is recoverable by
            // re-running `aic`. The draft was produced up front; surfacing its
            // error after staging (not before) keeps the "remaining changes
            // are still staged" contract identical to drafting inline.
            let outcome = async {
                let paths = staging.stage_batch(git, batch, &step_display)?;
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
                    &frame,
                    DraftedCommit {
                        undo_start: undo_start.clone(),
                        prefix,
                        draft,
                    },
                    &messenger,
                    &confirm,
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
                // carry the recovery contract (spec §2.9).
                if e.downcast_ref::<CommitDeclined>().is_some() {
                    frame.fail(
                        &display,
                        commit_step,
                        Failure {
                            reason: format!("declined on batch {} of {count}", i + 1),
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
                let mut detail = vec![format!("{e:#}")];
                detail.extend(remediation);
                frame.fail(
                    &display,
                    commit_step,
                    Failure {
                        reason: format!("failed on batch {} of {count}", i + 1),
                        detail,
                    },
                );
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
        frame.complete(
            &display,
            commit_step,
            format!("Committed {}", plural(count, "batch")),
        );
    } else {
        let paths: Vec<String> = staged_files.iter().map(|f| f.path.clone()).collect();
        // Step skeleton (spec §2.3): the staged branch's three steps (four
        // with the gate) — no plan or review; the user did the partitioning.
        let redact_on = redact == Redact::On;
        let mut labels: Vec<String> = vec!["Scan changes".into()];
        if redact_on {
            labels.push("Redaction gate".into());
        }
        labels.push("Draft message".into());
        labels.push("Commit".into());
        let frame = RunFrame::new(labels).await;
        let _frame_guard = driver::install(frame.clone());
        const SCAN: usize = 0;
        let redact_step = redact_on.then_some(1);
        let draft_step = usize::from(redact_on) + 1;
        let commit_step = draft_step + 1;

        frame.activate(SCAN);
        let staged_paths: Vec<String> = staged_files.iter().map(|f| f.path.clone()).collect();
        let scan_delta = delta_suffix(&git.staged_stats(&staged_paths)?);
        frame.complete(
            &display,
            SCAN,
            format!(
                "Scanned {}{}",
                plural(staged_files.len(), "file"),
                scan_delta
            ),
        );

        if let Some(rs) = redact_step {
            frame.activate(rs);
            // Redaction Gate, second payload site (issue #155, ADR 0017): the
            // staged branch's one LLM call sends `staged_diff_json`'s content.
            // Gate reads the same per-file diffs itself rather than scanning
            // inside `staged_diff_json` — that fn is shared with the confirm
            // menu's Re-generate redraft, which stays ungated by design (the
            // cheap second libgit2 read buys that boundary).
            let pairs: Vec<(String, String)> = paths
                .iter()
                .map(|p| Ok((p.clone(), git.diff(Some(p.as_str()))?)))
                .collect::<anyhow::Result<_>>()?;
            if let Err(e) = redact::gate(&pairs, redact::Override::Run) {
                frame.fail(
                    &display,
                    rs,
                    Failure {
                        reason: "secret-like content detected".into(),
                        detail: vec![format!("{e:#}")],
                    },
                );
                return Err(e);
            }
            frame.complete(&display, rs, "Redaction gate · clean");
        }
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
        let diff_str = staged_diff_json(git, &paths)?;
        let draft = match messenger(diff_str.clone()).await {
            Ok(d) => d,
            Err(e) => {
                frame.fail(
                    &display,
                    draft_step,
                    Failure {
                        reason: "failed".into(),
                        detail: vec![format!("{e:#}")],
                    },
                );
                return Err(e);
            }
        };
        frame.complete(&display, draft_step, "Drafted message");

        frame.activate(commit_step);
        let step_display = frame.step_display(&display);
        match generate_and_commit(
            git,
            &paths,
            &step_display,
            &frame,
            DraftedCommit {
                undo_start: undo_start.clone(),
                prefix: String::new(),
                draft,
            },
            &messenger,
            &confirm,
        )
        .await
        {
            Ok(()) => {}
            // Declining the confirmation is a user choice, not an error: report
            // it as a clean abort naming the outcome — nothing committed.
            Err(e) if e.downcast_ref::<CommitDeclined>().is_some() => {
                frame.fail(
                    &display,
                    commit_step,
                    Failure {
                        reason: "aborted by user".into(),
                        detail: vec!["no commit made".into()],
                    },
                );
                anyhow::bail!("aborted — no commit made");
            }
            Err(e) => {
                frame.fail(
                    &display,
                    commit_step,
                    Failure {
                        reason: "failed".into(),
                        detail: vec![format!("{e:#}")],
                    },
                );
                return Err(e);
            }
        }
        frame.complete(&display, commit_step, "Committed 1 batch");
    }

    Ok(())
}

/// One commit's payload into [`generate_and_commit`]: the pre-drafted
/// message plus the Run's undo anchor. Bundled so the commit step's
/// signature stays under clippy's argument budget as it accretes state.
struct DraftedCommit {
    undo_start: Option<String>,
    /// The Commit entry's `[i/N]` prefix (empty on the single-commit path).
    prefix: String,
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
        prefix,
        draft,
    } = drafted;
    // The first draft was produced up front by the caller (the unstaged
    // multi-batch path drafts all batches concurrently — ADR 0014 — and the
    // single-commit path drafts inline before calling this). What remains is
    // the confirmation loop — whose Re-generate action redrafts against the
    // staged diff — then the commit. Staged stats (what the commit would land)
    // feed the preview footer — shown only when confirmation is on; landed
    // stats (what it did land) always feed the ✓ line. `display` is the
    // frame's step display (spec §2.7): on a terminal the preview, the
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
        d.commit_line(&hash, &message, &prefix, &landed)
    });
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

/// Run an LLM call behind the live reasoning feed. On a terminal with a Run
/// frame on screen the feed is the frame's [`FrameReasoningSink`] — the
/// window rides the Plan step's live rows, its height budgeted against the
/// whole frame (spec §2.4); otherwise the standalone probe-and-paint
/// [`progress::ReasoningRenderer`] path, as before the timeline.
async fn run_with_reasoning_feed<F, T>(
    label: &'static str,
    cold_start: Option<String>,
    make_call: F,
) -> anyhow::Result<T>
where
    F: FnOnce(reasoning_feed::ReasoningTap) -> BoxFuture<anyhow::Result<T>>,
    T: Send,
{
    if let Some(frame) = driver::active_frame().filter(|f| f.is_tty()) {
        let step = frame.active_index();
        let mut sink = FrameReasoningSink {
            frame: frame.clone(),
            step: step.unwrap_or(0),
            label,
            feed_width: frame.width().saturating_sub(6).max(20),
        };
        return reasoning_feed::run(
            &mut sink,
            frame.window_budget(),
            cold_start.as_deref(),
            make_call,
        )
        .await;
    }
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

/// The reasoning feed as the frame's Plan-step live rows (spec §2.4): the
/// window's rows replace the step's live output on every paint, and the
/// spinner + elapsed ride the step's own label (animated by the frame's
/// ticker), so only the window content repaints as reasoning streams. The
/// loading notice (cold start past the grace window) is the same window's
/// lone row. `finish` erases the window in one frame — the step's `✓`
/// flip takes it from there.
struct FrameReasoningSink {
    frame: RunFrame,
    step: usize,
    label: &'static str,
    feed_width: usize,
}

impl reasoning_feed::ReasoningSink for FrameReasoningSink {
    fn paint(&mut self, window: &[String], in_code_start: bool) {
        let budget = self.frame.window_budget();
        let mut rows = crate::render::markdown::reasoning_rows(
            "⠋",
            self.label,
            window,
            self.feed_width,
            None,
            budget,
            in_code_start,
        );
        // Drop the spinner+elapsed headline row: the step's own label carries
        // both (the ticker animates the glyph, the frame bakes the elapsed).
        if !rows.is_empty() {
            rows.remove(0);
        }
        self.frame.live(self.step, rows);
    }

    fn paint_loading(&mut self, _elapsed: Duration, notice: LoadingNotice) {
        let rows: Vec<String> =
            loading_rows("⠋", self.label, Duration::ZERO, notice, self.feed_width)
                .into_iter()
                .skip(1)
                .collect();
        self.frame.live(self.step, rows);
    }

    fn refresh(&mut self, window: &[String], in_code_start: bool, _elapsed: Duration) {
        self.paint(window, in_code_start);
    }

    fn finish(&mut self) {
        self.frame.clear_live(self.step);
    }
}

/// Production entry point for the default `aic` run — wires the real LLM
/// resolver, stdin y/n prompt, terminal confirmation menu, and message editor
/// into [`default_run`]. `no_redact` is the CLI's `--no-redact` bool, mapped
/// via [`Redact::from_flag`] — the only production site that may bypass the
/// Redaction Gate.
pub async fn default_workflow(hint: Option<&str>, no_redact: bool) -> anyhow::Result<()> {
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
#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::FileStats;

    /// `compact_count` keeps exact numbers below a thousand and collapses
    /// above it: one decimal while the whole part is a single digit, no
    /// decimal once it isn't, `k`→`m` at the next magnitude.
    #[test]
    fn compact_count_collapses_only_above_a_thousand() {
        assert_eq!(compact_count(0), "0");
        assert_eq!(compact_count(999), "999");
        assert_eq!(compact_count(1_000), "1k");
        assert_eq!(compact_count(2_100), "2.1k");
        assert_eq!(compact_count(9_999), "10k");
        assert_eq!(compact_count(15_000), "15k");
        assert_eq!(compact_count(1_234_567), "1.2m");
    }

    /// `delta_suffix` sums per-file stats into one ` · +A −D` segment and
    /// stays present at zero (binary-only scans still show the churn pair).
    #[test]
    fn delta_suffix_sums_and_survives_zero() {
        let stats = |a: usize, d: usize| FileStats {
            path: "x.rs".into(),
            added: a,
            deleted: d,
            new: false,
            removed: false,
            binary: false,
            old_size: 0,
            new_size: 0,
        };
        assert_eq!(delta_suffix(&[stats(3, 1), stats(5, 0)]), " · +8 −1");
        assert_eq!(delta_suffix(&[]), " · +0 −0");
    }
}
