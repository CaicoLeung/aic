//! `aic resolve` — the resolve workflow module (ADR 0005). Resolves merge
//! conflicts per-file via the LLM, reviews the combined diff, applies approved
//! files (sticky), and finalizes when all are resolved.
//!
//! The seams (`resolve`, `prompt`, `display`) are bundled in [`ResolveDeps`]
//! so the full workflow can be driven end-to-end in tests without a live LLM,
//! a TTY, or capturing real stderr. Production callers use
//! [`resolve_workflow`], which wires in `Generator::resolve_conflict`, stdin
//! [`input::prompt_yes_no`], and [`Display::new`]. The default Run's front
//! door (`crate::workflow::run::default_run`) hands off here on a conflicted repo.

use std::path::Path;
use std::time::Instant;

use anyhow::Context;

use crate::core::types::{BoxFuture, Prompt, Resolver};
use crate::git::Git;
use crate::git::conflict::{self, RepoState};
use crate::llm::generator;
use crate::llm::redact::{self, Redact};
use crate::llm::retry;
use crate::render::display::Display;
use crate::render::timeline::driver::{self, RunFrame};
use crate::render::timeline::{Failure, SubBody, ask_row};
use crate::workflow::input;
use crate::workflow::run::plural;

/// The resolve workflow's seam bundle. Passed as one unit so callers (the
/// production wiring and the Run front door's conflicted-repo handoff) stay
/// at two parameters: the repo and the deps. `redact` mirrors `RunDeps`
/// (issue #159, ADR 0017): `Redact::On` gates whole-file payloads before
/// any Backend call; `Redact::Off` is the one-off `--no-redact` override —
/// set by `aic resolve --no-redact` or carried from a top-level
/// `aic --no-redact` through the conflicted-repo handoff.
pub(crate) struct ResolveDeps {
    pub(crate) resolve: Resolver,
    pub(crate) prompt: Prompt,
    pub(crate) display: Display,
    pub(crate) redact: Redact,
}

/// The resolve workflow. See the module docs; behavior is specified by ADR
/// 0005, pinned by `e2e/resolve.rs` plus the auto-detect tests in
/// `e2e/commit.rs`.
///
/// Both entries (`aic resolve` and the mid-Run handoff) paint the resolve
/// timeline (ADR 0018): Detect → (Redaction Gate) → Resolve files → Review
/// resolutions → Finalize, with the per-file resolve sub-rows and the review
/// diffs as ephemeral live output and the review verdicts as durable rows.
/// The early exits — clean repo, rebase/am refusal, the all-resolved
/// finalize offer — render alone, with no timeline. Off a terminal the same
/// steps append the plain ledger (ADR 0018).
pub(crate) async fn resolve_run(git: &Git, deps: ResolveDeps) -> anyhow::Result<()> {
    let ResolveDeps {
        resolve,
        prompt,
        display,
        redact,
    } = deps;

    let conflict = git.conflict();
    let state = conflict.state()?;

    if !state.is_conflicted() {
        crate::git::conflict::no_conflicts(&display);
        return Ok(());
    }
    if !state.resolvable() {
        // rebase / am — detected but refused (not supported yet).
        crate::git::conflict::refused(&display, state);
        anyhow::bail!("aic cannot resolve a {} state yet", state.label());
    }

    let files = conflict.conflicted_files()?;
    if files.is_empty() {
        // Conflicted state but no unmerged index entries — the user resolved
        // every file by hand and only the finalize step remains.
        crate::git::conflict::all_resolved_offer_finalize(&display, state);
        if prompt("finalize now?")? {
            conflict.finalize(state)?;
            crate::git::conflict::finalize_done(&display, state);
        }
        return Ok(());
    }

    // Step skeleton (ADR 0018): Detect, the Redaction Gate (iff
    // `--no-redact` did not turn it off), Resolve, Review, Finalize.
    let redact_on = redact == Redact::On;
    let mut labels: Vec<String> = vec!["Detect conflicts".into()];
    if redact_on {
        labels.push("Redaction gate".into());
    }
    labels.push("Resolve files".into());
    labels.push("Review resolutions".into());
    labels.push("Finalize".into());
    let frame = RunFrame::new(labels).await;
    let _frame_guard = driver::install(frame.clone());
    const DETECT: usize = 0;
    let mut ix = 1;
    let redact_step = redact_on.then(|| {
        let s = ix;
        ix += 1;
        s
    });
    let resolve_step = ix;
    ix += 1;
    let review_step = ix;
    let finalize_step = ix;

    // Detect: the ⚠ header and conflicted-file summary ride the step's live
    // output and persist under its `✓ Detected N conflicts` summary.
    frame.activate(DETECT);
    let step_display = frame.step_display(&display);
    crate::git::conflict::conflict_detected(&step_display, state, files.len());
    crate::git::conflict::conflicted_summary(&step_display, &files);
    frame.complete(
        &display,
        DETECT,
        format!("Detected {}", plural(files.len(), "conflict")),
    );

    // Per-file resolution. `plans` carries (path, original, resolved) so the
    // review diff can be built without re-reading disk. `skipped` carries
    // (path, reason) for every file that will not reach review — structural
    // skips (binary/oversized — user must resolve by hand) and transient
    // failures (LLM error / markers after retry — user can re-run
    // `aic resolve`) — surfaced as caution rows in the Review step (spec
    // §2.8) instead of printing mid-Resolve.
    //
    // Reads are hoisted out of the resolve loop so the Redaction Gate below
    // scans every resolvable file *before* any Backend call: a secret in the
    // last file must refuse the Run without earlier files having been sent
    // (issue #159, ADR 0017). A non-UTF-8 resolvable file therefore also
    // aborts upfront instead of mid-loop — same error, earlier, and nothing
    // ungated leaves the machine first.
    let mut read: Vec<(String, String)> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    for f in &files {
        if !f.kind.resolvable() {
            skipped.push((f.path.clone(), f.kind.reason().to_string()));
            continue;
        }
        match read_original(&conflict, &f.path) {
            Ok(original) => read.push((f.path.clone(), original)),
            Err(e) => {
                frame.fail(
                    &display,
                    DETECT,
                    Failure {
                        reason: "failed".into(),
                        detail: vec![format!("{e:#}")],
                    },
                );
                return Err(e);
            }
        }
    }

    // Redaction Gate (issue #159, ADR 0017): the resolver sends whole-file
    // content, so gate the raw per-file payloads once, before the loop. The
    // marker-retry inside the loop re-sends bytes this gate already cleared
    // and stays ungated by design — the same boundary as the commit Run's
    // confirmation-menu Re-generate redraft. A refusal is the step's failed
    // screen (spec §2.9).
    if let Some(rs) = redact_step {
        frame.activate(rs);
        if let Err(e) = redact::gate(&read, redact::Override::Resolve) {
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

    // Resolve files (spec §2.8): one `[i/N]` sub-row per resolvable file —
    // in-flight while the LLM works (`Retrying {path}` on the marker-retry
    // attempt), `✓ {path} · dur` (with a `(retry)` note) when it lands.
    // Files the LLM can't resolve leave no durable row here; they surface in
    // the step summary (`N skipped`) and as Review caution rows.
    let resolvable = read.len();
    frame.activate(resolve_step);
    let mut plans: Vec<(String, String, String)> = Vec::new();
    for (i, (path, original)) in read.into_iter().enumerate() {
        let idx = i + 1;
        frame.set_label(
            resolve_step,
            format!("Resolving files · {idx} of {resolvable}"),
        );
        let started = Instant::now();
        // Per-file resolution on the shared retry module (seam C, ADR-0005).
        // The op folds the resolver's two failure shapes onto `RetryReason`:
        // marker-laden output → `Markers` (retryable), and any underlying
        // LLM error → `Fatal` (propagates immediately, never retried).
        // Clean output is `Ok`. `RetryPolicy::once()` is the budget-1 /
        // no-backoff auto-retry. The sub-row label distinguishes the first
        // attempt from the retry; `first_attempt` also records (after the
        // match) whether the retry ran, for the done row's `(retry)` note.
        let mut first_attempt = true;
        // Capture the resolver by reference so the `async move` block copies
        // the `&Resolver` (Copy) on each call instead of moving the owned
        // `Box` — otherwise the closure would be `FnOnce` and `retry` needs
        // `FnMut`.
        let resolve_ref = &resolve;
        let op = || {
            let retrying = !std::mem::replace(&mut first_attempt, false);
            let label = if retrying {
                format!("Retrying {path}")
            } else {
                format!("Resolving {path}")
            };
            frame.set_sub_row(resolve_step, idx, resolvable, &SubBody::InFlight(label));
            // `.to_string()` (not `.clone()`) because `original` is borrowed
            // by the closure, so the bare name is a `&String` whose
            // `.clone()` would copy the reference rather than the content.
            let content = original.to_string();
            async move {
                match resolve_ref(content).await {
                    Ok(resolved) if !conflict::has_conflict_markers(&resolved) => Ok(resolved),
                    Ok(_markers) => Err(retry::RetryReason::Markers),
                    Err(err) => Err(retry::RetryReason::Fatal(err)),
                }
            }
        };
        let resolved = match crate::llm::retry::retry(op, crate::llm::retry::RetryPolicy::once())
            .await
        {
            Ok(content) => {
                // `first_attempt` went false iff the marker-retry ran.
                let text = if first_attempt {
                    path.clone()
                } else {
                    format!("{path} (retry)")
                };
                frame.set_sub_row(
                    resolve_step,
                    idx,
                    resolvable,
                    &SubBody::Done(text, Some(started.elapsed())),
                );
                content
            }
            // Budget spent with markers still present — the file can't be
            // resolved; soft-skip it for a re-run.
            Err(crate::llm::retry::RetryError::Exhausted(crate::llm::retry::RetryExhausted {
                last_reason: crate::llm::retry::RetryReason::Markers,
                ..
            })) => {
                skipped.push((path, "markers remain after retry".to_string()));
                continue;
            }
            // The LLM call errored (first attempt or retry) and propagated
            // the original error verbatim.
            Err(crate::llm::retry::RetryError::Fatal(err)) => {
                skipped.push((path, format!("LLM error: {err:#}")));
                continue;
            }
            // The op only ever yields Ok / Markers / Fatal, so an exhausted
            // budget with any other `last_reason` is unreachable here.
            Err(crate::llm::retry::RetryError::Exhausted(_)) => unreachable!(
                "resolve op only yields Ok / Markers / Fatal, never Empty or Truncated"
            ),
        };

        plans.push((path, original, resolved));
    }

    if plans.is_empty() {
        // Nothing reached review — the failed screen names the step, and the
        // skipped files ride as its detail rows (ADR 0018) so the user
        // still sees why every file was refused.
        let mut detail: Vec<String> = skipped
            .iter()
            .map(|(path, reason)| format!("skipped: {path} — {reason}"))
            .collect();
        detail.push("resolve the conflicts manually".to_string());
        frame.fail(
            &display,
            resolve_step,
            Failure {
                reason: "no files could be resolved".into(),
                detail,
            },
        );
        anyhow::bail!("no files could be resolved; resolve the conflicts manually");
    }

    let skipped_unresolvable = files.len() - resolvable;
    let skipped_failed = skipped.len() - skipped_unresolvable;
    let mut summary = format!("Resolved {} of {resolvable}", plans.len());
    if !skipped.is_empty() {
        summary.push_str(&format!(" · {} skipped", skipped.len()));
    }
    frame.complete(&display, resolve_step, summary);

    // Review resolutions (spec §2.8): each file's diff + ask is ephemeral
    // live output; the verdict is durable (it records a worktree mutation).
    // `verdicts` is the live-row count the transient rows are truncated back
    // to, so the verdict rows (and the skipped caution rows printed first)
    // stack as the step's persistent block. Review still precedes the disk
    // write — the diff is in-memory (`unified_diff`), and `write_worktree`
    // only happens after approval (ADR 0005).
    frame.activate(review_step);
    for (path, reason) in &skipped {
        crate::git::conflict::skipped(&step_display, path, reason);
    }
    let mut verdicts = frame.live_len(review_step);
    let mut approved = 0usize;
    let mut rejected = 0usize;
    for (i, (path, original, resolved)) in plans.iter().enumerate() {
        frame.set_label(
            review_step,
            format!("Reviewing resolutions · {} of {}", i + 1, plans.len()),
        );
        crate::git::conflict::review_section(
            &step_display,
            path,
            &unified_diff(original, resolved),
        );
        let question = format!("apply {path}? [Y/n]");
        let apply = if frame.is_tty() {
            frame_ask(&frame, review_step, &question)
        } else {
            prompt(&question)?
        };
        frame.truncate_live(review_step, verdicts);
        if let Err(e) = if apply {
            conflict
                .write_worktree(path, resolved)
                .and_then(|()| git.add(&[path.as_str()]))
        } else {
            Ok(())
        } {
            frame.fail(
                &display,
                review_step,
                Failure {
                    reason: "failed".into(),
                    detail: vec![format!("{e:#}")],
                },
            );
            return Err(e);
        }
        if apply {
            crate::git::conflict::resolved(&step_display, path);
            approved += 1;
        } else {
            crate::git::conflict::rejected(&step_display, path);
            rejected += 1;
        }
        verdicts = frame.live_len(review_step);
    }
    let mut summary = format!(
        "Reviewed {} · {approved} applied",
        plural(plans.len(), "file")
    );
    if rejected > 0 {
        summary.push_str(&format!(" · {rejected} rejected"));
    }
    frame.complete(&display, review_step, summary);

    // Finalize is all-or-nothing: git's `--continue` blocks on any unmerged
    // path regardless (ADR 0005), so a single remaining blocker stops it.
    // Three blocker kinds, tracked separately so the hand-off message tells
    // the user what to do next instead of an opaque count:
    //   - rejected             resolvable file the user declined to apply
    //   - skipped_failed       resolvable kind, but the LLM/markers step failed
    //   - skipped_unresolvable binary / oversized / delete-modify — aic can't
    // Equivalent to the old `files.len() - approved == 0` test (every file
    // lands in exactly one of: skipped_unresolvable, skipped_failed, plans).
    // With any blocker the Finalize step never runs: it is dropped from the
    // frame (spec §2.8) and the hand-off rows ride under the Review summary.
    let needs_manual = rejected + skipped_failed + skipped_unresolvable;
    if needs_manual == 0 {
        frame.activate(finalize_step);
        if let Err(e) = conflict.finalize(state) {
            frame.fail(
                &display,
                finalize_step,
                Failure {
                    reason: "failed".into(),
                    detail: vec![format!("{e:#}")],
                },
            );
            return Err(e);
        }
        frame.complete(&display, finalize_step, finalize_summary(state));
    } else {
        frame.truncate_steps(finalize_step);
        crate::git::conflict::handoff(
            &step_display,
            approved,
            rejected,
            skipped_failed,
            skipped_unresolvable,
            state,
        );
    }

    Ok(())
}

/// Read one resolvable conflicted file's worktree bytes as UTF-8 content.
/// Split out of the Detect loop so the failure arm (the step's failed
/// screen + the propagated error) stays one `match` instead of a nested
/// `?` chain.
fn read_original(conflict: &conflict::Conflict<'_>, path: &str) -> anyhow::Result<String> {
    let bytes = conflict.read_worktree(path)?;
    String::from_utf8(bytes)
        .with_context(|| format!("{path} is not valid UTF-8 (should be Content)"))
}

/// The per-file apply ask as a frame row (spec §2.8): `? apply {path}?`
/// under the active Review step's diff, answered with raw y/n keys — Enter
/// = yes, the same default as `prompt_yes_no`; Esc / Ctrl-C / anything
/// else = no (a worktree mutation must never auto-approve on a stray key).
/// The ask's wall time is excluded from the step's duration (the menu
/// clock pause).
fn frame_ask(frame: &RunFrame, step: usize, question: &str) -> bool {
    frame.menu_open();
    let row = ask_row(question, frame.width(), &frame.theme());
    let base = frame.live_len(step);
    frame.replace_live_tail(step, base, vec![row]);
    let term = console::Term::stderr();
    let answer = loop {
        match term.read_key() {
            Ok(console::Key::Enter) | Ok(console::Key::Char('y')) => break true,
            Ok(console::Key::Char('n')) => break false,
            // Esc / Ctrl-C / a read failure (closed stdin) all decline.
            Ok(console::Key::Escape) | Ok(console::Key::Char('\u{3}')) | Err(_) => break false,
            Ok(_) => {}
        }
    };
    frame.truncate_live(step, base);
    frame.menu_close();
    answer
}

/// The Finalize step's completion summary (ADR 0018, open item 9): the
/// operation being finalized, from `RepoState` — `Finalized merge commit`
/// for a merge, `Finalized cherry-pick` / `Finalized revert` otherwise.
fn finalize_summary(state: RepoState) -> String {
    if state == RepoState::Merge {
        "Finalized merge commit".to_string()
    } else {
        format!("Finalized {}", state.label())
    }
}

/// Production entry point for `aic resolve` — wires the real LLM resolver and
/// stdin y/n prompt into [`resolve_run`]. `no_redact` is the resolve
/// subcommand's `--no-redact` bool (issue #159), mapped via
/// [`Redact::from_flag`] — the only production site that bypasses the
/// resolve Redaction Gate besides the top-level flag's handoff carry.
pub async fn resolve_workflow(no_redact: bool) -> anyhow::Result<()> {
    // Neon palette on dark terminals (ADR 0018 redesign) — same contract as
    // the default Run: probe once, before any styled output.
    crate::render::palette::init_terminal_theme();
    let resolve: Resolver = Box::new(|content: String| -> BoxFuture<anyhow::Result<String>> {
        Box::pin(async move { generator::Generator::resolve_conflict(&content).await })
    });
    let prompt: Prompt = Box::new(input::prompt_yes_no);
    let git = Git::at(Path::new("."))?;
    resolve_run(
        &git,
        ResolveDeps {
            resolve,
            prompt,
            display: Display::new(),
            redact: Redact::from_flag(no_redact),
        },
    )
    .await
}

/// Unified line diff between two strings for the resolution review (ADR 0005).
/// Computed in-memory via `similar` so the diff is shown *before* the resolved
/// content is written to disk — review must precede `git add`. Full context
/// (every equal line shown) and no `\ No newline` hints: this is a reading
/// view, not a patch to apply. Hunk headers (`@@`) render in the header style
/// via `review_section`'s fall-through arm.
fn unified_diff(old: &str, new: &str) -> String {
    use similar::TextDiff;
    // Full context: a radius of the larger line count always covers any
    // equal run. Deliberately not `usize::MAX` — similar multiplies the
    // radius by 2 internally (`group_diff_ops`), which overflows and panics
    // in debug builds the moment a diff contains a single equal line.
    let radius = old.lines().count().max(new.lines().count());
    TextDiff::from_lines(old, new)
        .unified_diff()
        .context_radius(radius)
        .missing_newline_hint(false)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::{RepoState, finalize_summary, unified_diff};

    /// The Finalize summary names the operation from `RepoState` (ADR 0018,
    /// open item 9): "merge commit" for a merge, the bare label otherwise —
    /// the sequencer states share their single-shot labels.
    #[test]
    fn finalize_summary_names_the_operation() {
        assert_eq!(finalize_summary(RepoState::Merge), "Finalized merge commit");
        assert_eq!(
            finalize_summary(RepoState::CherryPick),
            "Finalized cherry-pick"
        );
        assert_eq!(
            finalize_summary(RepoState::RevertSequence),
            "Finalized revert"
        );
    }

    /// The review diff keeps reading-view semantics on a *partial*
    /// resolution (equal lines present, the normal LLM-merge case): one
    /// hunk shows every line of both sides, and the full-context radius
    /// survives the `Equal` ops such input produces — `usize::MAX` panicked
    /// in debug builds via similar's internal `radius * 2` (PR #169).
    #[test]
    fn unified_diff_shows_full_context_on_partial_overlap() {
        let old = "fn main() {\n    let x = 1;\n}\n";
        let new = "fn main() {\n    let x = 2;\n}\n";
        assert_eq!(
            unified_diff(old, new),
            "@@ -1,3 +1,3 @@\n fn main() {\n-    let x = 1;\n+    let x = 2;\n }\n"
        );
    }
}
