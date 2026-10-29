//! The setup wizard's Verify probes (R3): the API-provider and CLI-agent
//! sample requests that run automatically when a backend sub-flow completes,
//! plus the smoke check that a configured program exists. Outcome screens
//! live in [`super::app`]; this module only runs the probes and returns
//! their results.

use super::*;
use crate::core::config::ResolvedConfig;
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

/// Drive `fut` on a dedicated current-thread Tokio runtime under a hard
/// `timeout`. Since ADR 0020 the probes run on the wizard's background job
/// threads, which are plain `std::thread`s outside any runtime, so the
/// nested `block_on` is legal without `block_in_place`. Shared by the
/// Verify probes and the wizard's model-listing fetch (ADR 0019), so every
/// setup-time network touch runs the same way. The spinner paints inside
/// the alternate screen via the app's render loop, not here.
///
/// The timeout is the wedge guard: the rig/reqwest path carries no total
/// request timeout of its own, so without this ceiling a hung connection
/// would spin the Busy screen forever (the screen itself is Esc-abortable
/// and Ctrl-C always cancels — this bound is the belt to those braces).
pub(super) fn probe<T>(
    timeout: Duration,
    label: &str,
    fut: impl Future<Output = Result<T>>,
) -> Result<T> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .context("failed to build verify runtime")?;
    rt.block_on(async {
        tokio::time::timeout(timeout, fut)
            .await
            .map_err(|_| anyhow::anyhow!("{label} timed out after {} s", timeout.as_secs()))?
    })
}

/// Budget for one provider verify call. The probe prompt is trivial (one
/// short reply), so anything beyond this is a wedged connection, not a slow
/// model.
const PROVIDER_PROBE_TIMEOUT: Duration = Duration::from_secs(60);

/// Total cap for one CLI verify call — above the CLI backend's own idle
/// budget (`DEFAULT_TIMEOUT_SECS` = 240 s, reset on every output line), so
/// it only ever fires for a CLI that stays chatty without finishing.
const CLI_PROBE_TIMEOUT: Duration = Duration::from_secs(300);

/// Budget for a model-listing fetch — above `list_models`' own 5 s cap, so
/// it is the outer belt only.
pub(super) const MODEL_FETCH_TIMEOUT: Duration = Duration::from_secs(10);

/// The API-provider probe behind R3 auto-verify (AIC-23): a minimal sample
/// request against the selected provider using the **effective** draft
/// values — exactly the values the sub-menu rows show. Returns the model's
/// trimmed reply, or the underlying error (validation setup-hint, auth,
/// rate limit, network, unknown model). No UI: the caller decides which
/// screen an outcome gets.
pub(super) fn probe_provider(eff: &Effective) -> Result<String> {
    // There is no default model (ADR 0019) — an empty one fails `validate`
    // below with the setup hint.
    let resolved = ResolvedConfig::from_parts(
        eff.provider.name().to_string(),
        eff.api_key.clone(),
        eff.model.clone(),
        eff.base_url.clone(),
    );

    // The same validation the Run path runs (`LlmConfig::load`), so a missing
    // required field reads as a setup hint, not an opaque provider error.
    resolved.validate()?;

    let llm = resolved.to_llm();
    let label = format!("Contacting {} ({})", eff.provider.display(), eff.model);
    probe(PROVIDER_PROBE_TIMEOUT, &label, async {
        llm.agent("You are a connectivity checker. Follow the user's instruction exactly.")
            .verify()
            .await
    })
}

/// The CLI-agent probe behind R3 auto-verify: run the configured command in
/// print mode with a connectivity-check prompt; `Ok(reply)` means the
/// command exists and its envelope decoded. Runs on a dedicated
/// current-thread runtime like [`probe_provider`].
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
    let label = format!("Probing `{}`", spec.command);
    let agent = crate::llm::cli_agent::CliAgent::new(
        spec,
        "You are a connectivity checker. Follow the user's instruction exactly.".to_string(),
    );
    probe(CLI_PROBE_TIMEOUT, &label, async { agent.verify().await })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `probe` enforces its timeout even when the wrapped future never
    /// resolves: without the ceiling a wedged network call would spin the
    /// Busy screen forever (the screen is Esc-abortable, this is the belt).
    #[test]
    fn probe_times_out_a_never_resolving_future() {
        let start = std::time::Instant::now();
        let err = probe(
            Duration::from_millis(50),
            "test probe",
            std::future::pending::<anyhow::Result<()>>(),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("timed out after"),
            "error must name the timeout, got: {err:#}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must return promptly"
        );
    }

    /// A future that finishes inside the budget passes through untouched.
    #[test]
    fn probe_passes_through_a_fast_future() {
        let out = probe(Duration::from_secs(5), "test probe", async { Ok(7) }).unwrap();
        assert_eq!(out, 7);
    }
}
