//! The interactive `aic setup` wizard — a menu-driven configuration flow that
//! writes [`Config`] to disk. The wizard is the shallow UI over the deep
//! config-resolution concept that lives in [`crate::config`]; it was extracted
//! out of `config.rs` (AIC-17) so resolution is no longer buried under ~900
//! lines of TUI machinery.
//!
//! The wizard is menu-driven, not a forced linear path: the top level offers
//! two independent entries — the AI provider (provider + key + base URL +
//! model) and the pre-commit confirmation toggle — plus `Save & exit`. Generic
//! interactive primitives (single-choice menu, text prompt, IO-cancel
//! classifier) live in [`crate::input`].

use anyhow::{Context, Result};
use console::Term;
use std::io::{self, IsTerminal};
use std::time::Duration;

use crate::cli_agent::{DEFAULT_TIMEOUT_SECS, PRESETS, PROMPT_PLACEHOLDER, cli_preset};
use crate::config::{
    BackendKind, Config, Source, config_path, resolve_api_key, resolve_base_url, resolve_field,
};
use crate::input::{OptNav, TextAct, opt_nav, prompt_text};
use crate::llm::{BaseUrlRequirement, Provider};

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
fn show_screen() -> Result<()> {
    let term = Term::stdout();
    term.clear_screen()?;
    term.move_cursor_to(0, 0)?;
    term.write_line("aic setup — configure your AI provider")?;
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
/// existing config ([`seed_draft`]) so untouched fields survive saving.
#[derive(Default)]
struct Draft {
    /// Which Backend this session has chosen (ADR 0011). `None` ⇒ not yet
    /// chosen; [`Draft::active_backend`] defaults it to [`BackendKind::Api`].
    /// Set by the mode-first screen or a radio switch; drives [`finalize`].
    backend_kind: Option<BackendKind>,
    provider: Option<Provider>,
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    /// Whether to require confirmation before each commit. `None` means
    /// "not chosen yet" (finalize keeps it unset → config absent → default
    /// off); the wizard default shown to the user is `false`.
    confirm_before_commit: Option<bool>,
    /// External coding-agent CLI (ADR 0010). When set, aic runs in CLI-backend
    /// mode and the provider/api-key fields are ignored.
    cli_command: Option<String>,
    cli_args: Option<Vec<String>>,
    cli_timeout_secs: Option<u64>,
}

impl Draft {
    /// The active CLI command (trimmed, non-empty) from the in-progress draft,
    /// or `None` when the API provider path is selected. Mirrors
    /// [`Config::active_cli_command`] so "is the CLI backend set?" has one
    /// definition across the wizard.
    fn active_cli_command(&self) -> Option<&str> {
        self.cli_command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// The Backend this draft resolves to: the session choice, else
    /// [`BackendKind::Api`] (the default when nothing is chosen — ADR 0011).
    fn active_backend(&self) -> BackendKind {
        self.backend_kind.unwrap_or(BackendKind::Api)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Provider,
    ApiKey,
    BaseUrl,
    Model,
}

/// Top-level menu choices for the setup wizard. `Esc`/Ctrl-C at the menu
/// cancels the whole setup; entering a sub-flow and finishing (or backing out
/// of its first step) returns here.
enum MenuChoice {
    Backend,
    Provider,
    CliAgent,
    Confirm,
    Save,
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
    let mut draft = seed_draft(&existing);

    // Mode-first on a fresh install (ADR 0011): with no config yet, teach the
    // two-backend model up front and route into the chosen backend's flow.
    // Re-config skips this and lands on the menu, whose Backend selector row
    // sets the active backend (both backends' fields stay in the draft).
    if existing.is_none() {
        match step_mode_choice()? {
            ModeChoice::Api => {
                draft.backend_kind = Some(BackendKind::Api);
                if run_provider_flow(&existing, existing_provider, &mut draft)? {
                    return Ok(None);
                }
            }
            ModeChoice::Cli => {
                draft.backend_kind = Some(BackendKind::Cli);
                if run_cli_flow(&mut draft)? {
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
        match step_menu(&draft, highlight)? {
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
                if run_provider_flow(&existing, existing_provider, &mut draft)? {
                    return Ok(None); // Ctrl-C inside the provider path
                }
                highlight = 1;
            }
            MenuChoice::CliAgent => {
                if run_cli_flow(&mut draft)? {
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
            MenuChoice::Save => return Ok(Some(finalize(draft))),
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
    show_screen()?;
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
/// draft — this only flips which one a Run uses (and which [`finalize`]
/// writes). Defaults to the current selection; a config with no `backend_kind`
/// seeds as API (the historical default). Returns `true` on Ctrl-C (cancel
/// setup).
fn step_backend_choice(draft: &mut Draft) -> Result<bool> {
    show_screen()?;
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

fn key_applies(p: Provider) -> bool {
    p.requires_key() || p == Provider::OpenAiCompatible
}

fn base_url_applies(p: Provider) -> bool {
    !matches!(p.base_url_requirement(), BaseUrlRequirement::None)
}

/// Ordered provider-scoped steps that actually apply to `p`. Steps that would
/// be a no-op — an API key for local Ollama, a base URL for a cloud provider —
/// are absent, so forward/back never lands on one. `Provider` always starts
/// the list and `Model` always ends it. `ConfirmCommit` is a top-level menu
/// entry, not part of the provider path, so it never appears here. Navigation
/// is just index ±1 off this list (see [`run_provider_flow`])
fn applicable_steps(p: Provider) -> Vec<Step> {
    let mut steps = vec![Step::Provider];
    if key_applies(p) {
        steps.push(Step::ApiKey);
    }
    if base_url_applies(p) {
        steps.push(Step::BaseUrl);
    }
    steps.push(Step::Model);
    steps
}

/// Seed the in-session draft from the existing config so untouched fields
/// survive `Save & exit` (a full-file write, see [`Config::save`]). A fresh
/// install leaves the draft empty — `finalize` then falls back to the OpenAI
/// default, as before.
fn seed_draft(existing: &Option<Config>) -> Draft {
    let mut draft = Draft::default();
    if let Some(c) = existing {
        draft.provider = c.backend.as_deref().map(Provider::from_name);
        draft.backend_kind = c.backend_kind;
        draft.api_key = c.api_key.clone();
        draft.base_url = c.base_url.clone();
        draft.model = c.model.clone();
        draft.confirm_before_commit = c.confirm_before_commit;
        draft.cli_command = c.command.clone();
        draft.cli_args = c.args.clone();
        draft.cli_timeout_secs = c.timeout_secs;
    }
    draft
}

/// The model the selected provider will actually use, for display: the
/// in-session draft choice (seeded from the existing config in
/// [`seed_draft`]) first, else the provider default. Empty when the provider
/// has no default (OpenRouter, OpenAI-compatible).
fn effective_model(p: Provider, draft: &Draft) -> String {
    draft
        .model
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(String::from)
        .unwrap_or_else(|| p.default_model().to_string())
}

/// `AI provider` menu row: the current provider and the model that would be
/// used (the chosen one, else the provider default). `(not set)` when no
/// provider is configured yet.
fn provider_label(draft: &Draft) -> String {
    let Some(p) = draft.provider else {
        return "(not set)".to_string();
    };
    let model = effective_model(p, draft);
    if model.is_empty() {
        p.display().to_string()
    } else {
        format!("{} · {model}", p.display())
    }
}

/// One row in the provider picker (the screen *before* this provider's
/// submenu). For the currently selected provider, show the model the user
/// actually chose (`draft.model`, seeded from the existing config) rather than
/// the bare default — otherwise re-entering setup makes the selection read as
/// lost (AIC-15). For every other provider, show its default so the options
/// stay comparable at a glance.
fn provider_choice_label(p: Provider, draft: &Draft) -> String {
    let model = if draft.provider == Some(p) {
        effective_model(p, draft)
    } else {
        p.default_model().to_string()
    };
    if model.is_empty() {
        format!("{}  (no default — you'll pick a model)", p.display())
    } else {
        format!("{}  ({model})", p.display())
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
            parts.extend(draft.cli_args.clone().unwrap_or_default());
            parts.join(" ")
        }
        None => "(not configured)".to_string(),
    }
}

/// A configurable field inside the AI-provider sub-menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderEntry {
    ApiKey,
    BaseUrl,
    Model,
    Verify,
    Done,
}

/// `API key` sub-menu row: the masked effective key (or "(not set)"). An API
/// key has no provider default, so — unlike [`base_url_label`] /
/// [`model_label`] — it takes no [`Source`].
fn api_key_label(key: &str) -> String {
    if key.is_empty() {
        return "(not set)".to_string();
    }
    mask_key(key)
}

/// `Base URL` sub-menu row: the effective URL (config > provider default),
/// annotated with its source.
fn base_url_label(url: Option<&str>, source: Source) -> String {
    match (url, source) {
        (Some(u), Source::Default) => format!("{u} (default)"),
        (Some(u), _) => u.to_string(),
        (None, _) => "(not set)".to_string(),
    }
}

/// `Model` sub-menu row: the effective model (config > provider default),
/// annotated with its source.
fn model_label(model: &str, source: Source) -> String {
    if model.is_empty() {
        return "(not set)".to_string();
    }
    match source {
        Source::Default => format!("{model} (default)"),
        _ => model.to_string(),
    }
}

/// The AI-provider sub-menu: one entry per applicable field (based on the
/// current provider) plus `Done`, each with its current value inline. The
/// provider itself is chosen on the screen before this menu.
fn provider_submenu_items(draft: &Draft) -> (Vec<ProviderEntry>, Vec<String>) {
    let p = draft.provider.unwrap_or(Provider::OpenAI);
    let mut entries = Vec::new();
    let mut labels = Vec::new();
    for step in applicable_steps(p) {
        match step {
            Step::Provider => {} // chosen before this menu
            Step::ApiKey => {
                // Effective key for display: the draft (a user edit or the
                // seeded config value).
                let (key, _) = resolve_api_key(draft.api_key.as_deref().filter(|k| !k.is_empty()));
                entries.push(ProviderEntry::ApiKey);
                labels.push(format!("{ICON_API_KEY} API key — {}", api_key_label(&key)));
            }
            Step::BaseUrl => {
                let (url, source) =
                    resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p);
                entries.push(ProviderEntry::BaseUrl);
                labels.push(format!(
                    "{ICON_BASE_URL} Base URL — {}",
                    base_url_label(url.as_deref(), source)
                ));
            }
            Step::Model => {
                let (model, source) = resolve_field(
                    draft.model.as_deref().filter(|m| !m.is_empty()),
                    p.default_model(),
                );
                entries.push(ProviderEntry::Model);
                labels.push(format!(
                    "{ICON_MODEL} Model — {}",
                    model_label(&model, source)
                ));
            }
        }
    }
    entries.push(ProviderEntry::Verify);
    labels.push(format!(
        "{ICON_VERIFY} Verify — test this provider with a sample request"
    ));
    entries.push(ProviderEntry::Done);
    labels.push(format!("{ICON_DONE} Done — back to main menu"));
    (entries, labels)
}

/// Render and run the top-level menu. Entering an entry routes to its
/// sub-flow; `Save & exit` finalizes; `Esc`/Ctrl-C cancels the whole setup.
/// `default_idx` is the row highlighted when the menu opens (persisted from
/// the entry the user just finished).
fn step_menu(draft: &Draft, default_idx: usize) -> Result<MenuChoice> {
    show_screen()?;
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
        format!("{ICON_SAVE} Save & exit"),
    ];
    match opt_nav("What would you like to configure?", &items, default_idx)? {
        OptNav::Value(0) => Ok(MenuChoice::Backend),
        OptNav::Value(1) => Ok(MenuChoice::Provider),
        OptNav::Value(2) => Ok(MenuChoice::CliAgent),
        OptNav::Value(3) => Ok(MenuChoice::Confirm),
        OptNav::Value(4) => Ok(MenuChoice::Save),
        OptNav::Value(_) => unreachable!("menu has exactly five entries"),
        OptNav::Back | OptNav::Cancel => Ok(MenuChoice::Cancel),
    }
}

/// Walk the provider-scoped path (Provider → key → base URL → model) until it
/// completes or the user backs out of its first step. Returns `true` when a
/// Ctrl-C cancels the whole setup; `false` means "return to the menu".
/// Entry into the AI-provider screen. First the provider is chosen (which
/// fields apply depends on it), then a sub-menu offers each applicable field —
/// API key, base URL, model — as a separate entry plus `Done`. Nothing is
/// forced: Esc on the sub-menu returns to the provider choice, Esc on the
/// provider choice returns to the main menu. Returns `true` on Ctrl-C (cancel
/// the whole setup); `false` returns to the main menu.
fn run_provider_flow(
    existing: &Option<Config>,
    existing_provider: Option<Provider>,
    draft: &mut Draft,
) -> Result<bool> {
    loop {
        // Screen 1: choose the provider. Switching clears key/url/model
        // (step_provider), which changes which sub-menu entries apply.
        match step_provider(existing_provider, draft)? {
            Nav::Next => {}
            Nav::Back => return Ok(false), // Esc on the provider choice -> main menu
            Nav::Cancel => return Ok(true),
        }
        // Screen 2: configure the provider's fields independently.
        loop {
            show_screen()?;
            let (entries, labels) = provider_submenu_items(draft);
            match opt_nav("Configure AI provider", &labels, 0)? {
                OptNav::Value(i) => {
                    let nav = match entries[i] {
                        ProviderEntry::ApiKey => step_api_key(draft)?,
                        ProviderEntry::BaseUrl => {
                            step_base_url(existing, existing_provider, draft)?
                        }
                        ProviderEntry::Model => step_model(existing, existing_provider, draft)?,
                        ProviderEntry::Verify => step_verify(draft)?,
                        ProviderEntry::Done => return Ok(false),
                    };
                    // Any non-cancel outcome returns to the sub-menu; the field
                    // steps manage their own Esc-back-to-options internally.
                    match nav {
                        Nav::Next | Nav::Back => {}
                        Nav::Cancel => return Ok(true),
                    }
                }
                OptNav::Back => break, // Esc on the sub-menu -> re-choose provider
                OptNav::Cancel => return Ok(true),
            }
        }
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

/// Best-effort check that `program` is installed. Runs `program --version`
/// with **stdin detached** and a hard 3 s cap, so a misconfigured custom CLI
/// that ignores `--version` and tries to read stdin or enter an interactive
/// loop cannot hang `aic setup`. Never blocks longer than the cap; a miss or
/// timeout yields a warning the user can ignore (ADR 0010 — aic never installs
/// or authenticates a CLI on the user's behalf). Returns a human one-liner.
fn smoke_check(program: &str) -> String {
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

/// Configure the CLI-agent backend: pick a preset (claude/codex/pi) or enter a
/// custom command + args template. Choosing a CLI backend is mutually
/// exclusive with the API provider path — [`finalize`] drops the provider
/// fields when a command is set. Returns `true` only on Ctrl-C (cancel setup).
fn run_cli_flow(draft: &mut Draft) -> Result<bool> {
    loop {
        show_screen()?;
        let preset_labels: Vec<String> = PRESETS
            .iter()
            .map(|name| {
                let spec = cli_preset(name).unwrap();
                format!("{name} — `{} {}`", spec.command, spec.args.join(" "))
            })
            .collect();
        let mut items = vec!["Choose a preset:".to_string()];
        // Clippy: collect then extend to avoid borrowing preset_labels across the closure.
        items.extend(preset_labels.iter().cloned());
        items.push("Custom command…".to_string());
        let command_set = draft.active_cli_command().is_some();
        if command_set {
            items.push("Clear CLI backend (use API key instead)".to_string());
        }
        if command_set {
            // Probe install + auth now (mirrors the API path's Verify item) so
            // an installed-but-unauthenticated CLI is caught at setup, not
            // mid-Run (ADR 0010 reliability).
            items.push("Verify (probe the CLI now)".to_string());
        }
        items.push("↩️ Done — back to main menu".to_string());
        let n_presets = PRESETS.len();
        let custom_idx = 1 + n_presets;
        // Running index after presets + custom; clear & verify are both gated
        // on a command being set, so they shift done_idx only then.
        let mut idx = custom_idx + 1;
        let clear_idx = if command_set {
            let i = idx;
            idx += 1;
            Some(i)
        } else {
            None
        };
        let verify_idx = if command_set {
            let i = idx;
            idx += 1;
            Some(i)
        } else {
            None
        };
        let done_idx = idx;

        match opt_nav("CLI agent backend", &items, 0)? {
            OptNav::Back => return Ok(false),
            OptNav::Cancel => return Ok(true),
            OptNav::Value(i) => {
                // Preset rows live at indices 1..=n_presets.
                if i >= 1 && i <= n_presets {
                    let name = PRESETS[i - 1];
                    let spec = cli_preset(name).unwrap();
                    println!("\n{}", smoke_check(&spec.command));
                    draft.cli_command = Some(spec.command);
                    draft.cli_args = Some(spec.args);
                    draft.cli_timeout_secs = Some(spec.timeout_secs);
                    pause_done()?;
                    return Ok(false);
                }
                if i == custom_idx {
                    if step_custom_cli(draft)? == Nav::Cancel {
                        return Ok(true);
                    }
                    continue;
                }
                if clear_idx == Some(i) {
                    draft.cli_command = None;
                    draft.cli_args = None;
                    draft.cli_timeout_secs = None;
                    println!("\nCLI backend cleared — aic will use the API provider.");
                    pause_done()?;
                    return Ok(false);
                }
                if verify_idx == Some(i) {
                    if step_verify_cli(draft)? == Nav::Cancel {
                        return Ok(true);
                    }
                    continue;
                }
                if i == done_idx {
                    return Ok(false);
                }
                unreachable!("unmapped CLI menu row {i}");
            }
        }
    }
}

/// Enter a custom command + args template + timeout. Args defaults to the
/// `{prompt}` placeholder; an empty submit keeps the existing value.
fn step_custom_cli(draft: &mut Draft) -> Result<Nav> {
    show_screen()?;
    let initial_cmd = draft.cli_command.as_deref().unwrap_or("");
    let command = match prompt_text(
        "Command (e.g. claude, codex, pi)",
        if initial_cmd.is_empty() {
            None
        } else {
            Some(initial_cmd)
        },
        false,
        "command is required",
    )? {
        TextAct::Value(v) => v.trim().to_string(),
        TextAct::Back => return Ok(Nav::Back),
        TextAct::Cancel => return Ok(Nav::Cancel),
    };
    let initial_args = draft
        .cli_args
        .as_ref()
        .map(|a| a.join(" "))
        .unwrap_or_else(|| PROMPT_PLACEHOLDER.to_string());
    let args_str = match prompt_text(
        &format!("Args template (space-separated, use {PROMPT_PLACEHOLDER} for the prompt)"),
        Some(&initial_args),
        true,
        "",
    )? {
        TextAct::Value(v) if v.trim().is_empty() => initial_args.clone(),
        TextAct::Value(v) => v.trim().to_string(),
        TextAct::Back => return Ok(Nav::Back),
        TextAct::Cancel => return Ok(Nav::Cancel),
    };
    let args: Vec<String> = split_args(&args_str);
    let initial_to = draft
        .cli_timeout_secs
        .map(|t| t.to_string())
        .unwrap_or_else(|| DEFAULT_TIMEOUT_SECS.to_string());
    let to_str = match prompt_text("Timeout in seconds", Some(&initial_to), true, "")? {
        TextAct::Value(v) if v.trim().is_empty() => initial_to.clone(),
        TextAct::Value(v) => v.trim().to_string(),
        TextAct::Back => return Ok(Nav::Back),
        TextAct::Cancel => return Ok(Nav::Cancel),
    };
    let timeout_secs = to_str.trim().parse::<u64>().unwrap_or(DEFAULT_TIMEOUT_SECS);
    println!("\n{}", smoke_check(&command));
    draft.cli_command = Some(command);
    draft.cli_args = Some(args);
    draft.cli_timeout_secs = Some(timeout_secs);
    pause_done()?;
    Ok(Nav::Next)
}

/// Wait for Enter so a smoke-check / status message stays visible before the
/// screen redraws. Best-effort: a non-interactive stdin just continues.
fn pause_done() -> Result<()> {
    use std::io::BufRead;
    eprint!("\nPress Enter to continue… ");
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);
    Ok(())
}

/// Whitespace-only splitter for the custom-CLI args template. This is **not**
/// shell parsing: quotes are not honored and multi-word values cannot be
/// expressed as a single argument. Use it only for simple flags where each
/// whitespace-delimited token is one argv element, and let `{prompt}` carry
/// the full prompt as one arg anyway. (Avoids pulling in a shell-parsing crate
/// for a handful of simple tokens.)
fn split_args(s: &str) -> Vec<String> {
    s.split_whitespace().map(String::from).collect()
}

fn finalize(draft: Draft) -> Config {
    // Both backends keep their configured fields, so switching the active
    // Backend never wipes what was entered for the other. `backend_kind`
    // selects which Backend a Run actually uses; the inactive one's fields
    // stay dormant on disk and are restored when you switch back (ADR 0011).
    let active = draft.active_backend();
    let command = draft.active_cli_command().map(str::to_owned);
    let has_cli = command.is_some();

    // `backend_kind` is written whenever it is non-default (CLI) or a dormant
    // CLI command is present, so the discriminator always disambiguates a
    // config that carries both backends' fields. For a pure API config (API
    // active, no command) it stays absent — byte-identical to before this
    // field existed, so released configs need no migration.
    let backend_kind = match active {
        BackendKind::Cli => Some(BackendKind::Cli),
        BackendKind::Api if has_cli => Some(BackendKind::Api),
        BackendKind::Api => None,
    };

    // When the API Backend is active, default a missing provider to OpenAI
    // (historical behavior). When it is dormant (CLI active), preserve the
    // draft's value verbatim so switching back restores it.
    let backend = match active {
        BackendKind::Api => Some(draft.provider.unwrap_or(Provider::OpenAI).name().to_string()),
        BackendKind::Cli => draft.provider.map(|p| p.name().to_string()),
    };

    // CLI fields are a unit (command + args + timeout); only persist them when
    // a command is set, so an unconfigured CLI leaves no orphaned keys.
    let (args, timeout_secs) = if has_cli {
        (draft.cli_args, draft.cli_timeout_secs)
    } else {
        (None, None)
    };

    Config {
        backend_kind,
        backend,
        api_key: draft.api_key,
        model: draft.model,
        base_url: draft.base_url,
        confirm_before_commit: draft.confirm_before_commit,
        command,
        args,
        timeout_secs,
    }
}

/// Effective initial value for one field, in precedence order: the in-session
/// draft value first, then the existing-config value when the provider is
/// unchanged, else none. `field` selects which `Config` column to read, so the
/// one shared body replaces the old per-field `key/base_url/model_initial`
/// triple that were byte-for-byte apart from the field they touched.
fn field_initial(
    draft_val: Option<&str>,
    existing: &Option<Config>,
    existing_provider: Option<Provider>,
    draft_provider: Option<Provider>,
    field: impl Fn(&Config) -> Option<&String>,
) -> Option<String> {
    if let Some(v) = draft_val {
        return Some(v.to_string());
    }
    if existing_provider.is_some() && existing_provider == draft_provider {
        return existing.as_ref().and_then(field).cloned();
    }
    None
}

fn step_provider(existing_provider: Option<Provider>, draft: &mut Draft) -> Result<Nav> {
    show_screen()?;
    let providers = Provider::all();
    let items: Vec<String> = providers
        .iter()
        .map(|&p| provider_choice_label(p, draft))
        .collect();
    let default_idx = draft
        .provider
        .or(existing_provider)
        .and_then(|ep| providers.iter().position(|p| *p == ep))
        .unwrap_or(0);

    match opt_nav("Choose your AI provider", &items, default_idx)? {
        OptNav::Value(i) => {
            let chosen = providers[i];
            // Switching provider invalidates previously entered key/url/model.
            if draft.provider != Some(chosen) {
                draft.api_key = None;
                draft.base_url = None;
                draft.model = None;
            }
            draft.provider = Some(chosen);
            Ok(Nav::Next)
        }
        OptNav::Back => Ok(Nav::Back),
        OptNav::Cancel => Ok(Nav::Cancel),
    }
}

fn step_api_key(draft: &mut Draft) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before api key");
    // Effective key for editing + keep semantics: the draft (a user edit or
    // the seeded config value).
    let (key, _) = resolve_api_key(draft.api_key.as_deref().filter(|k| !k.is_empty()));
    match provider.requires_key() {
        // Cloud provider — key required. A single visible, editable prompt:
        // type a new key (shown in the clear) or leave it blank to keep the
        // current one. No masked input and no keep/replace menu, so the user
        // always sees what they enter — the masked Password left no visible
        // field and gave no feedback after pasting.
        true => {
            let prompt = if key.is_empty() {
                "API key".to_string()
            } else {
                format!(
                    "API key (current: {} — leave blank to keep)",
                    mask_key(&key)
                )
            };
            // Blank is accepted only when there is a current key to keep;
            // otherwise the validator enforces a non-empty entry.
            let allow_empty = !key.is_empty();
            show_screen()?;
            match prompt_text(&prompt, None, allow_empty, "API key cannot be empty")? {
                TextAct::Value(v) => {
                    let v = if v.is_empty() { key } else { v };
                    draft.api_key = Some(v);
                    Ok(Nav::Next)
                }
                TextAct::Back => Ok(Nav::Back),
                TextAct::Cancel => Ok(Nav::Cancel),
            }
        }
        // OpenAI-compatible — key optional (keyless servers allowed). "No key"
        // is a first-class choice a single text field cannot express, so keep
        // it as a menu option; but enter the key in the clear (not masked) so
        // the user sees what they type.
        false if provider == Provider::OpenAiCompatible => {
            let has_key = !key.is_empty();
            let items: Vec<String> = if has_key {
                vec![
                    format!("Keep current key ({})", mask_key(&key)),
                    "Enter a new key…".to_string(),
                    "No API key (keyless server)".to_string(),
                ]
            } else {
                vec![
                    "No API key (keyless server)".to_string(),
                    "Enter API key…".to_string(),
                ]
            };
            let no_key_idx = if has_key { 2 } else { 0 };
            let enter_idx = 1;
            // Row 0 is "Keep current key" only when a key is already set; with
            // no key, row 0 is the "No API key" option instead.
            let keep_idx = if has_key { Some(0) } else { None };
            loop {
                show_screen()?;
                match opt_nav("API key", &items, 0)? {
                    OptNav::Value(i) if keep_idx == Some(i) => {
                        // Keep current key — draft.api_key already holds it.
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == no_key_idx => {
                        draft.api_key = None;
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == enter_idx => {
                        show_screen()?;
                        match prompt_text("API key (blank = keyless server)", None, true, "")? {
                            TextAct::Value(v) => {
                                draft.api_key = if v.is_empty() { None } else { Some(v) };
                                return Ok(Nav::Next);
                            }
                            TextAct::Back => continue,
                            TextAct::Cancel => return Ok(Nav::Cancel),
                        }
                    }
                    OptNav::Value(_) => unreachable!("api key options"),
                    OptNav::Back => return Ok(Nav::Back),
                    OptNav::Cancel => return Ok(Nav::Cancel),
                }
            }
        }
        // Ollama — unreachable (step skipped via key_applies); defensive.
        false => {
            draft.api_key = None;
            Ok(Nav::Next)
        }
    }
}

fn step_base_url(
    existing: &Option<Config>,
    ep: Option<Provider>,
    draft: &mut Draft,
) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before base url");
    let initial = field_initial(
        draft.base_url.as_deref(),
        existing,
        ep,
        draft.provider,
        |c| c.base_url.as_ref(),
    );
    match provider.base_url_requirement() {
        BaseUrlRequirement::Required => {
            show_screen()?;
            match prompt_text(
                "Base URL (e.g. http://localhost:1234/v1)",
                initial.as_deref(),
                false,
                "base URL cannot be empty",
            )? {
                TextAct::Value(v) => {
                    draft.base_url = Some(v);
                    Ok(Nav::Next)
                }
                TextAct::Back => Ok(Nav::Back),
                TextAct::Cancel => Ok(Nav::Cancel),
            }
        }
        BaseUrlRequirement::Optional(default) => {
            let (effective, source) = resolve_base_url(
                draft.base_url.as_deref().filter(|u| !u.is_empty()),
                &provider,
            );
            let has_url = matches!(source, Source::Config);
            let current = effective.as_deref().unwrap_or(default);
            let items: Vec<String> = if has_url {
                vec![
                    format!("Keep current URL ({current})"),
                    format!("Use default ({default})"),
                    "Enter custom URL…".to_string(),
                ]
            } else {
                vec![
                    format!("Use default ({default})"),
                    "Enter custom URL…".to_string(),
                ]
            };
            let use_default_idx = if has_url { 1 } else { 0 };
            let custom_idx = if has_url { 2 } else { 1 };
            // Row 0 is "Keep current URL" only when a URL is already set; with
            // none, row 0 is the "Use default" option instead.
            let keep_idx = if has_url { Some(0) } else { None };
            loop {
                show_screen()?;
                match opt_nav("Base URL", &items, 0)? {
                    OptNav::Value(i) if keep_idx == Some(i) => {
                        // Keep current URL — draft.base_url already holds it.
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == use_default_idx => {
                        draft.base_url = None;
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == custom_idx => {
                        show_screen()?;
                        match prompt_text(
                            &format!("Custom base URL (e.g. {default})"),
                            None,
                            false,
                            "base URL cannot be empty",
                        )? {
                            TextAct::Value(v) => {
                                draft.base_url = Some(v);
                                return Ok(Nav::Next);
                            }
                            TextAct::Back => continue,
                            TextAct::Cancel => return Ok(Nav::Cancel),
                        }
                    }
                    OptNav::Value(_) => unreachable!("base url options"),
                    OptNav::Back => return Ok(Nav::Back),
                    OptNav::Cancel => return Ok(Nav::Cancel),
                }
            }
        }
        // Unreachable (step skipped via base_url_applies); defensive.
        BaseUrlRequirement::None => Ok(Nav::Next),
    }
}

fn step_model(existing: &Option<Config>, ep: Option<Provider>, draft: &mut Draft) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before model");
    let default_model = provider.default_model();
    let models = provider.models();
    let initial = field_initial(draft.model.as_deref(), existing, ep, draft.provider, |c| {
        c.model.as_ref()
    });

    // No curated list (OpenRouter, OpenAI-compatible) -> required free text.
    if models.is_empty() {
        show_screen()?;
        return match prompt_text(
            "Model (required)",
            initial.as_deref(),
            false,
            "model cannot be empty",
        )? {
            TextAct::Value(v) => {
                draft.model = Some(v);
                Ok(Nav::Next)
            }
            TextAct::Back => Ok(Nav::Back),
            TextAct::Cancel => Ok(Nav::Cancel),
        };
    }

    let mut items: Vec<String> = models.iter().map(|m| (*m).to_string()).collect();
    items.push("✏️  Custom model…".to_string());
    let custom_idx = items.len() - 1;
    let highlight = initial
        .as_deref()
        .and_then(|v| models.iter().position(|m| *m == v))
        .unwrap_or_else(|| {
            if default_model.is_empty() {
                0
            } else {
                models.iter().position(|m| *m == default_model).unwrap_or(0)
            }
        });

    loop {
        show_screen()?;
        match opt_nav("Model", &items, highlight)? {
            OptNav::Value(i) if i == custom_idx => {
                show_screen()?;
                match prompt_text("Custom model", None, false, "model cannot be empty")? {
                    TextAct::Value(v) => {
                        draft.model = Some(v);
                        return Ok(Nav::Next);
                    }
                    // "Custom model…" is a sub-mode of the Model step, not a
                    // separate step: Esc cancels the custom entry and returns to
                    // the model picker (Esc there then leaves the step), so the
                    // "Esc goes back on every step" invariant holds at step
                    // granularity.
                    TextAct::Back => continue,
                    TextAct::Cancel => return Ok(Nav::Cancel),
                }
            }
            OptNav::Value(i) => {
                let m = models[i];
                draft.model = if !default_model.is_empty() && m == default_model {
                    None
                } else {
                    Some(m.to_string())
                };
                return Ok(Nav::Next);
            }
            OptNav::Back => return Ok(Nav::Back),
            OptNav::Cancel => return Ok(Nav::Cancel),
        }
    }
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
fn step_verify(draft: &Draft) -> Result<Nav> {
    let p = draft.provider.unwrap_or(Provider::OpenAI);
    // Effective values: the draft (a user edit or the seeded config value),
    // then the default.
    let api_key = resolve_api_key(draft.api_key.as_deref().filter(|k| !k.is_empty())).0;
    let base_url = resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p).0;
    let model = resolve_field(
        draft.model.as_deref().filter(|m| !m.is_empty()),
        p.default_model(),
    )
    .0;

    // Pre-flight validation mirrors ResolvedConfig::validate so a missing
    // required field reads as a setup hint, not an opaque provider error.
    if let Err(e) = verify_preflight(p, base_url.as_deref(), &model) {
        show_verify_result(&p, &model, &api_key, base_url.as_deref(), Err(e))?;
        return Ok(Nav::Next);
    }

    let llm = crate::llm::LLM {
        provider: p,
        model: model.clone(),
        api_key: api_key.clone(),
        base_url: base_url.clone(),
    };
    let label = format!("Contacting {} ({model})…", p.display());
    // block_in_place parks the outer multi-thread task; the nested
    // current-thread runtime then drives the async verify future. Without
    // block_in_place, Runtime::block_on panics inside an active runtime.
    let result = tokio::task::block_in_place(|| -> Result<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build verify runtime")?;
        rt.block_on(crate::progress::with_spinner(&label, async {
            llm.agent("You are a connectivity checker. Follow the user's instruction exactly.")
                .verify()
                .await
        }))
    });

    show_verify_result(&p, &model, &api_key, base_url.as_deref(), result)?;
    Ok(Nav::Next)
}

/// Pre-flight checks for Verify: a provider whose base URL is required must
/// have one, and the model must be set. Catches missing-field misconfigurations
/// before they become opaque provider/HTTP errors.
fn verify_preflight(p: Provider, base_url: Option<&str>, model: &str) -> Result<()> {
    if p.base_url_requirement() == BaseUrlRequirement::Required && base_url.is_none() {
        anyhow::bail!(
            "the {} provider requires a base URL — set one in this menu first",
            p.display()
        );
    }
    if model.trim().is_empty() {
        anyhow::bail!(
            "no model is set — pick one in this menu first ({} has no default model)",
            p.display()
        );
    }
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
    let term = Term::stdout();
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
/// as the matching [`LlmError`](crate::llm::LlmError); success reports the
/// trimmed reply.
///
/// Runs on a dedicated current-thread runtime like [`step_verify`] — `aic
/// setup` is already inside `#[tokio::main]`, so `block_in_place` parks the
/// outer task while the nested runtime drives the async probe.
fn step_verify_cli(draft: &Draft) -> Result<Nav> {
    let command = match draft.active_cli_command() {
        Some(c) => c.to_string(),
        None => {
            // Defensive: the menu only offers Verify when a command is set.
            show_cli_verify_result(Err(anyhow::anyhow!("no CLI command is set yet")))?;
            return Ok(Nav::Next);
        }
    };
    let args = draft
        .cli_args
        .clone()
        .unwrap_or_else(|| vec![crate::cli_agent::PROMPT_PLACEHOLDER.to_string()]);
    let timeout_secs = draft
        .cli_timeout_secs
        .unwrap_or(crate::cli_agent::DEFAULT_TIMEOUT_SECS);
    let spec = crate::cli_agent::CliSpec {
        command,
        args,
        timeout_secs,
    };
    let label = format!("Probing `{}` (print mode)…", spec.command);
    let agent = crate::cli_agent::CliAgent::new(
        spec,
        "You are a connectivity checker. Follow the user's instruction exactly.".to_string(),
    );
    let result = tokio::task::block_in_place(|| -> Result<String> {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .context("failed to build verify runtime")?;
        rt.block_on(crate::progress::with_spinner(&label, async {
            agent.verify().await
        }))
    });
    show_cli_verify_result(result)?;
    Ok(Nav::Next)
}

/// Render the CLI Verify result on a fresh screen and pause for a keypress,
/// mirroring [`show_verify_result`] for the API path. `result` carries the
/// trimmed reply on success, or the propagated
/// [`LlmError`](crate::llm::LlmError) on failure (its `Display` already
/// carries a human hint).
fn show_cli_verify_result(result: Result<String>) -> Result<()> {
    let term = Term::stdout();
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
    show_screen()?;
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

/// Mask a secret for the prompt hint and pre-save summary. Never reveals the
/// full value.
fn mask_key(k: &str) -> String {
    if k.is_empty() {
        return "(empty)".to_string();
    }
    "•".repeat(k.len().min(12))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(
        backend: &str,
        api_key: Option<&str>,
        model: Option<&str>,
        base_url: Option<&str>,
    ) -> Config {
        Config {
            backend: Some(backend.to_string()),
            api_key: api_key.map(String::from),
            model: model.map(String::from),
            base_url: base_url.map(String::from),
            confirm_before_commit: None,
            ..Default::default()
        }
    }

    fn draft(
        provider: Option<Provider>,
        api_key: Option<&str>,
        model: Option<&str>,
        base_url: Option<&str>,
    ) -> Draft {
        Draft {
            provider,
            api_key: api_key.map(String::from),
            model: model.map(String::from),
            base_url: base_url.map(String::from),
            confirm_before_commit: None,
            ..Default::default()
        }
    }

    #[test]
    fn key_and_base_url_applicability() {
        // Cloud provider: key applies, no base URL.
        assert!(key_applies(Provider::OpenAI));
        assert!(!base_url_applies(Provider::OpenAI));
        // Local Ollama: no key, optional base URL.
        assert!(!key_applies(Provider::Ollama));
        assert!(base_url_applies(Provider::Ollama));
        // OpenAI-compatible: optional key (keyless servers) + required base URL.
        assert!(key_applies(Provider::OpenAiCompatible));
        assert!(base_url_applies(Provider::OpenAiCompatible));
    }

    #[test]
    fn applicable_steps_skip_no_op_steps() {
        // OpenAI has a key but no base URL -> ApiKey present, BaseUrl absent.
        assert_eq!(
            applicable_steps(Provider::OpenAI),
            vec![Step::Provider, Step::ApiKey, Step::Model]
        );
        // Ollama has no key but a base URL -> BaseUrl present, ApiKey absent.
        assert_eq!(
            applicable_steps(Provider::Ollama),
            vec![Step::Provider, Step::BaseUrl, Step::Model]
        );
        // OpenAI-compatible needs both.
        assert_eq!(
            applicable_steps(Provider::OpenAiCompatible),
            vec![Step::Provider, Step::ApiKey, Step::BaseUrl, Step::Model]
        );
    }

    #[test]
    fn applicable_steps_always_bracketed_and_unique() {
        // Every provider's list starts at Provider and ends at Model, so back
        // never escapes past the first step and forward always reaches the end
        // of the provider path. No step repeats.
        for p in Provider::all() {
            let steps = applicable_steps(*p);
            assert_eq!(
                steps.first(),
                Some(&Step::Provider),
                "{p:?} missing Provider"
            );
            assert_eq!(steps.last(), Some(&Step::Model), "{p:?} missing Model");
            assert_eq!(
                steps.iter().filter(|s| **s == Step::Provider).count(),
                1,
                "{p:?} has a duplicate Provider"
            );
        }
    }

    #[test]
    fn seed_draft_carries_existing_config() {
        let existing = Some(cfg("openai", Some("k"), Some("m"), None));
        let d = seed_draft(&existing);
        assert_eq!(d.provider, Some(Provider::OpenAI));
        assert_eq!(d.api_key.as_deref(), Some("k"));
        assert_eq!(d.model.as_deref(), Some("m"));
        assert_eq!(d.base_url, None);
        assert_eq!(d.confirm_before_commit, None);

        // Fresh install -> all None (finalize then defaults to OpenAI).
        let fresh = seed_draft(&None);
        assert_eq!(fresh.provider, None);
        assert_eq!(fresh.api_key, None);
        assert_eq!(fresh.confirm_before_commit, None);
    }

    #[test]
    fn seed_draft_carries_confirm_before_commit() {
        let existing = Some(Config {
            backend: None,
            api_key: None,
            model: None,
            base_url: None,
            confirm_before_commit: Some(true),
            ..Default::default()
        });
        let d = seed_draft(&existing);
        assert_eq!(d.confirm_before_commit, Some(true));
        assert_eq!(d.provider, None);
    }

    #[test]
    fn provider_label_shows_not_set_when_unconfigured() {
        let d = draft(None, None, None, None);
        assert_eq!(provider_label(&d), "(not set)");
    }

    #[test]
    fn provider_label_shows_provider_and_model() {
        // Explicit model wins.
        let d = draft(Some(Provider::OpenAI), None, Some("gpt-5"), None);
        assert_eq!(provider_label(&d), "OpenAI · gpt-5");
        // No explicit model -> provider default.
        let d = draft(Some(Provider::OpenAI), None, None, None);
        assert_eq!(provider_label(&d), "OpenAI · gpt-5-mini");
        // Provider with no default model -> just the provider name.
        let d = draft(Some(Provider::OpenRouter), None, None, None);
        assert_eq!(provider_label(&d), "OpenRouter");
    }

    #[test]
    fn confirm_label_defaults_off_and_reflects_choice() {
        assert_eq!(confirm_label(&draft(None, None, None, None)), "no");
        let mut d = draft(None, None, None, None);
        d.confirm_before_commit = Some(true);
        assert_eq!(confirm_label(&d), "yes");
        d.confirm_before_commit = Some(false);
        assert_eq!(confirm_label(&d), "no");
    }

    #[test]
    fn submenu_labels_reflect_value_and_source() {
        // API key: masked, (not set) when empty.
        assert_eq!(api_key_label("sk-123"), "••••••");
        assert_eq!(api_key_label(""), "(not set)");

        // Model: value, (default) when provider-default-sourced, (not set) when empty.
        assert_eq!(model_label("gpt-5", Source::Config), "gpt-5");
        assert_eq!(
            model_label("deepseek-v4-flash", Source::Default),
            "deepseek-v4-flash (default)"
        );
        assert_eq!(model_label("", Source::Default), "(not set)");

        // Base URL: value, annotated by source, (not set) when none.
        assert_eq!(
            base_url_label(Some("http://h:1"), Source::Config),
            "http://h:1"
        );
        assert_eq!(
            base_url_label(Some("http://localhost:11434"), Source::Default),
            "http://localhost:11434 (default)"
        );
        assert_eq!(base_url_label(None, Source::Default), "(not set)");
    }

    #[test]
    fn provider_submenu_entries_follow_applicability() {
        // OpenAI: API key + Model + Verify + Done (no base URL).
        let d = draft(Some(Provider::OpenAI), None, None, None);
        let (entries, _) = provider_submenu_items(&d);
        assert_eq!(
            entries,
            vec![
                ProviderEntry::ApiKey,
                ProviderEntry::Model,
                ProviderEntry::Verify,
                ProviderEntry::Done
            ]
        );

        // Ollama: Base URL + Model + Verify + Done (no API key).
        let d = draft(Some(Provider::Ollama), None, None, None);
        let (entries, _) = provider_submenu_items(&d);
        assert_eq!(
            entries,
            vec![
                ProviderEntry::BaseUrl,
                ProviderEntry::Model,
                ProviderEntry::Verify,
                ProviderEntry::Done
            ]
        );

        // OpenAI-compatible: API key + Base URL + Model + Verify + Done.
        let d = draft(Some(Provider::OpenAiCompatible), None, None, None);
        let (entries, _) = provider_submenu_items(&d);
        assert_eq!(
            entries,
            vec![
                ProviderEntry::ApiKey,
                ProviderEntry::BaseUrl,
                ProviderEntry::Model,
                ProviderEntry::Verify,
                ProviderEntry::Done
            ]
        );
    }

    #[test]
    fn submenu_labels_show_effective_value() {
        // The in-session draft (a user choice or a seeded config value) is the
        // effective value and is shown as-is — re-entering setup must not read
        // as if the choice was lost (AIC-15).
        let d = draft(
            Some(Provider::DeepSeek),
            Some("sk-123"),
            Some("deepseek-v4-pro"),
            None,
        );
        let (entries, labels) = provider_submenu_items(&d);
        assert_eq!(
            entries,
            vec![
                ProviderEntry::ApiKey,
                ProviderEntry::Model,
                ProviderEntry::Verify,
                ProviderEntry::Done
            ]
        );
        assert_eq!(labels[0], "🔑 API key — ••••••");
        assert_eq!(labels[1], "🧠 Model — deepseek-v4-pro");
        assert_eq!(
            labels[2],
            "🔌 Verify — test this provider with a sample request"
        );
        assert_eq!(labels[3], "↩️ Done — back to main menu");
    }

    #[test]
    fn effective_model_prefers_draft_then_default() {
        // Draft model wins over the provider default.
        let d = draft(Some(Provider::OpenAI), None, Some("gpt-5"), None);
        assert_eq!(effective_model(Provider::OpenAI, &d), "gpt-5");
        // No draft model -> provider default.
        let d = draft(Some(Provider::OpenAI), None, None, None);
        assert_eq!(effective_model(Provider::OpenAI, &d), "gpt-5-mini");
        // Provider with no default -> empty string.
        let d = draft(Some(Provider::OpenRouter), None, None, None);
        assert_eq!(effective_model(Provider::OpenRouter, &d), "");
    }

    #[test]
    fn provider_choice_label_shows_chosen_model_for_selected_provider() {
        // The selected provider shows the user's chosen model, not the bare
        // default — re-entering setup must not read as if the choice was lost
        // (AIC-15).
        let mut d = draft(Some(Provider::DeepSeek), None, None, None);
        d.model = Some("deepseek-v4-pro".into());
        assert_eq!(
            provider_choice_label(Provider::DeepSeek, &d),
            "DeepSeek  (deepseek-v4-pro)"
        );

        // Other providers still show their default for comparison.
        assert_eq!(
            provider_choice_label(Provider::OpenAI, &d),
            "OpenAI  (gpt-5-mini)"
        );

        // No chosen model -> the provider default for the selected provider.
        let d = draft(Some(Provider::DeepSeek), None, None, None);
        assert_eq!(
            provider_choice_label(Provider::DeepSeek, &d),
            "DeepSeek  (deepseek-v4-flash)"
        );

        // Selected provider with no default and a chosen model -> chosen model.
        let mut d = draft(Some(Provider::OpenRouter), None, None, None);
        d.model = Some("meta-llama/llama-4-scout".into());
        assert_eq!(
            provider_choice_label(Provider::OpenRouter, &d),
            "OpenRouter  (meta-llama/llama-4-scout)"
        );

        // Selected provider with no default and no chosen model -> the hint.
        let d = draft(Some(Provider::OpenRouter), None, None, None);
        assert_eq!(
            provider_choice_label(Provider::OpenRouter, &d),
            "OpenRouter  (no default — you'll pick a model)"
        );
    }

    #[test]
    fn field_initial_precedence() {
        let existing: Option<Config> =
            Some(cfg("openai", Some("old-key"), Some("old-model"), None));
        let key = |d: &Draft, ex: &Option<Config>, ep: Option<Provider>| {
            field_initial(d.api_key.as_deref(), ex, ep, d.provider, |c| {
                c.api_key.as_ref()
            })
        };

        // 1. Draft value wins over the existing config value.
        let d = draft(Some(Provider::OpenAI), Some("draft-key"), None, None);
        assert_eq!(
            key(&d, &existing, Some(Provider::OpenAI)),
            Some("draft-key".to_string())
        );

        // 2. No draft value, same provider -> reuse the existing config value.
        let d = draft(Some(Provider::OpenAI), None, None, None);
        assert_eq!(
            key(&d, &existing, Some(Provider::OpenAI)),
            Some("old-key".to_string())
        );

        // 3. No draft value, provider changed -> old value is invalid, no reuse.
        let d = draft(Some(Provider::Anthropic), None, None, None);
        assert_eq!(key(&d, &existing, Some(Provider::OpenAI)), None);

        // 4. No draft value and no existing config at all.
        let d = draft(Some(Provider::OpenAI), None, None, None);
        assert_eq!(key(&d, &None, None), None);
    }

    #[test]
    fn finalize_defaults_provider_and_carries_fields() {
        // No provider chosen -> defaults to OpenAI; other fields carried.
        let out = finalize(draft(None, Some("k"), Some("m"), None));
        assert_eq!(out.backend.as_deref(), Some("openai"));
        assert_eq!(out.api_key.as_deref(), Some("k"));
        assert_eq!(out.model.as_deref(), Some("m"));
        assert_eq!(out.base_url, None);

        // A chosen provider wins and base_url round-trips.
        let out = finalize(draft(
            Some(Provider::Ollama),
            None,
            None,
            Some("http://host:11434"),
        ));
        assert_eq!(out.backend.as_deref(), Some("ollama"));
        assert_eq!(out.base_url.as_deref(), Some("http://host:11434"));
    }

    #[test]
    fn finalize_carries_confirm_before_commit() {
        // Untouched (None) when the user never visited the step — config stays
        // absent, so the runtime default remains off.
        let out = finalize(draft(Some(Provider::OpenAI), None, None, None));
        assert_eq!(out.confirm_before_commit, None);

        // Explicit choice round-trips into the written config.
        let mut d = draft(Some(Provider::OpenAI), None, None, None);
        d.confirm_before_commit = Some(true);
        let out = finalize(d);
        assert_eq!(out.confirm_before_commit, Some(true));
    }

    #[test]
    fn confirm_initial_prefers_draft_then_existing_then_false() {
        let existing: Option<Config> = Some(Config {
            backend: None,
            api_key: None,
            model: None,
            base_url: None,
            confirm_before_commit: Some(true),
            ..Default::default()
        });

        // No draft, existing true -> true.
        let d = draft(Some(Provider::OpenAI), None, None, None);
        assert!(confirm_initial(&d, &existing));

        // No draft, no existing -> false (default off).
        assert!(!confirm_initial(&d, &None));

        // Draft choice wins over existing.
        let mut d = draft(Some(Provider::OpenAI), None, None, None);
        d.confirm_before_commit = Some(false);
        assert!(!confirm_initial(&d, &existing));
        d.confirm_before_commit = Some(true);
        assert!(confirm_initial(&d, &None));
    }

    #[test]
    fn resolve_api_key_uses_config_value() {
        // aic reads only the config file: the config value is used as-is.
        let (key, source) = resolve_api_key(Some("sk-config"));
        assert_eq!(key, "sk-config");
        assert_eq!(source, Source::Config);
        // No config value -> empty, default source.
        let (key, source) = resolve_api_key(None);
        assert_eq!(key, "");
        assert_eq!(source, Source::Default);
    }

    #[test]
    fn verify_preflight_requires_base_url_and_model() {
        // OpenAI-compatible requires a base URL — without one it fails with a
        // readable hint, before any network call.
        let err = verify_preflight(Provider::OpenAiCompatible, None, "m").unwrap_err();
        assert!(err.to_string().contains("base URL"));
        // With a URL + model it is fine.
        assert!(verify_preflight(Provider::OpenAiCompatible, Some("http://h/v1"), "m").is_ok());

        // OpenRouter has no default model — an empty model fails with a hint.
        let err = verify_preflight(Provider::OpenRouter, None, "").unwrap_err();
        assert!(err.to_string().contains("model"));

        // OpenAI needs no base URL and carries a default model — ok.
        assert!(verify_preflight(Provider::OpenAI, None, "gpt-5-mini").is_ok());
    }

    fn draft_with_cli(command: Option<&str>) -> Draft {
        Draft {
            backend_kind: command.map(|_| BackendKind::Cli),
            provider: Some(Provider::OpenAI),
            api_key: Some("sk-stale".into()),
            model: Some("gpt-5".into()),
            base_url: None,
            confirm_before_commit: Some(true),
            cli_command: command.map(String::from),
            cli_args: Some(vec!["-p".into(), "{prompt}".into()]),
            cli_timeout_secs: Some(90),
        }
    }

    #[test]
    fn finalize_cli_backend_preserves_dormant_provider_fields() {
        // backend_kind = cli is active, but the API-provider fields are kept
        // dormant on disk so switching back to the API Backend restores them
        // (ADR 0011) — switching never wipes the other Backend's config.
        let cfg = finalize(draft_with_cli(Some("claude")));
        // CLI Backend is active:
        assert_eq!(cfg.command.as_deref(), Some("claude"));
        assert_eq!(
            cfg.args.as_deref(),
            Some(&["-p".to_string(), "{prompt}".to_string()][..])
        );
        assert_eq!(cfg.timeout_secs, Some(90));
        assert_eq!(cfg.backend_kind, Some(BackendKind::Cli));
        // API-provider fields preserved dormant (not dropped):
        assert_eq!(cfg.backend.as_deref(), Some("openai"));
        assert_eq!(cfg.api_key.as_deref(), Some("sk-stale"));
        assert_eq!(cfg.model.as_deref(), Some("gpt-5"));
        assert!(cfg.base_url.is_none());
        // confirm_before_commit is orthogonal and survives.
        assert_eq!(cfg.confirm_before_commit, Some(true));
    }

    #[test]
    fn finalize_provider_backend_clears_cli_fields() {
        // No CLI command → provider path; any stale CLI fields are cleared.
        let cfg = finalize(draft_with_cli(None));
        assert_eq!(cfg.backend.as_deref(), Some("openai"));
        assert_eq!(cfg.api_key.as_deref(), Some("sk-stale"));
        assert!(cfg.command.is_none());
        assert!(cfg.args.is_none());
        assert!(cfg.timeout_secs.is_none());
        assert!(cfg.backend_kind.is_none());
    }

    #[test]
    fn cli_label_shows_command_or_not_configured() {
        assert_eq!(
            cli_label(&draft_with_cli(Some("claude"))),
            "claude -p {prompt}"
        );
        assert_eq!(cli_label(&draft_with_cli(None)), "(not configured)");
    }

    #[test]
    fn cli_label_ignores_blank_command() {
        let mut d = draft_with_cli(Some("   "));
        d.cli_command = Some("   ".into());
        assert_eq!(
            cli_label(&d),
            "(not configured)",
            "whitespace-only command is treated as unset"
        );
    }
}
