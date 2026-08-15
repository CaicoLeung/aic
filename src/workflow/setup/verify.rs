//! The setup wizard's Verify probes: the API-provider sample request, the
//! CLI-agent probe, and the smoke check that a configured program exists.

use super::finalize::mask_key;
use super::*;
use crate::core::config::{ResolvedConfig, resolve_api_key, resolve_base_url, resolve_field};
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
                "⚠️  `{program}` not found on $PATH — install + authenticate it before using aic"
            );
        }
        Err(_) => return format!("⚠️  could not verify `{program}`"),
    };
    let deadline = std::time::Instant::now() + SMOKE_TIMEOUT;
    loop {
        match cmd.try_wait() {
            Ok(Some(status)) if status.success() => return format!("✅ `{program}` found"),
            Ok(Some(_)) => {
                return format!(
                    "⚠️  `{program}` ran but `--version` exited non-zero — it may still work"
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
                    "⚠️  `{program}` did not respond to `--version` within 3s — it may not support print mode, or is not installed"
                );
            }
            Err(_) => return format!("⚠️  could not verify `{program}`"),
        }
    }
}
/// Wait for Enter so a smoke-check / status message stays visible before the
/// screen redraws. Best-effort: a non-interactive stdin just continues.
pub(super) fn pause_done() -> Result<()> {
    use std::io::BufRead;
    eprint!("\nPress Enter to continue… ");
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    Ok(())
}
/// The Verify item (AIC-23): make a minimal sample request against the
/// selected provider using the **effective** config — config > default, with
/// an in-session draft edit standing in for the config value — i.e. exactly
/// the values the sub-menu rows show. Success or the underlying provider
/// error (auth, rate limit, network, unknown model) is reported on a dedicated
/// screen, then the wizard returns to the sub-menu. Never auto-advances:
/// Verify is a probe, not a field edit.
///
/// The sample call runs on a dedicated current-thread Tokio runtime. `aic
/// setup` is dispatched from `#[tokio::main]`, so the wizard is already
/// executing inside a runtime; `block_in_place` parks the main task on the
/// multi-thread runtime so a nested runtime can drive the async verify call
/// without panicking.
pub(super) fn step_verify(draft: &Draft) -> Result<Nav> {
    let p = draft.provider.unwrap_or_default();
    // Effective values: the draft (a user edit or the seeded config value),
    // then the default.
    let api_key = resolve_api_key(draft.api_key.as_deref().filter(|k| !k.is_empty())).0;
    let base_url = resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p).0;
    let model = resolve_field(
        draft.model.as_deref().filter(|m| !m.is_empty()),
        p.default_model(),
    )
    .0;
    let resolved = ResolvedConfig::from_parts(
        p.name().to_string(),
        api_key,
        model.clone(),
        base_url.clone(),
    );

    // The same validation the Run path runs (`LlmConfig::load`), so a missing
    // required field reads as a setup hint, not an opaque provider error.
    if let Err(e) = resolved.validate() {
        show_verify_result(&p, &model, &resolved.api_key, base_url.as_deref(), Err(e))?;
        return Ok(Nav::Next);
    }

    let llm = resolved.to_llm();
    let label = format!("Contacting {} ({model})…", p.display());
    // block_in_place parks the outer multi-thread task; the nested
    // current-thread runtime then drives the async verify future. Without
    // block_in_place, Runtime::block_on panics inside an active runtime.
    let result = tokio::task::block_in_place(|| -> Result<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build verify runtime")?;
        rt.block_on(crate::render::progress::with_spinner(&label, async {
            llm.agent("You are a connectivity checker. Follow the user's instruction exactly.")
                .verify()
                .await
        }))
    });

    show_verify_result(&p, &model, &resolved.api_key, base_url.as_deref(), result)?;
    Ok(Nav::Next)
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
    term.write_line(&format!("Verify — {} ({model})", p.display()))?;
    term.write_line(&format!(
        "  API key:  {}",
        if api_key.is_empty() {
            "(none)".to_string()
        } else {
            mask_key(api_key)
        }
    ))?;
    term.write_line(&format!("  Base URL: {}", base_url.unwrap_or("(none)")))?;
    term.write_line("")?;
    match result {
        Ok(reply) => {
            term.write_line("✅ Success — the provider responded.")?;
            if !reply.is_empty() {
                term.write_line(&format!("  Reply: {reply}"))?;
            }
        }
        Err(e) => {
            term.write_line("❌ Failed — the provider did not accept the request.")?;
            term.write_line(&format!("  Error: {e}"))?;
            term.write_line("")?;
            term.write_line("  Common causes: wrong API key, model name, base URL, or network.")?;
        }
    }
    term.write_line("")?;
    term.write_line("Press Enter to return to the menu…")?;
    let _ = term.read_char();
    Ok(())
}

/// The CLI analogue of [`step_verify`] (AIC-23): probe the configured CLI with a
/// minimal prompt using the **effective** draft values, so a missing binary or
/// an unauthenticated CLI is caught here — at setup time — rather than failing
/// mid-Run. The CLI runs in headless/print mode; the probe sends "Reply with
/// exactly: OK" and checks for a reply. Install / auth / timeout errors surface
/// as the matching [`LlmError`](crate::llm::cli_agent::LlmError); success reports the
/// trimmed reply.
///
/// Runs on a dedicated current-thread runtime like [`step_verify`] — `aic
/// setup` is already inside `#[tokio::main]`, so `block_in_place` parks the
/// outer task while the nested runtime drives the async probe.
pub(super) fn step_verify_cli(draft: &Draft) -> Result<Nav> {
    if draft.active_cli_command().is_none() {
        // Defensive: the menu only offers Verify when a command is set.
        show_cli_verify_result(Err(anyhow::anyhow!("no CLI command is set yet")))?;
        return Ok(Nav::Next);
    }
    // Resolve the spec the SAME way a run does — `CliConfig::to_spec` — so
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
    let result = tokio::task::block_in_place(|| -> Result<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build verify runtime")?;
        rt.block_on(crate::render::progress::with_spinner(&label, async {
            agent.verify().await
        }))
    });
    show_cli_verify_result(result)?;
    Ok(Nav::Next)
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
    term.write_line("Verify — CLI agent")?;
    term.write_line("")?;
    match result {
        Ok(reply) => {
            term.write_line("✅ Success — the CLI responded.")?;
            if !reply.is_empty() {
                term.write_line(&format!("  Reply: {reply}"))?;
            }
        }
        Err(e) => {
            term.write_line("❌ Failed — the CLI did not answer.")?;
            term.write_line(&format!("  Error: {e}"))?;
            term.write_line("")?;
            term.write_line("  Common causes: the CLI is not installed, not")?;
            term.write_line("  authenticated, or timed out. Run the CLI once to log in.")?;
        }
    }
    term.write_line("")?;
    term.write_line("Press Enter to return to the menu…")?;
    let _ = term.read_char();
    Ok(())
}
