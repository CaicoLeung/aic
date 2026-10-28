//! The setup wizard's Verify probes (R3): the API-provider and CLI-agent
//! sample requests that run automatically when a backend sub-flow completes,
//! plus the smoke check that a configured program exists and the failure
//! screen (Edit / Retry / Save anyway) a failed probe offers.

use super::finalize::mask_key;
use super::*;
use crate::core::config::{ResolvedConfig, resolve_base_url, resolve_field};
use crate::render::layout::MARGIN;
use crate::render::oneshot::DETAIL_INDENT;
use crate::render::{oneshot, palette};
use std::time::Duration;

/// Best-effort check that `program` is installed. Runs `program --version`
/// with **stdin detached** and a hard 3 s cap, so a misconfigured custom CLI
/// that ignores `--version` and tries to read stdin or enter an interactive
/// loop cannot hang `aic setup`. Never blocks longer than the cap; a miss or
/// timeout yields a warning the user can ignore (ADR 0010 — aic never installs
/// or authenticates a CLI on the user's behalf). Returns a human one-liner.
pub(super) fn smoke_check(program: &str) -> String {
    const SMOKE_TIMEOUT: Duration = Duration::from_secs(3);
    let mut cmd = match std::process::Command::new(program)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return format!(
                "{ICON_ALERT}  `{program}` not found on $PATH — install + authenticate it before using aic"
            );
        }
        Err(_) => return format!("{ICON_ALERT}  could not verify `{program}`"),
    };
    let deadline = std::time::Instant::now() + SMOKE_TIMEOUT;
    loop {
        match cmd.try_wait() {
            Ok(Some(status)) if status.success() => {
                return format!("{ICON_CHECK} `{program}` found");
            }
            Ok(Some(_)) => {
                return format!(
                    "{ICON_ALERT}  `{program}` ran but `--version` exited non-zero — it may still work"
                );
            }
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                // Exceeded the cap: kill + reap the child, then report.
                let _ = cmd.kill();
                let _ = cmd.wait();
                return format!(
                    "{ICON_ALERT}  `{program}` did not respond to `--version` within 3s — it may not support print mode, or is not installed"
                );
            }
            Err(_) => return format!("{ICON_ALERT}  could not verify `{program}`"),
        }
    }
}
/// Wait for Enter so a smoke-check / status message stays visible before the
/// screen redraws. Best-effort: a non-interactive stdin just continues.
pub(super) fn pause_done() -> Result<()> {
    use std::io::BufRead;
    eprint!("\n{MARGIN}{DETAIL_INDENT}Press Enter to continue… ");
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    Ok(())
}

/// Drive `fut` under a spinner label on a dedicated current-thread Tokio
/// runtime. `block_in_place` parks the outer multi-thread task; the nested
/// current-thread runtime then drives the async future. Without
/// `block_in_place`, `Runtime::block_on` panics inside an active runtime.
/// Shared by the Verify probes and the wizard's model-listing fetch
/// (ADR 0019), so every setup-time network touch runs the same way.
pub(super) fn probe<T>(label: &str, fut: impl Future<Output = Result<T>>) -> Result<T> {
    tokio::task::block_in_place(|| -> Result<T> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build verify runtime")?;
        rt.block_on(crate::render::progress::with_spinner(label, fut))
    })
}
/// The API-provider probe behind R3 auto-verify (AIC-23): a minimal sample
/// request against the selected provider using the **effective** draft
/// values — exactly the values the sub-menu rows show. Returns the model's
/// trimmed reply, or the underlying error (validation setup-hint, auth,
/// rate limit, network, unknown model). No UI: the caller decides which
/// screen an outcome gets.
///
/// The sample call runs on a dedicated current-thread Tokio runtime. `aic
/// setup` is dispatched from `#[tokio::main]`, so the wizard is already
/// executing inside a runtime; `block_in_place` parks the main task on the
/// multi-thread runtime so a nested runtime can drive the async verify call
/// without panicking.
pub(super) fn probe_provider(draft: &Draft) -> Result<String> {
    let p = draft.provider.unwrap_or_default();
    // Effective values: the draft (a user edit or the seeded config value).
    // There is no default model (ADR 0019) — an empty one fails `validate`
    // below with the setup hint.
    let api_key = resolve_field(draft.api_key.as_deref().filter(|k| !k.is_empty()), "").0;
    let base_url = resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p).0;
    let model = resolve_field(draft.model.as_deref().filter(|m| !m.is_empty()), "").0;
    let resolved = ResolvedConfig::from_parts(
        p.name().to_string(),
        api_key,
        model.clone(),
        base_url.clone(),
    );

    // The same validation the Run path runs (`LlmConfig::load`), so a missing
    // required field reads as a setup hint, not an opaque provider error.
    resolved.validate()?;

    let llm = resolved.to_llm();
    let label = format!("Contacting {} ({model})…", p.display());
    probe(&label, async {
        llm.agent("You are a connectivity checker. Follow the user's instruction exactly.")
            .verify()
            .await
    })
}

/// The outcome block shared by both Verify screens: the success / failure
/// line, the trimmed reply, the error plus its common causes, and the pause
/// before the sub-menu redraws. Callers print their own headers first.
fn show_outcome(
    term: &Term,
    result: Result<String>,
    ok: &str,
    failed: &str,
    causes: &[&str],
) -> Result<()> {
    match result {
        Ok(reply) => {
            oneshot::stderr_line(&format!("{ICON_CHECK} {ok}"), palette::success());
            if !reply.is_empty() {
                oneshot::stderr_detail(&format!("Reply: {reply}"), palette::muted());
            }
        }
        Err(e) => {
            oneshot::stderr_line(&format!("{ICON_CROSS} {failed}"), palette::failure());
            oneshot::stderr_detail(&format!("Error: {e}"), palette::muted());
            eprintln!();
            for cause in causes {
                oneshot::stderr_detail(cause, palette::muted());
            }
        }
    }
    eprintln!();
    oneshot::stderr_detail("Press Enter to return to the menu…", palette::muted());
    let _ = term.read_char();
    Ok(())
}

/// Render the Verify result on a fresh screen and pause for a keypress so the
/// user can read it before the sub-menu redraws. `result` carries the model's
/// trimmed reply on success, or the propagated error on failure.
fn show_verify_result(
    p: &Provider,
    model: &str,
    api_key: &str,
    base_url: Option<&str>,
    result: Result<String>,
) -> Result<()> {
    let term = Term::stderr();
    term.clear_screen()?;
    term.move_cursor_to(0, 0)?;
    oneshot::stderr_line(
        &format!("Verify — {} ({model})", p.display()),
        palette::plain(),
    );
    oneshot::stderr_detail(
        &format!(
            "API key:  {}",
            if api_key.is_empty() {
                "(none)".to_string()
            } else {
                mask_key(api_key)
            }
        ),
        palette::muted(),
    );
    oneshot::stderr_detail(
        &format!("Base URL: {}", base_url.unwrap_or("(none)")),
        palette::muted(),
    );
    eprintln!();
    show_outcome(
        &term,
        result,
        "Success — the provider responded.",
        "Failed — the provider did not accept the request.",
        &["Common causes: wrong API key, model name, base URL, or network."],
    )
}

/// The CLI analogue of [`probe_provider`] (AIC-23): probe the configured CLI
/// with a minimal prompt using the **effective** draft values, so a missing
/// binary or an unauthenticated CLI is caught at setup time rather than
/// failing mid-Run. The CLI runs in headless/print mode; the probe sends
/// "Reply with exactly: OK" and checks for a reply. Install / auth /
/// timeout errors surface as the matching
/// [`LlmError`](crate::llm::cli_agent::LlmError). No UI.
///
/// Runs on a dedicated current-thread runtime like [`probe_provider`].
pub(super) fn probe_cli(draft: &Draft) -> Result<String> {
    if draft.active_cli_command().is_none() {
        // Defensive: callers only auto-verify once a command is set.
        anyhow::bail!("no CLI command is set yet");
    }
    // verify decodes the CLI's stdout envelope with the right decoder. The
    // prior hand-built spec hardcoded `Encoding::Plain`, so a claude-preset
    // verify (whose argv carries `--output-format stream-json`) rendered raw
    // NDJSON events as the reply instead of `OK`. Sharing `to_spec` with
    // `LlmConfig::load` means setup and run-time can no longer disagree on
    // the decoder.
    let spec = draft.cli.to_spec();
    let label = format!("Probing `{}` (print mode)…", spec.command);
    let agent = crate::llm::cli_agent::CliAgent::new(
        spec,
        "You are a connectivity checker. Follow the user's instruction exactly.".to_string(),
    );
    probe(&label, async { agent.verify().await })
}

/// Render the CLI Verify result on a fresh screen and pause for a keypress,
/// mirroring [`show_verify_result`] for the API path. `result` carries the
/// trimmed reply on success, or the propagated
/// [`LlmError`](crate::llm::cli_agent::LlmError) on failure (its `Display` already
/// carries a human hint).
fn show_cli_verify_result(result: Result<String>) -> Result<()> {
    let term = Term::stderr();
    term.clear_screen()?;
    term.move_cursor_to(0, 0)?;
    oneshot::stderr_line("Verify — CLI agent", palette::plain());
    eprintln!();
    show_outcome(
        &term,
        result,
        "Success — the CLI responded.",
        "Failed — the CLI did not answer.",
        &[
            "Common causes: the CLI is not installed, not",
            "authenticated, or timed out. Run the CLI once to log in.",
        ],
    )
}

/// Which way out of the auto-verify failure screen (R3) the user took. `Edit`
/// returns to the just-finished sub-flow (the most common failure cause is a
/// mistyped key or model, so fixing beats retrying); `Retry` re-runs the
/// probe; `Save` records the failure and continues anyway (the offline
/// escape hatch — a failed probe must never lock the user out of saving);
/// `Back` is Esc back to the top menu with the failure recorded; `Cancel`
/// is Ctrl-C for the whole setup.
pub(super) enum FailExit {
    Edit,
    Retry,
    Save,
    Back,
    Cancel,
}

/// The auto-verify failure screen (R3): the probe error plus its common
/// causes, then a three-way exit — Edit (default highlight), Retry, Save
/// anyway. Esc maps to [`FailExit::Back`]; Ctrl-C to [`FailExit::Cancel`].
/// Pure UI: the caller owns the probe loop and the status bookkeeping.
pub(super) fn failure_exit(header: &str, error: &str, causes: &[&str]) -> Result<FailExit> {
    let term = Term::stderr();
    term.clear_screen()?;
    term.move_cursor_to(0, 0)?;
    oneshot::stderr_line(header, palette::plain());
    eprintln!();
    oneshot::stderr_line(
        &format!("{ICON_CROSS} Failed — the probe did not succeed."),
        palette::failure(),
    );
    oneshot::stderr_detail(&format!("Error: {error}"), palette::muted());
    eprintln!();
    for cause in causes {
        oneshot::stderr_detail(cause, palette::muted());
    }
    eprintln!();
    let items = vec![
        format!("{ICON_PENCIL}  Edit settings"),
        format!("{DOT} Retry probe"),
        format!("{DOT} Save anyway"),
    ];
    // Edit leads and is the default highlight: a mistyped key or model is
    // the usual failure, and fixing it beats retrying the same values.
    Ok(match opt_nav("How do you want to proceed?", &items, 0)? {
        OptNav::Value(0) => FailExit::Edit,
        OptNav::Value(1) => FailExit::Retry,
        OptNav::Value(_) => FailExit::Save,
        OptNav::Back => FailExit::Back,
        OptNav::Cancel => FailExit::Cancel,
    })
}

/// Outcome of one auto-verify pass (R3) for the calling sub-flow. `Pass` and
/// `FailDone` both mean "return to the top menu" (status recorded — the
/// failure screen's Save-anyway / Esc exits); `Edit` means the user chose to
/// fix the settings, so the caller re-enters its own sub-flow. Returned
/// inside `Option`: `None` is Ctrl-C (cancel the whole setup).
pub(super) enum Verified {
    Pass,
    Edit,
    FailDone,
}

/// Drive one auto-verify pass (R3): run `probe` against the draft's
/// effective values, record the outcome in `draft.verify_status`, show the
/// result screen, and on failure offer Edit / Retry / Save anyway. Shared by
/// the provider and CLI flows; `target` is the status-row label (`openai`,
/// `claude`), `header`/`causes` shape the failure screen.
pub(super) fn auto_verify(
    draft: &mut Draft,
    target: &str,
    probe: &dyn Fn(&Draft) -> Result<String>,
    show_success: &dyn Fn(&Draft, &str) -> Result<()>,
    header: &str,
    causes: &[&str],
) -> Result<Option<Verified>> {
    loop {
        match probe(draft) {
            Ok(reply) => {
                draft.verify_status = Some(VerifyStatus::pass(target.to_string()));
                show_success(draft, &reply)?;
                return Ok(Some(Verified::Pass));
            }
            Err(e) => match failure_exit(header, &e.to_string(), causes)? {
                FailExit::Edit => return Ok(Some(Verified::Edit)),
                FailExit::Retry => continue,
                FailExit::Save | FailExit::Back => {
                    draft.verify_status = Some(VerifyStatus::fail(target.to_string()));
                    return Ok(Some(Verified::FailDone));
                }
                FailExit::Cancel => return Ok(None),
            },
        }
    }
}

/// Success screen for the provider auto-verify (R3): reuses the Verify
/// result screen, showing the effective key / base URL context.
pub(super) fn show_provider_success(draft: &Draft, reply: &str) -> Result<()> {
    let p = draft.provider.unwrap_or_default();
    let api_key = resolve_field(draft.api_key.as_deref().filter(|k| !k.is_empty()), "").0;
    let base_url = resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p).0;
    let model = resolve_field(draft.model.as_deref().filter(|m| !m.is_empty()), "").0;
    show_verify_result(
        &p,
        &model,
        &api_key,
        base_url.as_deref(),
        Ok(reply.to_string()),
    )
}

/// Success screen for the CLI auto-verify (R3): the draft is already
/// reflected in the probe, so the reply is all there is to show.
pub(super) fn show_cli_success(_draft: &Draft, reply: &str) -> Result<()> {
    show_cli_verify_result(Ok(reply.to_string()))
}
