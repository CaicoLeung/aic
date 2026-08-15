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

use anyhow::Context;

use crate::core::types::{BoxFuture, Prompt, Resolver};
use crate::git::Git;
use crate::git::conflict;
use crate::llm::generator;
use crate::llm::retry;
use crate::render::display::Display;
use crate::render::progress;
use crate::workflow::input;

/// The resolve workflow's seam bundle. Passed as one unit so callers (the
/// production wiring and the Run front door's conflicted-repo handoff) stay
/// at two parameters: the repo and the deps.
pub(crate) struct ResolveDeps {
    pub(crate) resolve: Resolver,
    pub(crate) prompt: Prompt,
    pub(crate) display: Display,
}

/// The resolve workflow. See the module docs; behavior is specified by ADR 0005
/// and pinned by `e2e/resolve.rs` plus the auto-detect tests in `e2e/commit.rs`.
pub(crate) async fn resolve_run(git: &Git, deps: ResolveDeps) -> anyhow::Result<()> {
    let ResolveDeps {
        resolve,
        prompt,
        display,
    } = deps;

    let conflict = git.conflict();
    let state = conflict.state()?;

    if !state.is_conflicted() {
        crate::git::conflict::no_conflicts(&display);
        return Ok(());
    }
    if !state.resolvable() {
        // rebase / am — detected but refused in v1.
        crate::git::conflict::refused(&display, state);
        anyhow::bail!("aic cannot resolve a {} state in v1", state.label());
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

    crate::git::conflict::conflict_detected(&display, state, files.len());
    crate::git::conflict::conflicted_summary(&display, &files);

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
            crate::git::conflict::skipped(&display, &f.path, f.kind.reason());
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
        let resolved = match crate::llm::retry::retry(op, crate::llm::retry::RetryPolicy::once())
            .await
        {
            Ok(content) => content,
            // Budget spent with markers still present — the file can't be
            // resolved; soft-skip it for a re-run.
            Err(crate::llm::retry::RetryError::Exhausted(crate::llm::retry::RetryExhausted {
                last_reason: crate::llm::retry::RetryReason::Markers,
                ..
            })) => {
                crate::git::conflict::skipped(&display, &f.path, "markers remain after retry");
                skipped_failed += 1;
                continue;
            }
            // The LLM call errored (first attempt or retry) and propagated the
            // original error verbatim.
            Err(crate::llm::retry::RetryError::Fatal(err)) => {
                crate::git::conflict::skipped(&display, &f.path, &format!("LLM error: {err:#}"));
                skipped_failed += 1;
                continue;
            }
            // The op only ever yields Ok / Markers / Fatal, so an exhausted
            // budget with any other `last_reason` is unreachable here.
            Err(crate::llm::retry::RetryError::Exhausted(_)) => unreachable!(
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
    crate::git::conflict::review_section(&display, &combined);

    let mut approved = 0usize;
    let mut rejected = 0usize;
    for (path, _original, resolved) in &plans {
        if prompt(&format!("apply {path}?"))? {
            conflict.write_worktree(path, resolved)?;
            git.add(&[path.as_str()])?;
            crate::git::conflict::resolved(&display, path);
            approved += 1;
        } else {
            crate::git::conflict::rejected(&display, path);
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
        crate::git::conflict::finalize_done(&display, state);
    } else {
        crate::git::conflict::handoff(
            &display,
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
/// stdin y/n prompt into [`resolve_run`].
pub async fn resolve_workflow() -> anyhow::Result<()> {
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
        },
    )
    .await
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
