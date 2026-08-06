pub mod cli;
pub mod config;
pub mod diff;
pub mod display;
pub mod generator;
pub mod git;
pub mod llm;
pub mod prompt;
pub mod retry;
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
use console::Key;
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

/// One action the user can take on the pre-commit confirmation menu (issue
/// #78). [`ConfirmMenu`] returns it; [`generate_and_commit`] translates it:
/// Commit lands the commit, Regenerate and Edit loop back to the menu
/// (re-showing the message), Abort ends the run with nothing further
/// committed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmChoice {
    Commit,
    Regenerate,
    Edit,
    Abort,
}

/// Erased confirmation menu: given the drafted subject, returns the user's
/// choice. Boxed for the same reason as [`Resolver`] — production wires it to
/// a terminal menu (issue #78), tests inject a scripted choice sequence.
pub(crate) type ConfirmMenu = Box<dyn Fn(&str) -> anyhow::Result<ConfirmChoice>>;

/// Erased message editor: takes the current (subject, body) and returns the
/// edited (subject, body) — the prior values unchanged when the user cancels
/// the edit. Boxed for the same reason as [`Resolver`] — production uses an
/// inline TUI when stdin is a TTY and `$VISUAL`/`$EDITOR` otherwise, tests
/// inject a stub.
pub(crate) type CommitEditor =
    Box<dyn Fn(&str, Option<&str>) -> anyhow::Result<(String, Option<String>)>>;

/// Opt-in pre-commit confirmation (issue #78): the gate plus the menu and
/// editor seams it needs, grouped so the workflow signatures stay within
/// clippy's argument budget. [`Confirm::disabled`] is the default — no menu,
/// generate-and-commit byte-for-byte as before the option existed.
pub(crate) struct Confirm {
    /// Gate: when `false`, `menu` and `editor` are never invoked.
    enabled: bool,
    /// Drafted subject → user choice (Commit / Re-generate / Edit / Abort).
    menu: ConfirmMenu,
    /// (subject, body) → edited (subject, body); unchanged when the user
    /// cancels the edit.
    editor: CommitEditor,
}

impl Confirm {
    /// Confirmation off — the seams are placeholders that must never run.
    pub(crate) fn disabled() -> Self {
        Self {
            enabled: false,
            menu: Box::new(|_| Ok(ConfirmChoice::Commit)),
            editor: Box::new(|s, b| Ok((s.to_string(), b.map(|b| b.to_string())))),
        }
    }

    /// Confirmation on, wired to the production menu and editor.
    pub(crate) fn interactive(menu: ConfirmMenu, editor: CommitEditor) -> Self {
        Self {
            enabled: true,
            menu,
            editor,
        }
    }
}

/// The pre-commit confirmation requires an interactive stdin: the menu
/// ([`confirm_menu`]) renders on stderr but reads keys from stdin, so a
/// non-TTY stdin leaves the menu unanswerable. Returns an error naming the
/// fix when confirmation is enabled but stdin is not a terminal — the guard
/// runs before any planning or staging, so the run fails cleanly instead of
/// aborting after the first batch is already staged (issue #78).
fn ensure_confirm_terminal(confirm_enabled: bool, stdin_tty: bool) -> anyhow::Result<()> {
    if confirm_enabled && !stdin_tty {
        anyhow::bail!(
            "confirm_before_commit is enabled but stdin is not a terminal — \
             run `aic` from a terminal, or turn the option off"
        );
    }
    Ok(())
}

/// Marker error for the user declining the pre-commit confirmation (issue
/// #78). Distinct from ordinary failures so each call site can translate it
/// into its own abort wording: the single-commit path reports "no commit
/// made", the batch loop reports how many batches already committed and that
/// the rest is recoverable.
#[derive(Debug)]
pub(crate) struct CommitDeclined;

impl std::fmt::Display for CommitDeclined {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "commit declined by user")
    }
}

impl std::error::Error for CommitDeclined {}

/// Run the batch-plan analysis behind a spinner that streams the model's
/// reasoning live. The reasoning is shown as a rolling
/// [`display::REASONING_WINDOW`]-row block that redraws in place as the
/// model thinks — newest rows at the bottom, oldest scrolled out of the
/// window — and is erased when thinking ends, so the reasoning never lingers
/// on screen or in the scrollback. The cap bounds the in-place block while
/// it streams, even when a line wraps long.
///
/// Rendering is hand-rolled via [`display::ReasoningRenderer`] rather than an
/// indicatif multi-line spinner: indicatif repaints by blanking every row then
/// redrawing them, and its steady tick forced that ~20×/s, so a multi-row
/// window flickered. The renderer clears and rewrites one row at a time (any
/// instant has at most one blank row) and repaints only on a reasoning change,
/// so the window is flicker-free. See [`display::ReasoningRenderer`] for the
/// redraw contract.
async fn analyze_changes(diff: &str) -> anyhow::Result<generator::BatchPlanOutput> {
    let mut view = display::ThinkingView::new();
    let mut renderer = display::ReasoningRenderer::new("Analyzing changes");

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

/// Run the confirmation loop for a drafted message. Returns the confirmed
/// (message, body, preview_rows) after the user approves or edits it. The
/// preview is shown after each edit/regeneration, and each preview is erased
/// before being replaced so superseded drafts never accumulate on screen.
async fn confirm_draft(
    draft: (String, Option<String>),
    paths: &[String],
    display: &Display,
    confirm: &Confirm,
    messenger: &CommitMessenger,
    diff: String,
) -> anyhow::Result<(String, Option<String>, usize)> {
    let (mut message, mut body) = draft;

    if !confirm.enabled {
        return Ok((message, body, 0));
    }

    loop {
        let rows = display.commit_preview(&message, body.as_deref(), paths);
        match (confirm.menu)(&message)? {
            ConfirmChoice::Commit => return Ok((message, body, rows)),
            ConfirmChoice::Regenerate => {
                display.clear_last(rows);
                let result =
                    display::with_spinner("Regenerating message", messenger(diff.clone())).await?;
                message = result.message;
                body = result.body;
            }
            ConfirmChoice::Edit => {
                display.clear_last(rows);
                (message, body) = (confirm.editor)(&message, body.as_deref())?;
            }
            ConfirmChoice::Abort => return Err(CommitDeclined.into()),
        }
    }
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
        display::with_spinner("Generating commit message", messenger(diff_str.clone())).await?;
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
                match display::with_spinner(&label, resolve_ref(content)).await {
                    Ok(resolved) if !git::has_conflict_markers(&resolved) => Ok(resolved),
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
                anyhow::bail!(
                    "aborted on batch {} of {} after {} batch(es) committed.\n\
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
    let menu: ConfirmMenu = Box::new(confirm_menu);
    let editor: CommitEditor = Box::new(edit_message);
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
        Confirm::interactive(menu, editor)
    } else {
        Confirm::disabled()
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

/// Production confirmation menu (issue #78): a dialoguer `Select` over the
/// four actions, matching the setup wizard's arrow-key UI. The drafted
/// subject rides in the prompt so the menu is self-describing even if the
/// preview above scrolled away. Esc (and `q`, dialoguer's quit key) abort —
/// there is nothing to go back to once the commit is pending — and Ctrl-C
/// ends the process the same way it does everywhere else in the wizard.
fn confirm_menu(message: &str) -> anyhow::Result<ConfirmChoice> {
    use dialoguer::{Select, theme::ColorfulTheme};

    let items = ["Commit", "Re-generate", "Edit", "Abort"];
    let mut subject: String = message.chars().take(40).collect();
    if message.chars().count() > 40 {
        subject.push('…');
    }

    let choice = Select::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("Commit this message?  ({subject})"))
        .items(items)
        .default(0)
        // No `✔ ...` echo line after the choice: the preview above is erased
        // by the caller after a commit, and a leftover selection line would
        // make that erase imprecise (and linger as residue).
        .report(false)
        .interact_opt()
        .context("could not read terminal input")?;
    Ok(match choice {
        Some(0) => ConfirmChoice::Commit,
        Some(1) => ConfirmChoice::Regenerate,
        Some(2) => ConfirmChoice::Edit,
        // Some(3) is Abort; None is Esc — both end the run.
        _ => ConfirmChoice::Abort,
    })
}

/// Production message editor (issue #78): an inline raw-mode editor when
/// stdin is a TTY, `$VISUAL`/`$EDITOR` on a temp file otherwise (git-style).
fn edit_message(subject: &str, body: Option<&str>) -> anyhow::Result<(String, Option<String>)> {
    if std::io::stdin().is_terminal() {
        edit_message_inline(subject, body)
    } else {
        edit_message_external(subject, body)
    }
}

/// The text an editor edits: the subject line, then the body (outer-whitespace
/// trimmed) on following lines. Shared by both editor paths so what the user
/// sees in the editor is exactly the (subject, body) pair that would commit.
fn message_to_edit(subject: &str, body: Option<&str>) -> String {
    let mut text = subject.to_string();
    if let Some(b) = body {
        let trimmed = b.trim();
        if !trimmed.is_empty() {
            text.push('\n');
            text.push_str(trimmed);
        }
    }
    text
}

/// Non-TTY fallback for [`edit_message`]: spawn `$VISUAL` (then `$EDITOR`,
/// git's order) on a temp file containing the current message, and read the
/// edited content back. The subject is the first line, the body the rest. An
/// editor that fails to launch or exits non-zero is an error — a message the
/// editor never saved must not silently commit — matching git's behavior of
/// aborting on a broken editor.
///
/// `$VISUAL`/`$EDITOR` are split shell-style ([`split_command`]) so the common
/// `EDITOR="code --wait"` form works without invoking a shell, and the temp
/// file is created with exclusive semantics ([`tempfile::NamedTempFile`]) so a
/// file planted in the shared temp dir is never followed or overwritten, and
/// it is removed on every exit path.
fn edit_message_external(
    subject: &str,
    body: Option<&str>,
) -> anyhow::Result<(String, Option<String>)> {
    let editor = std::env::var("VISUAL")
        .ok()
        .or_else(|| std::env::var("EDITOR").ok())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "stdin is not a TTY and neither $VISUAL nor $EDITOR is set — \
                 cannot open the commit message in an editor"
            )
        })?;
    let parts = split_command(&editor);
    let (program, args) = parts
        .split_first()
        .ok_or_else(|| anyhow::anyhow!("$VISUAL/$EDITOR is empty"))?;

    // Rebuild the message exactly as the messenger-shaped (subject, body)
    // pair would commit, then give the editor a trailing newline to chew on.
    let mut text = message_to_edit(subject, body);
    text.push('\n');

    // Exclusive-create (never follows a pre-planted symlink) and remove-on-
    // drop, so a failed editor cannot strand a stale file in the temp dir.
    let mut file = tempfile::Builder::new()
        .prefix("aic-commit-msg-")
        .suffix(".txt")
        .tempfile()
        .context("could not create a temp file for the commit message")?;
    use std::io::Write as _;
    file.write_all(text.as_bytes())
        .with_context(|| format!("could not write {}", file.path().display()))?;
    let path = file.path().to_path_buf();

    let status = match std::process::Command::new(program)
        .args(args)
        .arg(&path)
        .status()
    {
        Ok(status) => status,
        Err(e) => return Err(e).with_context(|| format!("failed to launch editor {:?}", program)),
    };
    if !status.success() {
        anyhow::bail!("editor {:?} exited with {status}", program);
    }

    // Read by path: an editor that replaces the file via rename (vim-style)
    // leaves the new content at `path`, which is what we want to commit.
    let edited = std::fs::read_to_string(&path)
        .with_context(|| format!("could not read back {}", path.display()))?;
    // Dropping `file` removes the temp file (even the renamed replacement).

    let mut lines = edited.trim_end_matches('\n').splitn(2, '\n');
    let new_subject = lines.next().unwrap_or("").to_string();
    // Collapse leading blank lines git-style, so a canonical
    // "subject\n\nbody" edit round-trips to the same (subject, body) pair.
    let new_body = lines.next().map(|s| s.trim_start_matches('\n').to_string());
    Ok((new_subject, new_body))
}

/// Split a `$VISUAL`/`$EDITOR` command line into program + arguments without
/// invoking a shell: whitespace separates tokens, single/double quotes group
/// tokens (the quotes are removed), and a backslash escapes the next
/// character — literal inside single quotes, as in POSIX shells. An
/// unterminated quote consumes the rest of the string. This makes the common
/// `EDITOR="code --wait"` and quoted paths work while keeping the editor a
/// plain argv, never a shell string.
fn split_command(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some('\'') => current.push(c),
            Some(_) => {
                // Double-quoted: backslash escapes the next char.
                if c == '\\' {
                    match chars.peek() {
                        Some(&next) => {
                            current.push(next);
                            chars.next();
                        }
                        None => current.push('\\'),
                    }
                } else {
                    current.push(c);
                }
            }
            None => match c {
                '\'' | '"' => quote = Some(c),
                '\\' => match chars.peek() {
                    Some(&next) => {
                        current.push(next);
                        chars.next();
                    }
                    None => current.push('\\'),
                },
                c if c.is_whitespace() => {
                    if !current.is_empty() {
                        args.push(std::mem::take(&mut current));
                    }
                }
                c => current.push(c),
            },
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// What a keypress did to an [`EditBuffer`]: ended the session (saved or
/// cancelled) or kept editing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditOutcome {
    Save,
    Cancel,
    Continue,
}

/// The pure, I/O-free state of one inline editing session: the message lines
/// plus the cursor (line index, char index within the line). Kept separate
/// from the terminal rendering ([`render_editor`]) so every key transition is
/// unit-testable without a TTY (issue #78 review).
struct EditBuffer {
    /// Logical lines of the message being edited (subject first, then body).
    lines: Vec<String>,
    /// 0-based index of the logical line the cursor is on.
    row: usize,
    /// Char index of the cursor within `lines[row]` (byte offsets are derived
    /// at edit time so multi-byte chars never split).
    col: usize,
    /// Display-column wrap budget for one wrapped piece — Up/Down navigation
    /// moves across *visual* (wrapped) rows, so it needs the same budget the
    /// renderer wraps with.
    width: usize,
}

impl EditBuffer {
    /// Start editing the drafted message with the cursor at the end of the
    /// last line. `width` is the display-column wrap budget used both to lay
    /// out the textarea and to navigate it.
    fn new(subject: &str, body: Option<&str>, width: usize) -> Self {
        let mut lines: Vec<String> = message_to_edit(subject, body)
            .lines()
            .map(|l| l.to_string())
            .collect();
        if lines.is_empty() {
            lines.push(String::new());
        }
        let row = lines.len() - 1;
        let col = lines[row].chars().count();
        EditBuffer {
            lines,
            row,
            col,
            width,
        }
    }

    /// Apply one key. Returns `Save`/`Cancel` for the keys that end the
    /// session; everything else mutates the buffer and returns `Continue`.
    ///
    /// Up/Down move across *visual* (wrapped) rows and keep the display
    /// column — a long subject line that wraps to two rows is walked row by
    /// row, not skipped as one jump. Left/Right/Home/End stay char-level
    /// (they already cross wrap boundaries correctly, and the caret renders
    /// on the right visual row).
    fn key(&mut self, key: Key) -> EditOutcome {
        match key {
            // Ctrl-S saves (raw mode surfaces it as a control char, not a
            // named key).
            Key::Char('\u{13}') => EditOutcome::Save,
            Key::Escape | Key::CtrlC => EditOutcome::Cancel,
            Key::Char(c) if !c.is_control() => {
                // `col` counts chars; `String::insert` is byte-indexed, so
                // multi-byte chars need the offset converted (a naive `col +=
                // 1` desyncs the cursor after the first non-ASCII char).
                let b = char_to_byte(&self.lines[self.row], self.col);
                self.lines[self.row].insert(b, c);
                self.col += 1;
                EditOutcome::Continue
            }
            Key::Enter => {
                let b = char_to_byte(&self.lines[self.row], self.col);
                let rest = self.lines[self.row].split_off(b);
                self.row += 1;
                self.lines.insert(self.row, rest);
                self.col = 0;
                EditOutcome::Continue
            }
            Key::Backspace => {
                if self.col > 0 {
                    let b = char_to_byte(&self.lines[self.row], self.col - 1);
                    self.lines[self.row].remove(b);
                    self.col -= 1;
                } else if self.row > 0 {
                    let prev_len = self.lines[self.row - 1].chars().count();
                    let tail = self.lines.remove(self.row);
                    self.row -= 1;
                    self.lines[self.row].push_str(&tail);
                    self.col = prev_len;
                }
                EditOutcome::Continue
            }
            Key::Del => {
                if self.col < self.lines[self.row].chars().count() {
                    let b = char_to_byte(&self.lines[self.row], self.col);
                    self.lines[self.row].remove(b);
                } else if self.row + 1 < self.lines.len() {
                    let tail = self.lines.remove(self.row + 1);
                    self.lines[self.row].push_str(&tail);
                }
                EditOutcome::Continue
            }
            Key::ArrowLeft => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.lines[self.row].chars().count();
                }
                EditOutcome::Continue
            }
            Key::ArrowRight => {
                if self.col < self.lines[self.row].chars().count() {
                    self.col += 1;
                } else if self.row + 1 < self.lines.len() {
                    self.row += 1;
                    self.col = 0;
                }
                EditOutcome::Continue
            }
            Key::ArrowUp => {
                let (vr, vc) = visual_pos(&self.lines, self.row, self.col, self.width);
                if vr > 0 {
                    (self.row, self.col) = from_visual(&self.lines, vr - 1, vc, self.width);
                }
                EditOutcome::Continue
            }
            Key::ArrowDown => {
                let (vr, vc) = visual_pos(&self.lines, self.row, self.col, self.width);
                if vr + 1 < total_visual_rows(&self.lines, self.width) {
                    (self.row, self.col) = from_visual(&self.lines, vr + 1, vc, self.width);
                }
                EditOutcome::Continue
            }
            Key::Home => {
                self.col = 0;
                EditOutcome::Continue
            }
            Key::End => {
                self.col = self.lines[self.row].chars().count();
                EditOutcome::Continue
            }
            _ => EditOutcome::Continue,
        }
    }

    /// The (subject, body) the buffer holds right now: first line is the
    /// subject, the rest the body. Leading and trailing blank body lines are
    /// collapsed git-style, matching [`edit_message_external`] so both editor
    /// paths round-trip to the same pair.
    fn message(&self) -> (String, Option<String>) {
        let subject = self.lines.first().cloned().unwrap_or_default();
        let body = self.lines[1..].join("\n");
        let body = body.trim_matches('\n').to_string();
        if body.is_empty() {
            (subject, None)
        } else {
            (subject, Some(body))
        }
    }
}

/// One frame of the inline editor: the text to write to the terminal, the
/// physical cursor position within that text, and how many rows the frame
/// occupies. Pure — built from the buffer and viewport without any terminal
/// I/O — so layout (including wide-char wrapping) is testable.
struct EditorRender {
    block: String,
    cursor_row: usize,
    cursor_col: usize,
    region: usize,
}

/// Lay out the visible slice of the buffer as a bordered textarea with the
/// subject and body in separate labeled sections:
///
/// ```text
///   ┌─ subject ───────────────────────┐
///   │  feat: add oauth                │
///   ├─ body ──────────────────────────┤
///   │  detailed description           │
///   └─────────────────────────────────┘
///   Ctrl-S = save · Esc/Ctrl-C = cancel
/// ```
///
/// `top`/`window` slice which logical lines are visible (scrolling), `pw` is
/// the display-column budget for one wrapped piece (the box interior is
/// `pw + 1` columns). The cursor is reported in physical coordinates inside
/// the returned block.
fn render_editor(
    lines: &[String],
    row: usize,
    col: usize,
    top: usize,
    window: usize,
    pw: usize,
) -> EditorRender {
    let inner = pw + 1; // box interior between the borders
    let mut block = String::new();
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;
    let mut phys = 0usize;

    // Top border labels the visible section: "subject" while the first line
    // is on screen, "body" once the subject scrolled off.
    let header_label = if top == 0 { "subject" } else { "body" };
    block.push_str(&format!(
        "  ┌─ {header_label} {}┐\n",
        "─".repeat(inner.saturating_sub(header_label.len() + 3))
    ));
    phys += 1;

    let mut divider = false;
    for (li, line) in lines.iter().enumerate() {
        if li < top || li >= top + window {
            continue;
        }
        let pieces = edit_wrap(line, pw);
        // Divider before the first visible body line — only when the subject
        // is still on screen (top == 0); a scrolled-in view of just the body
        // doesn't need a second "body" label.
        if top == 0 && li >= 1 && !divider {
            block.push_str(&format!(
                "  ├─ body {}┤\n",
                "─".repeat(inner.saturating_sub(7))
            ));
            phys += 1;
            divider = true;
        }
        if li == row {
            // The wrapped piece holding the cursor, plus the display width
            // before it — so the cursor column is right even when the line
            // wraps mid-text or contains wide (CJK) characters. The box adds
            // margin(2) + border(1) + leading space(1) before the piece.
            let (piece, _, vc) = piece_containing(line, col, pw);
            cursor_row = phys + piece;
            cursor_col = 4 + vc;
        }
        for p in &pieces {
            block.push_str(&format!("  │ {p}"));
            block.push_str(&" ".repeat(pw.saturating_sub(console::measure_text_width(p))));
            block.push_str("│\n");
            phys += 1;
        }
    }

    block.push_str(&format!("  └{}┘\n", "─".repeat(inner)));
    phys += 1;
    block.push_str("  Ctrl-S = save · Esc/Ctrl-C = cancel\n");
    phys += 1;

    EditorRender {
        block,
        cursor_row,
        cursor_col,
        region: phys,
    }
}

/// Display width of a single char (wide CJK chars count as two columns).
fn char_width(c: char) -> usize {
    console::measure_text_width(&c.to_string())
}

/// Display width (terminal columns) of the first `col` chars of `line` —
/// wide (CJK) characters count as two columns, so the cursor column tracks
/// the terminal, not the char count.
fn char_prefix_width(line: &str, col: usize) -> usize {
    line.chars().take(col).map(char_width).sum()
}

/// Char index whose boundary sits at or before display column `target` —
/// the inverse of [`char_prefix_width`], clamped to the line end. Used to
/// land the cursor at the column requested by an Up/Down arrow.
fn chars_up_to_width(line: &str, target: usize) -> usize {
    let mut width = 0usize;
    let mut count = 0usize;
    for ch in line.chars() {
        let w = char_width(ch);
        if width + w > target {
            break;
        }
        width += w;
        count += 1;
    }
    count
}

/// Visual (wrapped) position of `(row, col)`: `vr` counts wrapped pieces
/// across every line, `vc` is the display column within that piece.
fn visual_pos(lines: &[String], row: usize, col: usize, width: usize) -> (usize, usize) {
    let mut vr = 0usize;
    for (li, line) in lines.iter().enumerate() {
        let pieces = edit_wrap(line, width);
        if li == row {
            let (piece, _, vc) = piece_containing(line, col, width);
            return (vr + piece, vc);
        }
        vr += pieces.len();
    }
    // `row` is always in range; defensive fallback to the last line.
    let last = lines.len().saturating_sub(1);
    (
        vr.saturating_sub(1),
        lines.get(last).map(|l| l.chars().count()).unwrap_or(0),
    )
}

/// The wrapped piece of `line` containing char `col`, the display width
/// before that piece, and the display column of `col` within it. A boundary
/// (`col ==` a piece's end) belongs to the *next* piece — so a caret between
/// two wrapped rows renders at the start of the lower row — except at the
/// line end, where it belongs to the last piece at its full width.
fn piece_containing(line: &str, col: usize, width: usize) -> (usize, usize, usize) {
    let pieces = edit_wrap(line, width);
    let mut piece = pieces.len().saturating_sub(1);
    let mut width_before = 0usize;
    let mut consumed = 0usize;
    for (pi, p) in pieces.iter().enumerate() {
        let end = consumed + p.chars().count();
        if col < end {
            piece = pi;
            break;
        }
        if pi + 1 == pieces.len() {
            piece = pi;
            break;
        }
        consumed = end;
        width_before += console::measure_text_width(p);
    }
    let vc = char_prefix_width(line, col).saturating_sub(width_before);
    (piece, width_before, vc)
}

/// `(row, col)` at visual position `(vr, vc)`. `vc` is clamped to the target
/// row's width — a too-large `vc` (e.g. `usize::MAX` for "end of row") lands
/// at the row's end.
fn from_visual(lines: &[String], vr: usize, vc: usize, width: usize) -> (usize, usize) {
    let mut accum = 0usize;
    for (li, line) in lines.iter().enumerate() {
        let pieces = edit_wrap(line, width);
        if vr < accum + pieces.len() {
            let piece = vr - accum;
            let width_before: usize = pieces[..piece]
                .iter()
                .map(|p| console::measure_text_width(p))
                .sum();
            return (li, chars_up_to_width(line, width_before + vc));
        }
        accum += pieces.len();
    }
    // `vr` is always in range; defensive fallback to the last line's end.
    let last = lines.len().saturating_sub(1);
    (
        last,
        lines.get(last).map(|l| l.chars().count()).unwrap_or(0),
    )
}

/// Total visual (wrapped) rows across every line.
fn total_visual_rows(lines: &[String], width: usize) -> usize {
    lines.iter().map(|l| edit_wrap(l, width).len()).sum()
}

/// TTY path of [`edit_message`]: a raw-mode multi-line editor built on the
/// same `console` primitives as the setup wizard's text inputs. The message
/// renders as a bordered textarea on stderr (the Display stream — stdout stays
/// clean for piped output) with the subject and body in separate labeled
/// sections, and the region redraws in place on every keystroke:
///
/// - printable chars insert at the cursor; Enter splits the line
/// - Backspace / Del / arrow keys / Home / End move and delete as expected
///   (Up/Down walk visual rows, so wrapped lines are traversed row by row)
/// - **Ctrl-S** saves and returns the edited (subject, body)
/// - **Esc / Ctrl-C** cancels and returns the prior message unchanged
///
/// Lines longer than the terminal wrap to the box width (display-width aware,
/// so CJK text wraps correctly); the caret tracks the wrapped position, and
/// the window scrolls to keep it visible. The edit logic lives in
/// [`EditBuffer`] and the layout in [`render_editor`] — both pure — so this
/// function only shuttles frames between the terminal and the state machine.
fn edit_message_inline(
    subject: &str,
    body: Option<&str>,
) -> anyhow::Result<(String, Option<String>)> {
    use console::Term;

    let original = (subject.to_string(), body.map(|b| b.to_string()));

    let term = Term::stderr();
    let (rows, cols) = term.size();
    let (rows, cols) = (rows as usize, cols as usize);
    // Box interior budget: margin(2) + left border(1) + leading space(1) +
    // piece(pw) + right border(1) must fit the terminal width.
    let pw = cols.saturating_sub(5).max(1);
    // Editing window: terminal height minus the header/footer/hint rows.
    let window = rows.saturating_sub(3).max(1);
    let mut buf = EditBuffer::new(subject, body, pw);

    // Viewport state: which logical line the window starts at, how many rows
    // have been drawn so far (only grows, so a shrinking message never
    // strands stale rows below), and where the caret was parked after the
    // last frame — needed to return to the bottom of the drawn block before
    // clearing it.
    let mut top = 0usize;
    let mut drawn = 0usize;
    let mut parked_row = 0usize;

    let saved = loop {
        // Keep the cursor row inside the window, scrolling the view as it
        // crosses an edge.
        if buf.row < top {
            top = buf.row;
        } else if buf.row >= top + window {
            top = buf.row + 1 - window;
        }

        let r = render_editor(&buf.lines, buf.row, buf.col, top, window, pw);

        // Redraw: hide the caret, return to the bottom of the previously
        // drawn block and clear upward from there (`clear_last_lines` erases
        // the n lines above the cursor, so starting at the block's bottom
        // erases exactly the block — never the preview above it), then write
        // the new frame and park the caret on the edit position.
        let _ = term.hide_cursor();
        if drawn > 0 {
            let _ = term.move_cursor_down(drawn.saturating_sub(parked_row));
            let _ = term.clear_last_lines(drawn);
        }
        let _ = term.write_str(&r.block);
        for _ in r.region..drawn {
            let _ = term.write_line("");
        }
        drawn = drawn.max(r.region);

        let _ = term.move_cursor_up(drawn - r.cursor_row);
        let _ = term.move_cursor_right(r.cursor_col);
        parked_row = r.cursor_row;
        let _ = term.show_cursor();

        let key = match term.read_key_raw() {
            Ok(key) => key,
            Err(e) => {
                // Leave the terminal as we found it — a hidden caret would
                // linger after the error propagates.
                let _ = term.show_cursor();
                return Err(e).context("could not read keypress");
            }
        };
        match buf.key(key) {
            EditOutcome::Save => break true,
            EditOutcome::Cancel => break false,
            EditOutcome::Continue => {}
        }
    };
    let _ = term.show_cursor();
    // Clear the editor's own frame so a saved/cancelled edit never leaves a
    // stale block behind — the re-preview (or next output) starts clean.
    let _ = term.clear_last_lines(drawn);

    if saved {
        Ok(buf.message())
    } else {
        Ok(original)
    }
}

/// Byte offset of the char at char-index `col` in `line` (or `line.len()` when
/// `col` is past the end). The editor tracks the cursor as a char index and
/// converts here for the byte-indexed `String` operations.
fn char_to_byte(line: &str, col: usize) -> usize {
    line.char_indices()
        .nth(col)
        .map(|(i, _)| i)
        .unwrap_or(line.len())
}

/// Split `line` into display pieces of at most `w` terminal columns,
/// preserving char boundaries and treating wide (CJK) chars as two columns. A
/// `w` of 0 — pathological terminal — returns the line whole. An empty line
/// yields one empty piece so the editor's physical-row accounting always sees
/// at least one row per logical line.
fn edit_wrap(line: &str, w: usize) -> Vec<String> {
    if w == 0 {
        return vec![line.to_string()];
    }
    let mut pieces = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;
    for ch in line.chars() {
        let ch_width = console::measure_text_width(&ch.to_string());
        if current_width + ch_width > w && !current.is_empty() {
            pieces.push(std::mem::take(&mut current));
            current_width = 0;
        }
        current.push(ch);
        current_width += ch_width;
    }
    if !current.is_empty() || pieces.is_empty() {
        pieces.push(current);
    }
    pieces
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
    use dialoguer::{Select, theme::ColorfulTheme};

    let labels = Shell::ALL.map(Shell::name);
    let highlight = default
        .and_then(|d| Shell::ALL.iter().position(|&s| s == d))
        .unwrap_or(0);

    let selection = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Install completions for which shell?")
        .items(labels)
        .default(highlight)
        .interact_opt()?;
    Ok(selection.map(|i| Shell::ALL[i]))
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

    /// Confirmation off, or an interactive stdin, always passes; confirmation
    /// on with a non-TTY stdin fails fast with a message naming the fix —
    /// before any planning or staging happens.
    #[test]
    fn ensure_confirm_terminal_guards_non_tty_stdin() {
        assert!(ensure_confirm_terminal(false, false).is_ok());
        assert!(ensure_confirm_terminal(false, true).is_ok());
        assert!(ensure_confirm_terminal(true, true).is_ok());

        let err = ensure_confirm_terminal(true, false).expect_err("must refuse non-TTY stdin");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stdin is not a terminal"),
            "expected a clear non-TTY error, got: {msg}"
        );
        assert!(
            msg.contains("run `aic` from a terminal"),
            "expected the fix to be named, got: {msg}"
        );
    }

    /// The `edit` crate's temp-file editor honors `$EDITOR` arguments before
    /// the file path (the `code --wait` case). Verified with a fake editor.
    #[cfg(unix)]
    #[test]
    fn edit_message_honors_editor_arguments() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-editor.sh");
        // Fake editor: the file path is the last argument; rewrite it in place.
        std::fs::write(
            &script,
            "#!/bin/sh\nfor last; do :; done\nprintf 'fix: args\\n' > \"$last\"\n",
        )
        .unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let editor = format!("{} --wait", script.display());
        temp_env::with_vars(
            [("VISUAL", None), ("EDITOR", Some(editor.as_str()))],
            || {
                let (subject, body) = edit_message("feat: draft", None).unwrap();
                assert_eq!(subject, "fix: args");
                assert_eq!(body, None);
            },
        );
    }
}
