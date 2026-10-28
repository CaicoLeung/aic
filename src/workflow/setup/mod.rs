//! The interactive `aic setup` wizard — a menu-driven configuration flow that
//! writes [`Config`] to disk. The wizard is the shallow UI over the deep
//! config-resolution concept that lives in [`crate::core::config`]; it was extracted
//! out of `config.rs` (AIC-17) so resolution is no longer buried under ~900
//! lines of TUI machinery.
//!
//! The wizard is menu-driven, not a forced linear path: the top level offers
//! two independent entries — the AI provider (provider + key + base URL +
//! model) and the run toggles (confirm before commit, review batch plan) —
//! plus `Save & exit`. Generic
//! interactive primitives (single-choice menu, text prompt, IO-cancel
//! classifier) live in [`crate::workflow::input`].
//! The sub-flows live in submodules: [`provider`] (AI-provider path),
//! [`cli_flow`] (CLI-agent picker), [`verify`] (probes), and [`finalize`]
//! (draft↔Config conversion).

use anyhow::{Context, Result};
use console::Term;
use std::io::{self, IsTerminal};

use crate::core::config::{BackendKind, CliConfig, Config, ProviderProfile, config_path};
use crate::llm::{Provider, list_models};
use crate::render::{oneshot, palette};
use crate::workflow::input::{OptNav, TextAct, opt_nav, prompt_text, prompt_yes_no};
use std::collections::HashMap;

/// The wizard's injected seams (ADR 0019) — the operations `cargo test`
/// cannot run hermetically. Production wires real implementations; tests
/// script results. Shaped after [`crate::workflow::RunDeps`]: one purposeful
/// bundle, not a provider trait hierarchy.
pub(super) struct SetupDeps {
    /// Fetch a provider's live model list (`provider`, effective `api_key`,
    /// effective `base_url`). An empty return means failure / offline /
    /// missing key — the Model step then degrades to required free text and
    /// never blocks (ADR 0019 §4).
    pub fetch_models: FetchModels,
    /// Probe the API-provider backend with the draft's effective values
    /// (R3 auto-verify): `Ok(reply)` on success, `Err` with the provider /
    /// validation error otherwise.
    pub verify_provider: VerifyProbe,
    /// Probe the CLI-agent backend the same way (R3 auto-verify).
    pub verify_cli: VerifyProbe,
}

/// The one R1 seam: a boxed listing fetch, injectable for hermetic tests.
pub(super) type FetchModels = Box<dyn Fn(Provider, &str, Option<&str>) -> Vec<String>>;

/// An R3 seam: one boxed backend probe (`&Draft` → reply or error).
pub(super) type VerifyProbe = Box<dyn Fn(&Draft) -> Result<String>>;

impl SetupDeps {
    /// Production wiring: the real listing fetch and verify probes, driven
    /// on a dedicated nested runtime under a spinner (the `verify::probe`
    /// pattern). The listing collapses any error to an empty list — the
    /// degrade contract above.
    fn real() -> Self {
        Self {
            fetch_models: Box::new(|provider, api_key, base_url| {
                let label = format!("Fetching {} models…", provider.display());
                verify::probe(&label, list_models(provider, api_key, base_url)).unwrap_or_default()
            }),
            verify_provider: Box::new(verify::probe_provider),
            verify_cli: Box::new(verify::probe_cli),
        }
    }
}

mod cli_flow;
mod finalize;
mod provider;
mod verify;

#[cfg(test)]
mod tests;

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

    let deps = SetupDeps::real();
    match wizard(&deps)? {
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

/// Clear the terminal and render the setup header, so each menu or prompt
/// occupies a clean screen instead of leaving previous selections in the
/// scrollback.
fn show_screen(section: &str) -> Result<()> {
    // stderr: all interactive chrome shares inquire's stream, so a piped
    // stdout carries only real results (the save path's "Saved to" line).
    let term = Term::stderr();
    term.clear_screen()?;
    term.move_cursor_to(0, 0)?;
    oneshot::stderr_line(&format!("aic setup — {section}"), palette::plain());
    oneshot::stderr_detail(
        "↑/↓ move · Enter confirm · Esc back/cancel · Ctrl-C cancel",
        palette::muted(),
    );
    eprintln!();
    Ok(())
}

// Glyphs that visually distinguish each setup item across the menus. Each
// appears at the start of its menu row so items are scannable at a glance.
// Marker vocabulary fixed by ADR 0018: a muted `·` section bullet for menu
// rows, ✓/✗/⚠ for outcome rows — plain Unicode, no Nerd Font required.
const DOT: &str = "·"; // muted `·` section bullet for every menu row

// Status markers for verify outcomes and the save line — same glyph family.
const ICON_CHECK: &str = "✓";
const ICON_CROSS: &str = "✗";
const ICON_ALERT: &str = "⚠";
const ICON_PENCIL: &str = "·";

/// Per-step outcome for the setup state machine.
#[derive(PartialEq)]
enum Nav {
    Next,
    Back,
    Cancel,
}

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
    /// Latest auto-verify outcome (R3): which backend target was probed and
    /// whether it passed. `None` ⇒ not verified this session. Shown as the
    /// status line above the top-level menu; a completed sub-flow re-probes
    /// and overwrites it.
    verify_status: Option<VerifyStatus>,
}

/// One auto-verify probe outcome (R3): the probed target's short label
/// (`openai`, `claude`) and whether it passed.
#[derive(Clone, PartialEq, Debug)]
pub(super) struct VerifyStatus {
    pub(super) target: String,
    pub(super) passed: bool,
}

impl VerifyStatus {
    pub(super) fn pass(target: String) -> Self {
        Self {
            target,
            passed: true,
        }
    }

    pub(super) fn fail(target: String) -> Self {
        Self {
            target,
            passed: false,
        }
    }
}

/// The top-menu status line for the latest auto-verify (R3): `✓ target`,
/// `✗ target`, or `not verified` before any probe ran. Pure.
pub(super) fn verify_status_label(status: &Option<VerifyStatus>) -> String {
    match status {
        None => "not verified".to_string(),
        Some(s) if s.passed => format!("{ICON_CHECK} {}", s.target),
        Some(s) => format!("{ICON_CROSS} {}", s.target),
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

    /// The model the selected provider will use, for display: the in-session
    /// draft choice (seeded from the existing config in
    /// [`finalize::seed_draft`]). Empty when none is chosen — there is no
    /// default model (ADR 0019); the provider flow gates `Done` on it.
    fn effective_model(&self) -> String {
        self.model
            .as_deref()
            .filter(|m| !m.is_empty())
            .map(String::from)
            .unwrap_or_default()
    }
}

/// Top-level menu choices for the setup wizard. `Esc` and Ctrl-C at the menu
/// both cancel; the wizard distinguishes them so `Esc` can offer to keep
/// unsaved changes while Ctrl-C stays an immediate, unguarded exit.
enum MenuChoice {
    Backend,
    Provider,
    CliAgent,
    Confirm,
    Review,
    Save,
    /// Esc — cancel, but the wizard guard-checks unsaved changes first.
    Esc,
    /// Ctrl-C — immediate cancel, no guard.
    Cancel,
}

/// Run the setup wizard. Returns `None` when the user cancels (Esc on the
/// top-level menu, or Ctrl-C anywhere). Silent on the cancel path — the
/// caller prints the notice.
///
/// The wizard is menu-driven, not a forced linear path: the top level offers a
/// Backend selector (which of the API-provider / CLI-agent backends a Run
/// uses), the AI provider (provider + key + base URL + model), the CLI
/// agent (command + args + timeout), and the run toggles (confirm before
/// commit, review batch plan) — plus `Save & exit`. Each entry configures
/// only its own fields and returns to the
/// menu; both backends keep their configured content, so switching the active
/// Backend never wipes what was entered for the other.
fn wizard(deps: &SetupDeps) -> Result<Option<Config>> {
    let existing = Config::load().unwrap_or(None);
    let existing_provider = existing
        .as_ref()
        .and_then(|c| c.backend.as_deref())
        .map(Provider::from_name);

    // Seed the draft from the existing config so a partial visit never wipes
    // fields the user didn't touch: `Save & exit` writes the merged draft.
    let mut draft = finalize::seed_draft(&existing);

    // Mode-first on a fresh install (ADR 0011): with no config yet, teach the
    // two-backend model up front and route into the chosen backend's flow.
    // Re-config skips this and lands on the menu, whose Backend selector row
    // sets the active backend (both backends' fields stay in the draft).
    if existing.is_none() {
        match step_mode_choice()? {
            ModeChoice::Api => {
                draft.backend_kind = Some(BackendKind::Api);
                if provider::run_provider_flow(deps, &existing, existing_provider, &mut draft)? {
                    return Ok(None);
                }
            }
            ModeChoice::Cli => {
                draft.backend_kind = Some(BackendKind::Cli);
                if cli_flow::run_cli_flow(deps, &mut draft)? {
                    return Ok(None);
                }
            }
            ModeChoice::Skip => {} // Esc — drop straight into the menu
            ModeChoice::Cancel => return Ok(None),
        }
    }

    // Which menu row to highlight when the menu re-renders: returning from a
    // sub-flow highlights the entry the user just finished.
    let mut highlight = 0;
    loop {
        let dirty = finalize::draft_dirty(&draft, &existing);
        match step_menu(&draft, dirty, highlight)? {
            MenuChoice::Backend => {
                // Flip the active Backend (ADR 0011). Both backends keep their
                // configured fields; only the selector changes, and `finalize`
                // writes just the active one on save.
                if step_backend_choice(&mut draft)? {
                    return Ok(None); // Ctrl-C on the backend selector
                }
                highlight = 0;
            }
            MenuChoice::Provider => {
                if provider::run_provider_flow(deps, &existing, existing_provider, &mut draft)? {
                    return Ok(None); // Ctrl-C inside the provider path
                }
                highlight = 1;
            }
            MenuChoice::CliAgent => {
                if cli_flow::run_cli_flow(deps, &mut draft)? {
                    return Ok(None); // Ctrl-C inside the CLI-agent path
                }
                highlight = 2;
            }
            MenuChoice::Confirm => {
                // R2: Enter on the toggle row flips it in place and the
                // redrawn menu shows the new on/off — no on/off sub-screen.
                draft.confirm_before_commit = Some(!draft.confirm_before_commit.unwrap_or(false));
                highlight = 3;
            }
            MenuChoice::Review => {
                // R2: same in-place flip for the review gate (issue #157).
                draft.review_batch_plan = Some(!draft.review_batch_plan.unwrap_or(false));
                highlight = 4;
            }
            MenuChoice::Save => return Ok(Some(finalize::finalize(draft))),
            MenuChoice::Esc => {
                // Esc cancels too, but not silently: with unsaved changes,
                // confirm the discard (Enter = discard; "no" returns to the
                // menu). Ctrl-C stays the unconditional exit.
                if dirty && !prompt_yes_no("Discard unsaved changes?")? {
                    continue;
                }
                return Ok(None);
            }
            MenuChoice::Cancel => return Ok(None),
        }
    }
}

/// Mode-first first-run choice (ADR 0011): which Backend to set up. Offered
/// only when no config exists yet.
enum ModeChoice {
    Api,
    Cli,
    /// Esc on the mode screen — skip the guided choice and drop into the menu.
    Skip,
    Cancel,
}

/// First-run screen: pick which Backend aic should use. Teaches the
/// two-backend model without forcing re-configuring users through it. Esc
/// skips to the menu; Ctrl-C cancels setup.
fn step_mode_choice() -> Result<ModeChoice> {
    show_screen("choose a backend")?;
    let items = vec![
        format!("{DOT} API provider — use an API key (OpenAI, Anthropic, Gemini, …)"),
        format!("{DOT} CLI agent — reuse Claude Code / Codex / pi (no API key needed)"),
    ];
    Ok(match opt_nav("How should aic get its model?", &items, 0)? {
        OptNav::Value(0) => ModeChoice::Api,
        OptNav::Value(1) => ModeChoice::Cli,
        OptNav::Value(_) => unreachable!("mode choice has exactly two entries"),
        OptNav::Back => ModeChoice::Skip,
        OptNav::Cancel => ModeChoice::Cancel,
    })
}

/// Pick the active Backend (ADR 0011): a two-option nav between the API
/// provider and CLI agent. Both backends keep their configured fields in the
/// draft — this only flips which one a Run uses (and which [`finalize::finalize`]
/// writes). Defaults to the current selection; a config with no `backend_kind`
/// seeds as API (the historical default). Returns `true` on Ctrl-C (cancel
/// setup).
fn step_backend_choice(draft: &mut Draft) -> Result<bool> {
    show_screen("choose a backend")?;
    let items = vec![
        format!("{DOT} API provider — use an API key"),
        format!("{DOT} CLI agent — reuse a coding-agent CLI (no API key)"),
    ];
    let default = match draft.active_backend() {
        BackendKind::Api => 0,
        BackendKind::Cli => 1,
    };
    match opt_nav("Which backend should aic use?", &items, default)? {
        OptNav::Value(0) => draft.backend_kind = Some(BackendKind::Api),
        OptNav::Value(_) => draft.backend_kind = Some(BackendKind::Cli),
        OptNav::Back => {} // Esc — keep the current selection
        OptNav::Cancel => return Ok(true),
    }
    Ok(false)
}
/// `AI provider` menu row: the current provider and the model that would be
/// used (the chosen one; there is no default model — ADR 0019). The API
/// backend always resolves to a provider — OpenAI by default, matching
/// [`finalize::finalize`] — so the row never reads `(not set)` while API is
/// the active backend. `(not set)` stays correct only for the CLI backend,
/// which ignores the provider.
fn provider_label(draft: &Draft) -> String {
    let p = match draft.provider {
        Some(p) => p,
        // API backend defaults to OpenAI (mirrors `finalize`), so the menu
        // shows what aic will actually use instead of the misleading
        // "(not set)" after the API mode/backend is chosen.
        None if draft.active_backend() == BackendKind::Api => Provider::default(),
        None => return "(not set)".to_string(),
    };
    let model = draft.effective_model();
    if model.is_empty() {
        p.display().to_string()
    } else {
        format!("{} · {model}", p.display())
    }
}
/// Menu-row label for an opt-in toggle (both workflow gates default off):
/// `on` only when explicitly on (ADR 0018 toggle wording).
fn yes_no(value: Option<bool>) -> String {
    if value.unwrap_or(false) {
        "on".to_string()
    } else {
        "off".to_string()
    }
}

/// `CLI agent` menu row: the configured command's name — args stay in the
/// picker and the config, not on this row. `(not configured)` when no
/// CLI-agent backend is set.
fn cli_label(draft: &Draft) -> String {
    match draft.active_cli_command() {
        Some(cmd) => cmd.to_string(),
        None => "(not configured)".to_string(),
    }
}
/// Render and run the top-level menu. Entering an entry routes to its
/// sub-flow; `Save & exit` finalizes; Esc/`Ctrl-C` cancel the whole setup.
/// `default_idx` is the row highlighted when the menu opens (persisted from
/// the entry the user just finished). `dirty` marks the Save row when the
/// session has changes [`finalize::draft_dirty`] would not write back as
/// identical, so the cost of cancelling is visible before the user pays it.
fn step_menu(draft: &Draft, dirty: bool, default_idx: usize) -> Result<MenuChoice> {
    // R3: the latest auto-verify outcome rides above the menu as a status
    // line — informational, not a selectable row (toggles don't probe).
    oneshot::stderr_detail(
        &format!("Verify: {}", verify_status_label(&draft.verify_status)),
        palette::muted(),
    );
    eprintln!();
    let save_row = if dirty {
        format!("{DOT} Save & exit — unsaved changes")
    } else {
        format!("{DOT} Save & exit")
    };
    let items = vec![
        format!("{DOT} Backend — {}", draft.active_backend().display_name()),
        format!("{DOT} AI provider — {}", provider_label(draft)),
        format!("{DOT} CLI agent — {}", cli_label(draft)),
        format!(
            "{DOT} Confirm before commit — {}",
            yes_no(draft.confirm_before_commit)
        ),
        format!(
            "{ICON_PENCIL} Review batch plan — {}",
            yes_no(draft.review_batch_plan)
        ),
        save_row,
    ];
    match opt_nav("What would you like to configure?", &items, default_idx)? {
        OptNav::Value(0) => Ok(MenuChoice::Backend),
        OptNav::Value(1) => Ok(MenuChoice::Provider),
        OptNav::Value(2) => Ok(MenuChoice::CliAgent),
        OptNav::Value(3) => Ok(MenuChoice::Confirm),
        OptNav::Value(4) => Ok(MenuChoice::Review),
        OptNav::Value(5) => Ok(MenuChoice::Save),
        OptNav::Value(_) => unreachable!("menu has exactly six entries"),
        OptNav::Back => Ok(MenuChoice::Esc),
        OptNav::Cancel => Ok(MenuChoice::Cancel),
    }
}
