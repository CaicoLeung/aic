//! The interactive `aic setup` wizard — a menu-driven configuration flow that
//! writes [`Config`] to disk. The wizard is the shallow UI over the deep
//! config-resolution concept that lives in [`crate::core::config`]; it was
//! extracted out of `config.rs` (AIC-17) so resolution is no longer buried
//! under ~900 lines of TUI machinery.
//!
//! The wizard is menu-driven, not a forced linear path: the top level offers
//! the Backend selector, the AI provider (provider + key + base URL +
//! model), the CLI agent (command + args + timeout), and the run toggles
//! (confirm before commit, review batch plan), plus `Save & exit`.
//!
//! Since ADR 0020 the surface is a ratatui application ([`app`]): every
//! screen is one whole-frame diffed repaint inside the alternate screen, so
//! no intermediate frame can ever leak into the user's terminal — the
//! leftover-output defect class the old inquire screens (and their opt-in
//! per-screen clearing) kept reproducing. The sub-modules hold the flow's
//! pure logic: [`provider`] (AI-provider steps, ADR 0019), [`cli_flow`]
//! (CLI-agent presets), [`verify`] (probes), and [`finalize`]
//! (draft↔Config conversion).

use anyhow::{Context, Result};
use std::io::{self, IsTerminal};
use std::sync::Arc;

use crate::core::config::{
    BackendKind, CliConfig, Config, ProviderProfile, Source, config_path, resolve_base_url,
    resolve_field,
};
use crate::llm::{Provider, list_models};
use crate::render::tui::ICON_ALERT;
use crate::render::{oneshot, palette, tui};
use std::collections::HashMap;

mod app;
mod cli_flow;
mod finalize;
mod provider;
mod verify;

#[cfg(test)]
mod tests;

/// The wizard's injected seams (ADR 0019) — the operations `cargo test`
/// cannot run hermetically. Production wires real implementations; tests
/// script results. Shaped after [`crate::workflow::RunDeps`]: one purposeful
/// bundle, not a provider trait hierarchy. `Send + Sync` because the
/// ratatui event loop runs each operation on a background thread (ADR
/// 0020) so the spinner keeps animating.
pub(super) struct SetupDeps {
    /// Fetch a provider's live model list (`provider`, effective `api_key`,
    /// effective `base_url`). An empty return means failure / offline /
    /// missing key — the Model step then degrades to required free text and
    /// never blocks (ADR 0019 §4).
    pub fetch_models: FetchModels,
    /// Probe the API-provider backend with the draft's effective values
    /// (R3 auto-verify): `Ok(reply)` on success, `Err` with the provider /
    /// validation error otherwise.
    pub verify_provider: ProviderProbe,
    /// Probe the CLI-agent backend the same way (R3 auto-verify); it reads
    /// the whole draft (the CLI trio, not the provider fields).
    pub verify_cli: CliProbe,
    /// One `--version` existence check of a CLI program, as the human
    /// one-liner the verify screens show (ADR 0010 — aic never installs
    /// or authenticates a CLI itself).
    pub smoke: SmokeCheck,
}

/// The R1 seam: a boxed listing fetch, injectable for hermetic tests.
pub(super) type FetchModels =
    Box<dyn Fn(Provider, &str, Option<&str>) -> Vec<String> + Send + Sync>;

/// An R3 seam: a boxed provider probe (`&Effective` → reply or error).
pub(super) type ProviderProbe = Box<dyn Fn(&Effective) -> Result<String> + Send + Sync>;

/// An R3 seam: a boxed CLI probe (`&Draft` → reply or error) — the CLI
/// trio lives on the draft, not on [`Effective`].
pub(super) type CliProbe = Box<dyn Fn(&Draft) -> Result<String> + Send + Sync>;

/// The smoke-check seam: boxed so tests can script it or run real binaries.
pub(super) type SmokeCheck = Box<dyn Fn(&str) -> String + Send + Sync>;

impl SetupDeps {
    /// Production wiring: the real listing fetch and verify probes, each
    /// driven on a dedicated nested runtime by the wizard's background job
    /// threads. The listing collapses any error to an empty list — the
    /// degrade contract above.
    fn real() -> Self {
        Self {
            fetch_models: Box::new(|provider, api_key, base_url| {
                let label = format!("Fetching {} models…", provider.display());
                verify::probe(
                    verify::MODEL_FETCH_TIMEOUT,
                    &label,
                    list_models(provider, api_key, base_url),
                )
                .unwrap_or_default()
            }),
            verify_provider: Box::new(verify::probe_provider),
            verify_cli: Box::new(verify::probe_cli),
            smoke: Box::new(verify::smoke_check),
        }
    }
}

// Glyphs that visually distinguish each setup item across the menus. Each
// appears at the start of its menu row so items are scannable at a glance.
// Marker vocabulary fixed by ADR 0018: a muted `·` section bullet for menu
// rows, ✓/✗/⚠ for outcome rows — plain Unicode, no Nerd Font required.
const DOT: &str = "·"; // muted `·` section bullet for every menu row

// Status markers for verify outcomes and the save line — same glyph family.
// `ICON_ALERT` lives in [`crate::render::tui`], shared with the text
// input's validation hint so the glyph cannot drift between the two.
const ICON_CHECK: &str = "✓";
const ICON_CROSS: &str = "✗";
const ICON_PENCIL: &str = "·";

/// In-progress wizard selections. Values persist across sub-flow navigation so
/// the user can edit one entry without losing the others. Seeded from the
/// existing config ([`finalize::seed_draft`]) so untouched fields survive saving.
#[derive(Default, Clone)]
pub(super) struct Draft {
    /// Which Backend this session has chosen (ADR 0011). `None` ⇒ not yet
    /// chosen; [`Draft::active_backend`] defaults it to [`BackendKind::Api`].
    /// Set by the mode-first screen or a radio switch; drives [`finalize::finalize`].
    backend_kind: Option<BackendKind>,
    provider: Option<Provider>,
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    /// Whether to require confirmation before each commit. `None` means
    /// "not chosen yet" (finalize keeps it unset → config absent → default
    /// off); the wizard default shown to the user is `false`.
    confirm_before_commit: Option<bool>,
    /// Whether to show the Batch Review menu before drafting (issue #157).
    /// Same `None` semantics: not chosen → config absent → default off.
    review_batch_plan: Option<bool>,
    /// External coding-agent CLI fields (ADR 0010), shared with
    /// [`Config::cli`] so the command/args/timeout trio has one owner and is
    /// not redeclared here. When `backend_kind = cli`, aic runs in CLI-backend
    /// mode and the provider/api-key fields are dormant.
    cli: CliConfig,
    /// Remembered API-provider profiles (key/model/base_url per provider), so
    /// switching provider in this wizard restores them instead of clearing.
    /// Seeded from the existing config's `providers` list plus the active
    /// top-level fields (for pre-bank configs), and written back on save via
    /// [`finalize::finalize`].
    known_providers: Vec<ProviderProfile>,
    /// Fetched model lists this session (ADR 0019), keyed by
    /// provider + base URL + masked key — any change re-fetches. Never
    /// persisted; a fresh wizard run starts cold.
    model_cache: HashMap<String, Vec<String>>,
    /// Latest auto-verify outcome (R3): which Backend was probed and whether
    /// it passed. `None` ⇒ not verified this session. Since ADR 0020 it has
    /// no standalone status row — the ✓/✗ marker rides the top-menu row of
    /// the Backend it belongs to.
    verify_status: Option<VerifyStatus>,
}

/// The draft's effective API-provider values (ADR 0019 §5): what the
/// sub-menu rows show, the probes send, and the session model-cache and
/// fetch jobs are keyed by. One snapshot off [`Draft::effective`] instead
/// of a `resolve_field` dance re-derived at each site.
pub(super) struct Effective {
    pub(super) provider: Provider,
    pub(super) api_key: String,
    pub(super) base_url: Option<String>,
    pub(super) base_url_source: Source,
    /// The chosen model; empty when none is (no default model — ADR 0019).
    pub(super) model: String,
}

/// One auto-verify probe outcome (R3): the probed target's short label
/// (`openai`, `claude`), which Backend owns it, and whether it passed.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct VerifyStatus {
    pub(super) backend: BackendKind,
    pub(super) target: String,
    pub(super) passed: bool,
}

impl VerifyStatus {
    pub(super) fn backend_pass(backend: BackendKind, target: String) -> Self {
        Self {
            backend,
            target,
            passed: true,
        }
    }

    pub(super) fn backend_fail(backend: BackendKind, target: String) -> Self {
        Self {
            backend,
            target,
            passed: false,
        }
    }
}

impl Draft {
    /// The active CLI command (trimmed, non-empty) from the in-progress draft,
    /// or `None` when the API provider path is selected. Mirrors
    /// [`Config::active_cli_command`] so "is the CLI backend set?" has one
    /// definition across the wizard.
    fn active_cli_command(&self) -> Option<&str> {
        self.cli.active_command()
    }

    /// The Backend this draft resolves to: the session choice, else
    /// [`BackendKind::Api`] (the default when nothing is chosen — ADR 0011).
    fn active_backend(&self) -> BackendKind {
        self.backend_kind.unwrap_or(BackendKind::Api)
    }

    /// Resolve the draft's effective API-provider values — the draft's own
    /// choice (a user edit or the seeded config value) for every field.
    /// The API backend always resolves to a provider (OpenAI by default,
    /// mirroring [`finalize::finalize`]).
    pub(super) fn effective(&self) -> Effective {
        let provider = self.provider.unwrap_or_default();
        let api_key = resolve_field(self.api_key.as_deref().filter(|k| !k.is_empty()), "").0;
        let (base_url, base_url_source) = resolve_base_url(
            self.base_url.as_deref().filter(|u| !u.is_empty()),
            &provider,
        );
        let model = resolve_field(self.model.as_deref().filter(|m| !m.is_empty()), "").0;
        Effective {
            provider,
            api_key,
            base_url,
            base_url_source,
            model,
        }
    }
}

/// `AI provider` menu row: the current provider and the model that would be
/// used (the chosen one; there is no default model — ADR 0019). The API
/// backend always resolves to a provider — OpenAI by default, matching
/// [`finalize::finalize`] — so the row never reads `(not set)` while API is
/// the active backend. `(not set)` stays correct only for the CLI backend,
/// which ignores the provider.
pub(super) fn provider_label(draft: &Draft) -> String {
    let p = match draft.provider {
        Some(p) => p,
        // API backend defaults to OpenAI (mirrors `finalize`), so the menu
        // shows what aic will actually use instead of the misleading
        // "(not set)" after the API mode/backend is chosen.
        None if draft.active_backend() == BackendKind::Api => Provider::default(),
        None => return "(not set)".to_string(),
    };
    let model = draft.effective().model;
    if model.is_empty() {
        p.display().to_string()
    } else {
        format!("{} · {model}", p.display())
    }
}

/// `CLI agent` menu row: the configured command's name — args stay in the
/// picker and the config, not on this row. `(not configured)` when no
/// CLI-agent backend is set.
pub(super) fn cli_label(draft: &Draft) -> String {
    match draft.active_cli_command() {
        Some(cmd) => cmd.to_string(),
        None => "(not configured)".to_string(),
    }
}

/// The setup entry point. Refuses non-interactive or full-screen-incapable
/// terminals, runs the ratatui wizard inside the terminal guard, then —
/// after the guard has restored the user's shell — prints the save/cancel
/// notice into the normal buffer (the only setup bytes that ever touch it,
/// by design: results belong to the user's scrollback, chrome does not).
pub fn run_setup() -> Result<()> {
    if !io::stdin().is_terminal() {
        let path = config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "~/.config/aic/config.toml".into());
        oneshot::stderr_line(
            "aic setup needs an interactive terminal, but stdin is not a TTY.",
            palette::plain(),
        );
        oneshot::stderr_line(
            "To configure non-interactively you can either:",
            palette::plain(),
        );
        oneshot::stderr_detail(
            &format!(
                "• write the config file at {path} (TOML keys: backend, api_key, model, base_url, confirm_before_commit, review_batch_plan), or"
            ),
            palette::muted(),
        );
        oneshot::stderr_detail(
            "• run `aic setup` in a TTY, or edit the config file directly",
            palette::muted(),
        );
        anyhow::bail!("cannot run interactive setup without a TTY");
    }
    // ADR 0020: the wizard is a full-screen TUI; a terminal that cannot
    // host one (a VT-less Unix tty, legacy conhost) is refused outright —
    // the old stacked-scrollback degrade would resurrect exactly the
    // leftover-output defect class this migration eradicates.
    if !tui::terminal::tui_capable() {
        oneshot::stderr_line(
            "aic setup needs a full-screen terminal; this one does not support the required escape sequences.",
            palette::plain(),
        );
        anyhow::bail!("cannot run setup on this terminal");
    }
    let deps = Arc::new(SetupDeps::real());
    let existing = Config::load().unwrap_or(None);
    let mut term = tui::terminal::Tui::enter()?;
    let outcome = app::run(deps, existing, &mut term);
    drop(term);
    match outcome? {
        Some(config) => {
            config.save()?;
            let path = config_path().context("could not determine config path")?;
            println!();
            oneshot::stdout_line(
                &format!(
                    "{} Saved to {}",
                    palette::styled_stdout("\u{2713}", palette::success()),
                    path.display()
                ),
                palette::plain(),
            );
            println!();
            Ok(())
        }
        None => {
            oneshot::stdout_line("Setup cancelled. Nothing was saved.", palette::muted());
            Ok(())
        }
    }
}
