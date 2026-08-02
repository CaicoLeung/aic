pub mod cli;
pub mod config;
pub mod diff;
pub mod display;
pub mod generator;
pub mod git;
pub mod llm;
pub mod prompt;
pub mod staging;
pub mod types;
pub mod update;

#[cfg(test)]
mod e2e;

use crate::cli::Commands;
use crate::display::Display;
use crate::git::Git;
use anyhow::Context;
use clap::{CommandFactory, Parser};
use indicatif::{MultiProgress, ProgressBar, ProgressDrawTarget};
use std::future::Future;
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

/// Print one reasoning line above the spinner, greedy-wrapped under the
/// shared `│ ` indent. Returns `false` on the first write failure (e.g.
/// stderr closed) so the caller can stop trying — reasoning display is
/// best-effort and must never break the commit flow.
fn print_reasoning_line(mp: &MultiProgress, width: usize, line: &str) -> bool {
    for piece in display::wrap_line(line, width) {
        if mp.println(format!("{}│ {piece}", display::MARGIN)).is_err() {
            return false;
        }
    }
    true
}

/// Run the batch-plan analysis behind a spinner that streams the model's
/// reasoning live. Each completed reasoning line scrolls above the spinner via
/// [`MultiProgress::println`], which is indicatif's flicker-free path for text
/// alongside a spinner — the spinner itself stays single-line (its in-place
/// redraw is imperceptible), and the reasoning never triggers a multi-line
/// clear-each-line-then-repaint cycle. The view is a sliding window with no
/// line cap; older lines scroll off the top of the terminal while the spinner
/// stays pinned at the bottom. The `MultiProgress` draw target is rate-limited
/// to 60 fps so bursts of completions coalesce into one repaint — higher
/// effective frame rate, no flicker.
async fn analyze_changes(diff: &str) -> anyhow::Result<generator::BatchPlanOutput> {
    let mp = MultiProgress::new();
    // Rate-limit repaints so a burst of completed reasoning lines coalesces
    // into one paint instead of flickering the whole progress region — see
    // `display::PROGRESS_REDRAW_HZ` for the rationale and the chosen rate.
    mp.set_draw_target(ProgressDrawTarget::stderr_with_hz(
        display::PROGRESS_REDRAW_HZ,
    ));
    let pb = mp.add(ProgressBar::new_spinner());
    pb.set_style(display::spinner_style()?);
    pb.set_message("Analyzing changes");
    pb.enable_steady_tick(display::SPINNER_TICK);

    let mut view = display::ThinkingView::new();
    // The feed's content budget: the shared terminal width minus the "│ "
    // decoration and one column of breathing room.
    let feed_width = display::terminal_width().saturating_sub(6);

    // Best-effort: once a write fails, stop printing rather than failing per
    // line; the spinner itself keeps working.
    let mut printing = true;
    let result = generator::Generator::split_patch_streaming(diff, |delta| {
        if !printing {
            return;
        }
        for line in view.push(delta) {
            if !print_reasoning_line(&mp, feed_width, &line) {
                printing = false;
                break;
            }
        }
    })
    .await;

    // Drain the tail: a partial line that never got its final `\n` — the
    // last reasoning visible before the spinner clears.
    if printing {
        for line in view.flush() {
            if !print_reasoning_line(&mp, feed_width, &line) {
                break;
            }
        }
    }

    pb.disable_steady_tick();
    pb.finish_and_clear();
    result
}

fn format_rust_files(git: &Git, paths: &[String], display: &Display) {
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
    let mut cmd = std::process::Command::new("cargo");
    cmd.args(["fmt", "--all"]);
    // Run in the repo's workdir — never the process CWD.
    if let Some(workdir) = git.workdir() {
        cmd.current_dir(workdir);
    }
    match cmd.status() {
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
    git: &Git,
    paths: &[String],
    display: &Display,
    prefix: &str,
    messenger: &CommitMessenger,
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
    let result =
        display::with_spinner("Generating commit message", messenger(diff.to_string())).await?;
    let hash = git.commit(result.message.clone(), result.body.clone())?;
    display.commit_line(&hash, &result.message, result.body.as_deref(), prefix);
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
    let state = git.state()?;

    if !state.is_conflicted() {
        display.no_conflicts();
        return Ok(());
    }
    if !state.resolvable() {
        // rebase / am — detected but refused in v1.
        display.refused(state);
        anyhow::bail!("aic cannot resolve a {} state in v1", state.label());
    }

    let files = git.conflicted_files()?;
    if files.is_empty() {
        // Conflicted state but no unmerged index entries — the user resolved
        // every file by hand and only the finalize step remains.
        display.all_resolved_offer_finalize(state);
        if prompt("finalize now?")? {
            git.finalize(state)?;
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
        let original_bytes = git.read_worktree(&f.path)?;
        let original = String::from_utf8(original_bytes)
            .with_context(|| format!("{} is not valid UTF-8 (should be Content)", f.path))?;

        let resolved = match display::with_spinner(
            &format!("Resolving {}", f.path),
            resolve(original.clone()),
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                display.skipped(&f.path, &format!("LLM error: {e:#}"));
                skipped_failed += 1;
                continue;
            }
        };

        // Marker validation — auto-retry once (ADR 0005).
        let resolved = if git::has_conflict_markers(&resolved) {
            match display::with_spinner(&format!("Retrying {}", f.path), resolve(original.clone()))
                .await
            {
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
            git.write_worktree(path, resolved)?;
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
        git.finalize(state)?;
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
/// auto-detect branch, which hands off to the resolve workflow.
pub(crate) async fn run_commit_workflow_impl(
    git: &Git,
    resolve: Resolver,
    prompt: Prompt,
    display: Display,
    planner: BatchPlanner,
    messenger: CommitMessenger,
) -> anyhow::Result<()> {
    // Auto-detect a conflicted repo and offer `aic resolve` before the normal
    // stage+commit flow (ADR 0005). The commit guard in `Git::commit` is the
    // deeper net; this prompt is the friendly front door.
    let state = git.state()?;
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
        let all_unstaged: Vec<String> = unstaged_files.iter().map(|f| f.path.clone()).collect();

        // Format Rust files FIRST, so the diff the model sees — and the hunk
        // numbering we stage by — reflects the final formatted source. Doing it
        // after capturing the diff (as before) would let `cargo fmt` shift
        // hunks out from under the indices the model returned.
        format_rust_files(git, &all_unstaged, &display);

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
                generate_and_commit(git, &paths, &display, &prefix, &messenger).await
            };
            if let Err(e) = outcome.await {
                anyhow::bail!(
                    "aborted on batch {} of {} after {} batch(es) committed. \
                     Remaining changes are still in the working tree — re-run \
                     `aic` to continue: {e:#}",
                    i + 1,
                    count,
                    i
                );
            }
        }
    } else {
        let paths: Vec<String> = staged_files.iter().map(|f| f.path.clone()).collect();
        format_rust_files(git, &paths, &display);
        let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
        git.add(&refs)?;
        generate_and_commit(git, &paths, &display, "", &messenger).await?;
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
    run_commit_workflow_impl(&git, resolver, prompt, Display::new(), planner, messenger).await
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

    match cli.command {
        Some(Commands::Setup) => config::run_setup(),
        Some(Commands::List) => config::run_list(),
        Some(Commands::Update) => update::run_update(),
        Some(Commands::Resolve) => run_resolve_workflow().await,
        Some(Commands::GenerateCompletion { shell }) => {
            write_completion(shell, &mut std::io::stdout());
            Ok(())
        }
        None => run_commit_workflow().await,
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
}
