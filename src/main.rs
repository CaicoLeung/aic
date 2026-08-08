pub mod cli;
pub mod config;
pub mod confirm;
pub mod conflict;
pub mod diff;
pub mod display;
pub mod generator;
pub mod git;
pub mod grouping;
pub mod layout;
pub mod llm;
pub mod progress;
pub mod prompt;
pub mod retry;
pub mod staging;
pub mod types;
pub mod update;

#[cfg(test)]
mod e2e;

use crate::cli::Commands;
use crate::confirm::{
    CommitDeclined, Confirm, confirm_draft, ensure_confirm_terminal, inquire_opt,
};
use crate::display::Display;
use crate::git::Git;
use anyhow::Context;
use clap::{CommandFactory, Parser};
use std::future::Future;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
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
async fn analyze_changes(diff: &str) -> anyhow::Result<generator::BatchPlanOutput> {
    let mut view = progress::ThinkingView::new();
    let mut renderer = progress::ReasoningRenderer::new("Analyzing changes");

    let result = generator::Generator::split_patch_streaming(diff, |delta| {
        let window = view.push(delta);
        renderer.paint(&window);
    })
    .await;

    // Thinking is over: `finish` erases the reasoning block (in-place all
    // along, so nothing ever hit the scrollback) and parks the cursor below
    // it. The renderer's Drop is a backstop if the stream aborted first.
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
    // tolerance `LLM::from_env` uses for the provider fields.
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

/// Where a shell's completion script is installed, and whether the shell
/// picks it up on reload with no further action.
struct CompletionTarget {
    path: PathBuf,
    autoloaded: bool,
}

/// Shells `aic completion` can install for — the single source of truth for
/// everything per-shell: the menu label, the `$SHELL` basenames that detect it,
/// the script generator, the install path, and the follow-up when that path
/// isn't autoloaded. Adding a shell means adding one variant and filling in each
/// method; the exhaustive matches turn a forgotten step into a compile error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Shell {
    Bash,
    Zsh,
    Fish,
    Nushell,
}

impl Shell {
    /// Installable shells, in menu order.
    const ALL: [Self; 4] = [Self::Bash, Self::Zsh, Self::Fish, Self::Nushell];

    /// Maps a shell basename (e.g. the tail of `$SHELL`) to a `Shell`.
    fn from_name(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|&shell| shell.detect_names().contains(&name))
    }

    /// Lowercase display name — the menu label and the word used in messages.
    fn name(self) -> &'static str {
        match self {
            Self::Bash => "bash",
            Self::Zsh => "zsh",
            Self::Fish => "fish",
            Self::Nushell => "nushell",
        }
    }

    /// `$SHELL`-basenames that identify this shell (e.g. the tail of `/usr/bin/zsh`).
    fn detect_names(self) -> &'static [&'static str] {
        match self {
            Self::Bash => &["bash"],
            Self::Zsh => &["zsh"],
            Self::Fish => &["fish"],
            Self::Nushell => &["nu", "nushell"],
        }
    }

    /// Writes this shell's completion script to `out`.
    fn generate(self, cmd: &mut clap::Command, bin_name: &str, out: &mut dyn std::io::Write) {
        use clap_complete::{Shell as ClapShell, generate};
        use clap_complete_nushell::Nushell;
        match self {
            Self::Bash => generate(ClapShell::Bash, cmd, bin_name, out),
            Self::Zsh => generate(ClapShell::Zsh, cmd, bin_name, out),
            Self::Fish => generate(ClapShell::Fish, cmd, bin_name, out),
            Self::Nushell => generate(Nushell, cmd, bin_name, out),
        }
    }

    /// Conventional install location for this shell, plus whether the shell
    /// autoloads it. `bash` and `fish` are always autoloaded; `zsh` never is —
    /// its `site-functions` dir only loads when it's on `$fpath`, which depends
    /// on the user's zsh (Homebrew's own zsh adds the brew dir; macOS system
    /// zsh does not), so a follow-up is always shown. The Homebrew dir is still
    /// the better *location* when `aic` is brewed. `nushell` lands in its config
    /// dir but must be `source`d from `config.nu`.
    fn install_target(self, home: &Path, brew_prefix: Option<&Path>) -> CompletionTarget {
        match self {
            Self::Fish => CompletionTarget {
                path: home.join(".config/fish/completions/aic.fish"),
                autoloaded: true,
            },
            Self::Bash => CompletionTarget {
                path: home.join(".local/share/bash-completion/completions/aic"),
                autoloaded: true,
            },
            // Never autoloaded: the site-functions dir loads only if the user's
            // zsh has it on $fpath (Homebrew's own zsh yes; system zsh no).
            Self::Zsh => {
                let dir = brew_prefix
                    .map(|p| p.join("share/zsh/site-functions"))
                    .unwrap_or_else(|| home.join(".local/share/zsh/site-functions"));
                CompletionTarget {
                    path: dir.join("_aic"),
                    autoloaded: false,
                }
            }
            Self::Nushell => CompletionTarget {
                path: home.join(".config/nushell/aic.nu"),
                autoloaded: false,
            },
        }
    }

    /// Follow-up the user must perform when the install isn't autoloaded, or
    /// `None` when a reload is all that's needed.
    fn follow_up(self, path: &Path) -> Option<String> {
        let dir = path.parent().unwrap_or(path);
        match self {
            Self::Zsh => Some(format!(
                "Add this directory to $fpath for it to take effect:\n  \
                 fpath+=({0})  # then: autoload -Uz compinit && compinit",
                dir.display()
            )),
            Self::Nushell => Some(format!(
                "Source it from your nushell config to take effect — add to `config.nu`:\n  \
                 source {0}",
                path.display()
            )),
            // bash/fish are autoloaded, so this is never reached for them —
            // but the arm keeps the match exhaustive when a shell is added.
            Self::Bash | Self::Fish => None,
        }
    }
}

/// Best-effort detection of the current shell from `$SHELL`. `$SHELL` is the
/// login shell, not necessarily the one actually running, so it's only a hint —
/// it defaults the interactive menu and is the non-TTY fallback.
fn detect_shell() -> Option<Shell> {
    let shell = std::env::var("SHELL").ok()?;
    let name = shell.rsplit('/').next()?;
    Shell::from_name(name)
}

/// If `aic` itself lives under a Homebrew prefix, returns that prefix so zsh
/// completions can land in the tap's conventional `site-functions` directory.
fn homebrew_prefix_from(exe: &Path) -> Option<PathBuf> {
    [Path::new("/opt/homebrew"), Path::new("/usr/local")]
        .into_iter()
        .find(|prefix| exe.starts_with(prefix))
        .map(Path::to_path_buf)
}

/// Writes `shell`'s completion script to `out`.
fn write_completion(shell: Shell, out: &mut dyn std::io::Write) {
    let mut cmd = cli::Cli::command();
    let bin_name = cmd.get_name().to_owned();
    shell.generate(&mut cmd, &bin_name, out);
}

/// Writes `shell`'s completion script to its install location.
///
/// Split from [`install_completion`] so the file I/O can be exercised against a
/// temp directory instead of the real home.
fn install_completion_impl(
    shell: Shell,
    home: &Path,
    brew_prefix: Option<&Path>,
) -> anyhow::Result<CompletionTarget> {
    let target = shell.install_target(home, brew_prefix);
    if let Some(parent) = target.path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut buf = Vec::new();
    write_completion(shell, &mut buf);
    std::fs::write(&target.path, buf)?;
    Ok(target)
}

/// Installs `shell`'s completion to its conventional location and prints the
/// result, plus any follow-up the user needs.
fn install_completion(shell: Shell) -> anyhow::Result<()> {
    let home = dirs::home_dir()
        .ok_or_else(|| anyhow::anyhow!("could not determine your home directory"))?;
    let brew_prefix = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.canonicalize().ok())
        .and_then(|exe| homebrew_prefix_from(&exe));

    let target = install_completion_impl(shell, &home, brew_prefix.as_deref())?;
    eprintln!(
        "Installed {0} completion to: {1}",
        shell.name(),
        target.path.display()
    );
    if target.autoloaded {
        eprintln!(
            "Reload your shell (e.g. `exec {0}`) and Tab completion will be active.",
            shell.name()
        );
    } else if let Some(hint) = shell.follow_up(&target.path) {
        eprintln!("{hint}");
    }
    Ok(())
}

/// Interactively pick a shell to install completions for, defaulting the
/// highlight to `default` (usually the detected login shell). Returns `None`
/// when the user cancels (Esc / Ctrl-C).
fn prompt_shell(default: Option<Shell>) -> anyhow::Result<Option<Shell>> {
    use inquire::Select;
    use inquire::list_option::ListOption;

    let labels: Vec<&'static str> = Shell::ALL.iter().map(|s| Shell::name(*s)).collect();
    let highlight = default
        .and_then(|d| Shell::ALL.iter().position(|&s| s == d))
        .unwrap_or(0);

    let selection = Select::new("Install completions for which shell?", labels)
        .with_starting_cursor(highlight)
        .raw_prompt();
    Ok(inquire_opt(selection)?.map(|ListOption { index, .. }| Shell::ALL[index]))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    match cli.command {
        Some(Commands::Setup) => config::run_setup(),
        Some(Commands::List) => config::run_list(),
        Some(Commands::Update) => update::run_update(),
        Some(Commands::Resolve) => run_resolve_workflow().await,
        Some(Commands::Completion) => {
            // Interactive when stdout is a terminal; fall back to $SHELL
            // detection for scripts and pipes.
            let shell = if std::io::stdout().is_terminal() {
                match prompt_shell(detect_shell())? {
                    Some(shell) => shell,
                    None => {
                        eprintln!("Cancelled.");
                        return Ok(());
                    }
                }
            } else {
                detect_shell().ok_or_else(|| {
                    anyhow::anyhow!(
                        "couldn't detect your shell from $SHELL; run `aic completion` in a \
                         terminal to pick one (bash, zsh, fish, nushell)"
                    )
                })?
            };
            install_completion(shell)
        }
        None => run_commit_workflow().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_completion_emits_nonempty_script_naming_aic_for_every_shell() {
        for shell in Shell::ALL {
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
    fn from_name_maps_known_shells_and_rejects_unknown() {
        assert_eq!(Shell::from_name("zsh"), Some(Shell::Zsh));
        assert_eq!(Shell::from_name("bash"), Some(Shell::Bash));
        assert_eq!(Shell::from_name("fish"), Some(Shell::Fish));
        assert_eq!(Shell::from_name("nu"), Some(Shell::Nushell));
        assert_eq!(Shell::from_name("nushell"), Some(Shell::Nushell));
        assert_eq!(Shell::from_name("tcsh"), None);
        assert_eq!(Shell::from_name(""), None);
    }

    #[test]
    fn homebrew_prefix_matches_brew_locations_only() {
        use std::path::Path;
        assert_eq!(
            homebrew_prefix_from(Path::new("/opt/homebrew/bin/aic")),
            Some(Path::new("/opt/homebrew").to_path_buf())
        );
        assert_eq!(
            homebrew_prefix_from(Path::new("/usr/local/bin/aic")),
            Some(Path::new("/usr/local").to_path_buf())
        );
        assert_eq!(homebrew_prefix_from(Path::new("/usr/bin/aic")), None);
        assert_eq!(
            homebrew_prefix_from(Path::new("/home/me/.cargo/bin/aic")),
            None
        );
    }

    #[test]
    fn install_target_picks_autoloaded_dirs() {
        use std::path::Path;
        let home = Path::new("/home/me");

        // fish & bash: always autoloaded via their conventional dirs.
        let t = Shell::Fish.install_target(home, None);
        assert_eq!(
            t.path,
            Path::new("/home/me/.config/fish/completions/aic.fish")
        );
        assert!(t.autoloaded);

        let t = Shell::Bash.install_target(home, None);
        assert_eq!(
            t.path,
            Path::new("/home/me/.local/share/bash-completion/completions/aic")
        );
        assert!(t.autoloaded);

        // zsh under a Homebrew prefix: better location, but still not autoloaded
        // — the brew site-functions dir only loads if the user's zsh has it on
        // $fpath (Homebrew's own zsh yes, macOS system zsh no).
        let t = Shell::Zsh.install_target(home, Some(Path::new("/opt/homebrew")));
        assert_eq!(
            t.path,
            Path::new("/opt/homebrew/share/zsh/site-functions/_aic")
        );
        assert!(!t.autoloaded);

        // zsh elsewhere: XDG dir, needs the user to add it to $fpath.
        let t = Shell::Zsh.install_target(home, None);
        assert_eq!(
            t.path,
            Path::new("/home/me/.local/share/zsh/site-functions/_aic")
        );
        assert!(!t.autoloaded);

        // nushell: lands in its config dir but isn't autoloaded.
        let t = Shell::Nushell.install_target(home, None);
        assert_eq!(t.path, Path::new("/home/me/.config/nushell/aic.nu"));
        assert!(!t.autoloaded);
    }

    #[test]
    fn install_completion_impl_writes_a_nonempty_script_to_the_right_path() {
        let dir = tempfile::tempdir().expect("tempdir");

        // zsh (XDG fallback): file lands at the expected path and references aic.
        let target = install_completion_impl(Shell::Zsh, dir.path(), None).expect("install zsh");
        assert!(!target.autoloaded);
        assert!(
            target
                .path
                .ends_with(".local/share/zsh/site-functions/_aic")
        );
        let body = std::fs::read_to_string(&target.path).expect("read installed script");
        assert!(!body.is_empty());
        assert!(body.contains("aic"));

        // fish: autoloaded, distinct filename.
        let target = install_completion_impl(Shell::Fish, dir.path(), None).expect("install fish");
        assert!(target.autoloaded);
        assert!(target.path.ends_with(".config/fish/completions/aic.fish"));

        // nushell: installed to its config dir, not autoloaded.
        let target =
            install_completion_impl(Shell::Nushell, dir.path(), None).expect("install nushell");
        assert!(!target.autoloaded);
        assert!(target.path.ends_with(".config/nushell/aic.nu"));
        let body = std::fs::read_to_string(&target.path).expect("read installed script");
        assert!(!body.is_empty());
        assert!(body.contains("aic"));
    }

    /// `detect_shell` maps `$SHELL` (basename) to a supported shell; unknown
    /// names and an unset variable yield `None`, so the completion prompt can
    /// fall back to a manual pick. Uses `temp_env` to avoid unsafe env
    /// mutation racing other tests.
    #[test]
    fn detect_shell_reads_shell_env() {
        temp_env::with_var("SHELL", Some("/bin/zsh"), || {
            assert_eq!(detect_shell(), Some(Shell::Zsh));
        });
        temp_env::with_var("SHELL", Some("/usr/bin/bash"), || {
            assert_eq!(detect_shell(), Some(Shell::Bash));
        });
        temp_env::with_var("SHELL", Some("fish"), || {
            assert_eq!(detect_shell(), Some(Shell::Fish));
        });
        temp_env::with_var("SHELL", Some("nu"), || {
            assert_eq!(detect_shell(), Some(Shell::Nushell));
        });
        temp_env::with_var("SHELL", Some("/bin/tcsh"), || {
            assert_eq!(detect_shell(), None);
        });
        temp_env::with_var("SHELL", None::<&str>, || {
            assert_eq!(detect_shell(), None);
        });
    }
}
