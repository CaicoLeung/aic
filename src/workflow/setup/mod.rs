//! The interactive `aic setup` wizard — a menu-driven configuration flow that
//! writes [`Config`] to disk. The wizard is the shallow UI over the deep
//! config-resolution concept that lives in [`crate::core::config`]; it was extracted
//! out of `config.rs` (AIC-17) so resolution is no longer buried under ~900
//! lines of TUI machinery.
//!
//! The wizard is menu-driven, not a forced linear path: the top level offers
//! two independent entries — the AI provider (provider + key + base URL +
//! model) and the pre-commit confirmation toggle — plus `Save & exit`. Generic
//! interactive primitives (single-choice menu, text prompt, IO-cancel
//! classifier) live in [`crate::workflow::input`].
//! The sub-flows live in submodules: [`provider`] (AI-provider path),
//! [`cli_flow`] (CLI-agent picker), [`verify`] (probes), and [`finalize`]
//! (draft↔Config conversion).

use anyhow::{Context, Result};
use console::Term;
use std::io::{self, IsTerminal};

use crate::core::config::{BackendKind, CliConfig, Config, ProviderProfile, config_path};
use crate::llm::Provider;
use crate::workflow::input::{OptNav, TextAct, opt_nav, prompt_text, prompt_yes_no};
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
        eprintln!("aic setup needs an interactive terminal, but stdin is not a TTY.");
        eprintln!("To configure non-interactively you can either:");
        eprintln!(
            "  • write the config file at {path} (TOML keys: backend, api_key, model, base_url, confirm_before_commit), or"
        );
        eprintln!("  • run `aic setup` in a TTY, or edit the config file directly");
        anyhow::bail!("cannot run interactive setup without a TTY");
    }

    match wizard()? {
        Some(config) => {
            config.save()?;
            let path = config_path().context("could not determine config path")?;
            println!("\n✅ Saved to {}\n", path.display());
            Ok(())
        }
        None => {
            println!("Setup cancelled. Nothing was saved.");
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
    term.write_line(&format!("aic setup — {section}"))?;
    term.write_line("  ↑/↓ move · Enter confirm · Esc back/cancel · Ctrl-C cancel")?;
    term.write_line("")?;
    Ok(())
}

// Glyphs that visually distinguish each setup item across the menus. Each
// appears at the start of its menu row so items are scannable at a glance
// (AIC-15). Plain `&str` constants keep the menu code readable as labels.
const ICON_PROVIDER: &str = "🤖";
const ICON_API_KEY: &str = "🔑";
const ICON_BASE_URL: &str = "🌐";
const ICON_MODEL: &str = "🧠";
const ICON_CONFIRM: &str = "📋";
const ICON_SAVE: &str = "💾";
const ICON_VERIFY: &str = "🔌";
const ICON_DONE: &str = "↩️";
const ICON_CLI: &str = "⌨️";
const ICON_BACKEND: &str = "🔘";

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
struct Draft {
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

    /// The model the selected provider will actually use, for display: the
    /// in-session draft choice (seeded from the existing config in
    /// [`finalize::seed_draft`]) first, else the provider default. Empty when the
    /// provider has no default (OpenRouter, OpenAI-compatible).
    fn effective_model(&self, p: Provider) -> String {
        self.model
            .as_deref()
            .filter(|m| !m.is_empty())
            .map(String::from)
            .unwrap_or_else(|| p.default_model().to_string())
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
/// uses), the AI provider (provider + key + base URL + model), the CLI agent
/// (command + args + timeout), and the pre-commit confirmation toggle — plus
/// `Save & exit`. Each entry configures only its own fields and returns to the
/// menu; both backends keep their configured content, so switching the active
/// Backend never wipes what was entered for the other.
fn wizard() -> Result<Option<Config>> {
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
                if provider::run_provider_flow(&existing, existing_provider, &mut draft)? {
                    return Ok(None);
                }
            }
            ModeChoice::Cli => {
                draft.backend_kind = Some(BackendKind::Cli);
                if cli_flow::run_cli_flow(&mut draft)? {
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
                if provider::run_provider_flow(&existing, existing_provider, &mut draft)? {
                    return Ok(None); // Ctrl-C inside the provider path
                }
                highlight = 1;
            }
            MenuChoice::CliAgent => {
                if cli_flow::run_cli_flow(&mut draft)? {
                    return Ok(None); // Ctrl-C inside the CLI-agent path
                }
                highlight = 2;
            }
            MenuChoice::Confirm => {
                if run_confirm_flow(&existing, &mut draft)? {
                    return Ok(None); // Ctrl-C on the confirmation toggle
                }
                highlight = 3;
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
        "API provider — use an API key (OpenAI, Anthropic, Gemini, …)".to_string(),
        "CLI agent — reuse Claude Code / Codex / pi (no API key needed)".to_string(),
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
        "API provider — use an API key".to_string(),
        "CLI agent — reuse a coding-agent CLI (no API key)".to_string(),
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
/// used (the chosen one, else the provider default). The API backend always
/// resolves to a provider — OpenAI by default, matching [`finalize::finalize`] — so the
/// row never reads `(not set)` while API is the active backend. `(not set)`
/// stays correct only for the CLI backend, which ignores the provider.
fn provider_label(draft: &Draft) -> String {
    let p = match draft.provider {
        Some(p) => p,
        // API backend defaults to OpenAI (mirrors `finalize`), so the menu
        // shows what aic will actually use instead of the misleading
        // "(not set)" after the API mode/backend is chosen.
        None if draft.active_backend() == BackendKind::Api => Provider::default(),
        None => return "(not set)".to_string(),
    };
    let model = draft.effective_model(p);
    if model.is_empty() {
        p.display().to_string()
    } else {
        format!("{} · {model}", p.display())
    }
}
/// `Confirm before commit` menu row: yes/no, defaulting to off.
fn confirm_label(draft: &Draft) -> String {
    if draft.confirm_before_commit.unwrap_or(false) {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}

/// `CLI agent` menu row: the configured command, or `(not configured)` when no
/// CLI-agent backend is set. The active backend is named separately by the
/// [`backend_banner`] on the main menu.
fn cli_label(draft: &Draft) -> String {
    match draft.active_cli_command() {
        Some(cmd) => {
            let mut parts = vec![cmd.to_string()];
            parts.extend(draft.cli.args.clone().unwrap_or_default());
            parts.join(" ")
        }
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
    show_screen("main menu")?;
    let save_row = if dirty {
        format!("{ICON_SAVE} Save & exit — unsaved changes")
    } else {
        format!("{ICON_SAVE} Save & exit")
    };
    let items = vec![
        format!(
            "{ICON_BACKEND} Backend — {}",
            draft.active_backend().display_name()
        ),
        format!("{ICON_PROVIDER} AI provider — {}", provider_label(draft)),
        format!("{ICON_CLI} CLI agent — {}", cli_label(draft)),
        format!(
            "{ICON_CONFIRM} Confirm before commit — {}",
            confirm_label(draft)
        ),
        save_row,
    ];
    match opt_nav("What would you like to configure?", &items, default_idx)? {
        OptNav::Value(0) => Ok(MenuChoice::Backend),
        OptNav::Value(1) => Ok(MenuChoice::Provider),
        OptNav::Value(2) => Ok(MenuChoice::CliAgent),
        OptNav::Value(3) => Ok(MenuChoice::Confirm),
        OptNav::Value(4) => Ok(MenuChoice::Save),
        OptNav::Value(_) => unreachable!("menu has exactly five entries"),
        OptNav::Back => Ok(MenuChoice::Esc),
        OptNav::Cancel => Ok(MenuChoice::Cancel),
    }
}
/// Run the confirmation-toggle sub-flow (a single Yes/No step). Returns `true`
/// on Ctrl-C (cancel the whole setup); `false` returns to the menu either way.
fn run_confirm_flow(existing: &Option<Config>, draft: &mut Draft) -> Result<bool> {
    match step_confirm_commit(existing, draft)? {
        Nav::Next | Nav::Back => Ok(false),
        Nav::Cancel => Ok(true),
    }
}
/// Initial value for the confirmation toggle: the in-session draft choice
/// first, then the existing config's value, else `false` (default off). Not
/// provider-dependent, so no `field_initial`-style provider guard is needed.
fn confirm_initial(draft: &Draft, existing: &Option<Config>) -> bool {
    draft
        .confirm_before_commit
        .or_else(|| existing.as_ref().and_then(|c| c.confirm_before_commit))
        .unwrap_or(false)
}

/// Yes/No choice (an arrow-keyed option list) for requiring confirmation
/// before each commit (issue #78). Unlike the provider-scoped steps, this one
/// is not provider-dependent: the
/// initial value is the in-session draft choice, else the existing config's
/// value, else `false` (the default — behavior unchanged until the user opts
/// in).
fn step_confirm_commit(existing: &Option<Config>, draft: &mut Draft) -> Result<Nav> {
    show_screen("commit confirmation")?;
    let initial = confirm_initial(draft, existing);
    // A yes/no option list driven by arrow keys + Enter — never typed input,
    // so the user keeps their hands off the keyboard.
    let items = vec!["yes".to_string(), "no".to_string()];
    let default_idx = if initial { 0 } else { 1 };
    match opt_nav(
        "Require confirmation before each commit?",
        &items,
        default_idx,
    )? {
        OptNav::Value(0) => {
            draft.confirm_before_commit = Some(true);
            Ok(Nav::Next)
        }
        OptNav::Value(1) => {
            draft.confirm_before_commit = Some(false);
            Ok(Nav::Next)
        }
        OptNav::Value(_) => unreachable!("yes/no has exactly two entries"),
        OptNav::Back => Ok(Nav::Back),
        OptNav::Cancel => Ok(Nav::Cancel),
    }
}
