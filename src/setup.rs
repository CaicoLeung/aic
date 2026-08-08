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

use crate::config::{
    Config, Source, config_path, resolve_api_key, resolve_base_url, resolve_field,
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
        eprintln!(
            "  • set environment variables: LLM_BACKEND, LLM_API_KEY, LLM_MODEL, LLM_BASE_URL"
        );
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
const ICON_DONE: &str = "↩️";

/// Per-step outcome for the setup state machine.
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
    provider: Option<Provider>,
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    /// Whether to require confirmation before each commit. `None` means
    /// "not chosen yet" (finalize keeps it unset → config absent → default
    /// off); the wizard default shown to the user is `false`.
    confirm_before_commit: Option<bool>,
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
    Provider,
    Confirm,
    Save,
    Cancel,
}

/// Run the setup wizard. Returns `None` when the user cancels (Esc on the
/// top-level menu, or Ctrl-C anywhere). Silent on the cancel path — the
/// caller prints the notice.
///
/// The wizard is menu-driven, not a forced linear path: the top level offers
/// two independent entries — the AI provider (provider + key + base URL +
/// model) and the pre-commit confirmation toggle — plus `Save & exit`. Each
/// entry configures only its own fields and returns to the menu, so e.g. the
/// confirmation toggle is reachable without ever touching the provider path.
fn wizard() -> Result<Option<Config>> {
    let existing = Config::load().unwrap_or(None);
    let existing_provider = existing
        .as_ref()
        .and_then(|c| c.backend.as_deref())
        .map(Provider::from_name);

    // Seed the draft from the existing config so a partial visit never wipes
    // fields the user didn't touch: `Save & exit` writes the merged draft.
    // Switching provider later still clears key/url/model (step_provider).
    let mut draft = seed_draft(&existing);
    // Which menu row to highlight when the menu re-renders: returning from a
    // sub-flow highlights the entry the user just finished (0 = provider,
    // 1 = confirmation).
    let mut highlight = 0;
    loop {
        match step_menu(&draft, highlight)? {
            MenuChoice::Provider => {
                if run_provider_flow(&existing, existing_provider, &mut draft)? {
                    return Ok(None); // Ctrl-C inside the provider path
                }
                highlight = 0;
            }
            MenuChoice::Confirm => {
                if run_confirm_flow(&existing, &mut draft)? {
                    return Ok(None); // Ctrl-C on the confirmation toggle
                }
                highlight = 1;
            }
            MenuChoice::Save => return Ok(Some(finalize(draft))),
            MenuChoice::Cancel => return Ok(None),
        }
    }
}

fn key_applies(p: Provider) -> bool {
    p.env_key().is_some() || p == Provider::OpenAiCompatible
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
        draft.api_key = c.api_key.clone();
        draft.base_url = c.base_url.clone();
        draft.model = c.model.clone();
        draft.confirm_before_commit = c.confirm_before_commit;
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

/// A configurable field inside the AI-provider sub-menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProviderEntry {
    ApiKey,
    BaseUrl,
    Model,
    Done,
}

/// `API key` sub-menu row: the masked effective key, annotated with its
/// source so an environment-provided key doesn't read as unset.
fn api_key_label(key: &str, source: Source) -> String {
    if key.is_empty() {
        return "(not set)".to_string();
    }
    match source {
        Source::Env => format!("{} (env)", mask_key(key)),
        _ => mask_key(key),
    }
}

/// `Base URL` sub-menu row: the effective URL (env > config > provider
/// default), annotated with its source.
fn base_url_label(url: Option<&str>, source: Source) -> String {
    match (url, source) {
        (Some(u), Source::Env) => format!("{u} (env)"),
        (Some(u), Source::Default) => format!("{u} (default)"),
        (Some(u), _) => u.to_string(),
        (None, _) => "(not set)".to_string(),
    }
}

/// `Model` sub-menu row: the effective model (env > config > provider
/// default), annotated with its source when not from config.
fn model_label(model: &str, source: Source) -> String {
    if model.is_empty() {
        return "(not set)".to_string();
    }
    match source {
        Source::Env => format!("{model} (env)"),
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
                // The in-session draft wins for display: an env var must not
                // mask the key the user is actively editing. Env is the
                // fallback only when the draft has nothing.
                let (key, source) = match draft.api_key.as_deref().filter(|k| !k.is_empty()) {
                    Some(k) => (k.to_string(), Source::Config),
                    None => resolve_api_key(None, &p),
                };
                entries.push(ProviderEntry::ApiKey);
                labels.push(format!(
                    "{ICON_API_KEY} API key — {}",
                    api_key_label(&key, source)
                ));
            }
            Step::BaseUrl => {
                let (url, source) = match draft.base_url.as_deref().filter(|u| !u.is_empty()) {
                    Some(u) => (Some(u.to_string()), Source::Config),
                    None => resolve_base_url(None, &p),
                };
                entries.push(ProviderEntry::BaseUrl);
                labels.push(format!(
                    "{ICON_BASE_URL} Base URL — {}",
                    base_url_label(url.as_deref(), source)
                ));
            }
            Step::Model => {
                let (model, source) = match draft.model.as_deref().filter(|m| !m.is_empty()) {
                    Some(m) => (m.to_string(), Source::Config),
                    None => resolve_field("LLM_MODEL", None, p.default_model()),
                };
                entries.push(ProviderEntry::Model);
                labels.push(format!(
                    "{ICON_MODEL} Model — {}",
                    model_label(&model, source)
                ));
            }
        }
    }
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
        format!("{ICON_PROVIDER} AI provider — {}", provider_label(draft)),
        format!(
            "{ICON_CONFIRM} Confirm before commit — {}",
            confirm_label(draft)
        ),
        format!("{ICON_SAVE} Save & exit"),
    ];
    match opt_nav("What would you like to configure?", &items, default_idx)? {
        OptNav::Value(0) => Ok(MenuChoice::Provider),
        OptNav::Value(1) => Ok(MenuChoice::Confirm),
        OptNav::Value(2) => Ok(MenuChoice::Save),
        OptNav::Value(_) => unreachable!("menu has exactly three entries"),
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

fn finalize(draft: Draft) -> Config {
    let provider = draft.provider.unwrap_or(Provider::OpenAI);
    Config {
        backend: Some(provider.name().to_string()),
        api_key: draft.api_key,
        model: draft.model,
        base_url: draft.base_url,
        confirm_before_commit: draft.confirm_before_commit,
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
    // Effective key for editing: the in-session draft wins (it is what setup
    // writes), env is the fallback so a key supplied via LLM_API_KEY or the
    // provider's env var is still recognized when the config has none.
    let (key, _) = match draft.api_key.as_deref().filter(|k| !k.is_empty()) {
        Some(k) => (k.to_string(), Source::Config),
        None => resolve_api_key(None, &provider),
    };
    match provider.env_key() {
        // Cloud provider — key required.
        Some(_) => {
            if !key.is_empty() {
                // A key already exists (config or env): keep or replace it — a
                // choice, so offer an option list instead of a typed prompt.
                // The replace path is a sub-mode: Esc there returns to the
                // keep/replace choice.
                let masked = mask_key(&key);
                let items = vec![
                    "Keep current key".to_string(),
                    "Enter a new key…".to_string(),
                ];
                loop {
                    show_screen()?;
                    match opt_nav(&format!("API key (current: {masked})"), &items, 0)? {
                        OptNav::Value(0) => return Ok(Nav::Next),
                        OptNav::Value(1) => {
                            show_screen()?;
                            match prompt_text(
                                "API key (paste — input stays hidden)",
                                true,
                                None,
                                false,
                                "API key cannot be empty",
                            )? {
                                TextAct::Value(v) => {
                                    draft.api_key = Some(v);
                                    return Ok(Nav::Next);
                                }
                                TextAct::Back => continue,
                                TextAct::Cancel => return Ok(Nav::Cancel),
                            }
                        }
                        OptNav::Value(_) => unreachable!("two api key options"),
                        OptNav::Back => return Ok(Nav::Back),
                        OptNav::Cancel => return Ok(Nav::Cancel),
                    }
                }
            }
            // No key at all — it must be entered; there is no choice to offer.
            show_screen()?;
            match prompt_text(
                "API key (paste — input stays hidden)",
                true,
                None,
                false,
                "API key cannot be empty",
            )? {
                TextAct::Value(v) => {
                    draft.api_key = Some(v);
                    Ok(Nav::Next)
                }
                TextAct::Back => Ok(Nav::Back),
                TextAct::Cancel => Ok(Nav::Cancel),
            }
        }
        // OpenAI-compatible — key optional (keyless servers allowed).
        None if provider == Provider::OpenAiCompatible => {
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
            loop {
                show_screen()?;
                match opt_nav("API key", &items, 0)? {
                    OptNav::Value(i) if i == no_key_idx => {
                        draft.api_key = None;
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == enter_idx => {
                        show_screen()?;
                        match prompt_text("API key (blank = keyless server)", true, None, true, "")?
                        {
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
        None => {
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
                false,
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
            let (effective, source) = match draft.base_url.as_deref().filter(|u| !u.is_empty()) {
                Some(u) => (Some(u.to_string()), Source::Config),
                None => resolve_base_url(None, &provider),
            };
            let has_url = matches!(source, Source::Env | Source::Config);
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
            loop {
                show_screen()?;
                match opt_nav("Base URL", &items, 0)? {
                    OptNav::Value(i) if i == use_default_idx => {
                        draft.base_url = None;
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == custom_idx => {
                        show_screen()?;
                        match prompt_text(
                            &format!("Custom base URL (e.g. {default})"),
                            false,
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
            false,
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
                match prompt_text("Custom model", false, None, false, "model cannot be empty")? {
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
        // API key: masked, annotated when env-sourced, (not set) when empty.
        assert_eq!(api_key_label("sk-123", Source::Config), "••••••");
        assert_eq!(api_key_label("sk-123", Source::Env), "•••••• (env)");
        assert_eq!(api_key_label("", Source::Default), "(not set)");

        // Model: value, annotated when env-sourced, (not set) when empty.
        assert_eq!(model_label("gpt-5", Source::Config), "gpt-5");
        assert_eq!(model_label("gpt-5", Source::Env), "gpt-5 (env)");
        assert_eq!(model_label("", Source::Default), "(not set)");

        // Base URL: value, annotated by source, (not set) when none.
        assert_eq!(
            base_url_label(Some("http://h:1"), Source::Config),
            "http://h:1"
        );
        assert_eq!(
            base_url_label(Some("http://h:1"), Source::Env),
            "http://h:1 (env)"
        );
        assert_eq!(
            base_url_label(Some("http://localhost:11434"), Source::Default),
            "http://localhost:11434 (default)"
        );
        assert_eq!(base_url_label(None, Source::Default), "(not set)");
    }

    #[test]
    fn provider_submenu_entries_follow_applicability() {
        // OpenAI: API key + Model + Done (no base URL).
        let d = draft(Some(Provider::OpenAI), None, None, None);
        let (entries, _) = provider_submenu_items(&d);
        assert_eq!(
            entries,
            vec![
                ProviderEntry::ApiKey,
                ProviderEntry::Model,
                ProviderEntry::Done
            ]
        );

        // Ollama: Base URL + Model + Done (no API key).
        let d = draft(Some(Provider::Ollama), None, None, None);
        let (entries, _) = provider_submenu_items(&d);
        assert_eq!(
            entries,
            vec![
                ProviderEntry::BaseUrl,
                ProviderEntry::Model,
                ProviderEntry::Done
            ]
        );

        // OpenAI-compatible: API key + Base URL + Model + Done.
        let d = draft(Some(Provider::OpenAiCompatible), None, None, None);
        let (entries, _) = provider_submenu_items(&d);
        assert_eq!(
            entries,
            vec![
                ProviderEntry::ApiKey,
                ProviderEntry::BaseUrl,
                ProviderEntry::Model,
                ProviderEntry::Done
            ]
        );
    }

    #[test]
    fn submenu_labels_prefer_in_session_choice_over_env() {
        // The in-session draft must win over any env var: after picking
        // deepseek-v4-pro in the menu, the sub-menu has to show Pro, not a
        // default or env-sourced Flash. Draft-first makes this env-robust.
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
                ProviderEntry::Done
            ]
        );
        assert_eq!(labels[0], "🔑 API key — ••••••");
        assert_eq!(labels[1], "🧠 Model — deepseek-v4-pro");
        assert_eq!(labels[2], "↩️ Done — back to main menu");
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
}
