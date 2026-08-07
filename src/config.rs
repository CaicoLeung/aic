use anyhow::{Context, Result};
use console::Term;
use inquire::list_option::ListOption;
use inquire::validator::Validation;
use inquire::{InquireError, Password, Select, Text};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::PathBuf;

use crate::llm::{BaseUrlRequirement, DEFAULT_PROVIDER, Provider};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub backend: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// When `true`, `aic` shows the drafted commit message and the files it
    /// would land, then offers a Commit / Re-generate / Edit / Abort menu
    /// before each commit. Absent (or `false`) keeps the original
    /// generate-and-commit behavior.
    pub confirm_before_commit: Option<bool>,
}

pub fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("aic").join("config.toml"))
}

impl Config {
    pub fn load() -> Result<Option<Self>> {
        let path = match config_path() {
            Some(p) => p,
            None => return Ok(None),
        };

        if !path.exists() {
            return Ok(None);
        }

        let content = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let config: Config = toml::from_str(&content)
            .with_context(|| format!("failed to parse {}", path.display()))?;
        Ok(Some(config))
    }

    /// Whether each drafted commit must be confirmed before it lands.
    /// Absent or `false` → commit immediately, as before this option existed.
    pub fn confirm_before_commit(&self) -> bool {
        self.confirm_before_commit.unwrap_or(false)
    }

    pub fn save(&self) -> Result<()> {
        let path = config_path().context("could not determine config directory")?;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let content = toml::to_string_pretty(self).context("failed to serialize config")?;
        fs::write(&path, content).with_context(|| format!("failed to write {}", path.display()))?;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Source {
    Env,
    Config,
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Source::Env => write!(f, "env"),
            Source::Config => write!(f, "config"),
            Source::Default => write!(f, "default"),
        }
    }
}

pub struct ResolvedConfig {
    pub backend: String,
    pub backend_source: Source,
    pub api_key: String,
    pub api_key_source: Source,
    pub model: String,
    pub model_source: Source,
    pub base_url: Option<String>,
    pub base_url_source: Source,
}

impl ResolvedConfig {
    pub fn resolve(config: Option<&Config>) -> Self {
        let cfg = config.cloned().unwrap_or(Config {
            backend: None,
            api_key: None,
            model: None,
            base_url: None,
            confirm_before_commit: None,
        });

        let (backend, backend_source) =
            resolve_field("LLM_BACKEND", cfg.backend.as_deref(), DEFAULT_PROVIDER);

        let provider = Provider::from_name(&backend);

        let (api_key, api_key_source) = resolve_api_key(cfg.api_key.as_deref(), &provider);

        let (model, model_source) =
            resolve_field("LLM_MODEL", cfg.model.as_deref(), provider.default_model());

        let (base_url, base_url_source) = resolve_base_url(cfg.base_url.as_deref(), &provider);

        ResolvedConfig {
            backend,
            backend_source,
            api_key,
            api_key_source,
            model,
            model_source,
            base_url,
            base_url_source,
        }
    }

    /// Validate provider-specific requirements (a model or base URL the provider
    /// cannot default). Called when constructing an `LLM`, not when merely
    /// displaying resolved config (`aic list`).
    pub fn validate(&self) -> Result<()> {
        let provider = Provider::from_name(&self.backend);
        if provider.base_url_requirement() == BaseUrlRequirement::Required
            && self.base_url.is_none()
        {
            anyhow::bail!(
                "provider '{}' requires a base URL — set LLM_BASE_URL or `base_url` in config",
                provider.name()
            );
        }
        if self.model.trim().is_empty() {
            anyhow::bail!(
                "provider '{}' has no default model — set LLM_MODEL or `model` in config",
                provider.name()
            );
        }
        Ok(())
    }

    pub fn mask_api_key(&self) -> String {
        if self.api_key.is_empty() {
            return "(not set)".to_string();
        }
        let key = &self.api_key;
        if key.len() <= 8 {
            return "***".to_string();
        }
        format!("{}...{}", &key[..3], &key[key.len() - 3..])
    }
}

fn resolve_field(env_var: &str, config_value: Option<&str>, default: &str) -> (String, Source) {
    if let Ok(v) = env::var(env_var) {
        return (v, Source::Env);
    }
    if let Some(v) = config_value {
        return (v.to_string(), Source::Config);
    }
    (default.to_string(), Source::Default)
}

fn resolve_api_key(config_value: Option<&str>, provider: &Provider) -> (String, Source) {
    if let Ok(v) = env::var("LLM_API_KEY") {
        return (v, Source::Env);
    }
    if let Some(key) = provider.env_key()
        && let Ok(v) = env::var(key)
    {
        return (v, Source::Env);
    }
    if let Some(v) = config_value {
        return (v.to_string(), Source::Config);
    }
    (String::new(), Source::Default)
}

fn resolve_base_url(config_value: Option<&str>, provider: &Provider) -> (Option<String>, Source) {
    if let Ok(v) = env::var("LLM_BASE_URL") {
        return (Some(v), Source::Env);
    }
    if let Some(v) = config_value {
        return (Some(v.to_string()), Source::Config);
    }
    match provider.base_url_requirement() {
        BaseUrlRequirement::Optional(default) => (Some((*default).to_string()), Source::Default),
        BaseUrlRequirement::None | BaseUrlRequirement::Required => (None, Source::Default),
    }
}

// --- Interactive setup ---

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
/// is just index ±1 off this list (see [`run_provider_flow`]).
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

/// `AI provider` menu row: the current provider and the model that would be
/// used (the chosen one, else the provider default). `(not set)` when no
/// provider is configured yet.
fn provider_label(draft: &Draft) -> String {
    let Some(p) = draft.provider else {
        return "(not set)".to_string();
    };
    let model = draft
        .model
        .as_deref()
        .filter(|m| !m.is_empty())
        .map(String::from)
        .or_else(|| {
            let d = p.default_model();
            if d.is_empty() {
                None
            } else {
                Some(d.to_string())
            }
        });
    match model {
        Some(m) => format!("{} · {m}", p.display()),
        None => p.display().to_string(),
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
                labels.push(format!("API key — {}", api_key_label(&key, source)));
            }
            Step::BaseUrl => {
                let (url, source) = match draft.base_url.as_deref().filter(|u| !u.is_empty()) {
                    Some(u) => (Some(u.to_string()), Source::Config),
                    None => resolve_base_url(None, &p),
                };
                entries.push(ProviderEntry::BaseUrl);
                labels.push(format!(
                    "Base URL — {}",
                    base_url_label(url.as_deref(), source)
                ));
            }
            Step::Model => {
                let (model, source) = match draft.model.as_deref().filter(|m| !m.is_empty()) {
                    Some(m) => (m.to_string(), Source::Config),
                    None => resolve_field("LLM_MODEL", None, p.default_model()),
                };
                entries.push(ProviderEntry::Model);
                labels.push(format!("Model — {}", model_label(&model, source)));
            }
        }
    }
    entries.push(ProviderEntry::Done);
    labels.push("Done — back to main menu".to_string());
    (entries, labels)
}

/// Render and run the top-level menu. Entering an entry routes to its
/// sub-flow; `Save & exit` finalizes; `Esc`/Ctrl-C cancels the whole setup.
/// `default_idx` is the row highlighted when the menu opens (persisted from
/// the entry the user just finished).
fn step_menu(draft: &Draft, default_idx: usize) -> Result<MenuChoice> {
    show_screen()?;
    let items = vec![
        format!("AI provider — {}", provider_label(draft)),
        format!("Confirm before commit — {}", confirm_label(draft)),
        "Save & exit".to_string(),
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
        .map(|p| match p.default_model() {
            "" => format!("{}  (no default — you'll pick a model)", p.display()),
            m => format!("{}  ({m})", p.display()),
        })
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

/// Outcome of a single-choice menu ([`opt_nav`]): the chosen row index, or
/// Esc (back) / Ctrl-C (cancel). inquire models Esc and Ctrl-C as error
/// variants ([`InquireError::OperationCanceled`] /
/// [`InquireError::OperationInterrupted`]); this normalizes them into the
/// wizard's nav vocabulary so every menu dispatches identically.
enum OptNav {
    Value(usize),
    Back,
    Cancel,
}

/// Whether an inquire error is a hard cancel — Ctrl-C
/// ([`InquireError::OperationInterrupted`]) or a closed/EOF stdin (surfaced as
/// an [`InquireError::IO`] error of kind `Interrupted`/`UnexpectedEof`). Esc
/// ([`InquireError::OperationCanceled`]) is deliberately excluded: the wizard
/// treats Esc as *back* while the production menus treat every cancel alike,
/// so each caller decides where Esc falls. Shared with `is_graceful_cancel`
/// in `main.rs` so the IO-kind sub-clause isn't duplicated.
pub(crate) fn is_io_cancel(e: &InquireError) -> bool {
    matches!(e, InquireError::OperationInterrupted)
        || matches!(
            e,
            InquireError::IO(err) if matches!(
                err.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
            )
        )
}

/// Render an `inquire::Select` menu over `options` and map its three outcomes
/// — choose (index), Esc (back), Ctrl-C/closed-stdin (cancel) — onto
/// [`OptNav`]. `default` is the row highlighted when the menu opens (inquire's
/// `starting_cursor`). Uses `raw_prompt` so the chosen **index** is recoverable
/// (inquire's plain `prompt` returns only the value), which keeps the wizard's
/// index-based dispatch untouched.
///
/// `options` is borrowed (`&[String]`) and cloned once here
/// (`options.to_vec()`) because `inquire::Select` owns its list — borrowing
/// lets call sites keep one reusable `Vec` across loop iterations without a
/// per-iteration clone. The menu is built `.without_filtering()` so typing a
/// letter is a clean no-op, matching the dialoguer behavior this replaced
/// (inquire's default is filter-on-type, which would add an unfamiliar input
/// line and re-bind letter keys); see ADR-0007.
fn opt_nav(prompt: &str, options: &[String], default: usize) -> Result<OptNav> {
    match Select::new(prompt, options.to_vec())
        .with_starting_cursor(default)
        .without_filtering()
        .raw_prompt()
    {
        Ok(ListOption { index, .. }) => Ok(OptNav::Value(index)),
        Err(InquireError::OperationCanceled) => Ok(OptNav::Back),
        // Ctrl-C or a closed/EOF stdin — both cancel the wizard. Esc is
        // handled above as Back.
        Err(e) if is_io_cancel(&e) => Ok(OptNav::Cancel),
        Err(e) => Err(e).context("could not read terminal input"),
    }
}

/// Outcome of a raw-mode text prompt.
enum TextAct {
    Value(String),
    Back,
    Cancel,
}

/// Read a line of text via the `inquire` crate, which intercepts Esc (back)
/// and Ctrl-C (cancel) natively as error variants. `masked` hides each typed
/// char (for secrets). `initial` is offered as the kept value when the user
/// submits an empty line. `allow_empty` admits an empty submit; otherwise
/// `empty_hint` is shown (via inquire's validator) and the prompt retries until
/// non-empty.
fn prompt_text(
    prompt: &str,
    masked: bool,
    initial: Option<&str>,
    allow_empty: bool,
    empty_hint: &str,
) -> Result<TextAct> {
    // Show the current value as a help line; an empty submit keeps it.
    let help = match (initial, masked) {
        (Some(_), true) => Some("current: •••• (leave blank to keep)".to_string()),
        (Some(d), false) => Some(format!("current: {d} (leave blank to keep)")),
        _ => None,
    };

    let prompt_result = if masked {
        let mut p = Password::new(prompt);
        if let Some(h) = help.as_deref() {
            p = p.with_help_message(h);
        }
        if !allow_empty {
            let hint = empty_hint.to_string();
            p = p.with_validator(move |v: &str| {
                if v.trim().is_empty() {
                    Ok(Validation::Invalid(hint.clone().into()))
                } else {
                    Ok(Validation::Valid)
                }
            });
        }
        p.prompt()
    } else {
        let mut t = Text::new(prompt);
        if let Some(h) = help.as_deref() {
            t = t.with_help_message(h);
        }
        if !allow_empty {
            let hint = empty_hint.to_string();
            t = t.with_validator(move |v: &str| {
                if v.trim().is_empty() {
                    Ok(Validation::Invalid(hint.clone().into()))
                } else {
                    Ok(Validation::Valid)
                }
            });
        }
        t.prompt()
    };

    match prompt_result {
        Ok(v) => {
            let trimmed = v.trim().to_string();
            if trimmed.is_empty() {
                // Empty submit: keep the initial when present, else honor
                // allow_empty (the validator already enforced `required`).
                if let Some(d) = initial {
                    return Ok(TextAct::Value(d.to_string()));
                }
                if allow_empty {
                    return Ok(TextAct::Value(String::new()));
                }
            }
            Ok(TextAct::Value(trimmed))
        }
        // Esc — back out of this step.
        Err(InquireError::OperationCanceled) => Ok(TextAct::Back),
        // Ctrl-C — cancel the whole setup, same as everywhere in the wizard.
        Err(InquireError::OperationInterrupted) => Ok(TextAct::Cancel),
        Err(e) => Err(e).context("could not read terminal input"),
    }
}

pub fn run_list() -> Result<()> {
    let config = Config::load()?;
    let resolved = ResolvedConfig::resolve(config.as_ref());

    println!(
        "Provider: {} (source: {})",
        resolved.backend, resolved.backend_source
    );
    println!(
        "Model:    {} (source: {})",
        resolved.model, resolved.model_source
    );
    println!(
        "API key:  {} (source: {})",
        resolved.mask_api_key(),
        resolved.api_key_source
    );
    println!(
        "Base URL: {} (source: {})",
        resolved.base_url.as_deref().unwrap_or("(none)"),
        resolved.base_url_source
    );

    Ok(())
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
        assert_eq!(labels[0], "API key — ••••••");
        assert_eq!(labels[1], "Model — deepseek-v4-pro");
        assert_eq!(labels[2], "Done — back to main menu");
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
    fn confirm_before_commit_defaults_off_and_respects_value() {
        assert!(!cfg("openai", None, None, None).confirm_before_commit());
        let on = Config {
            backend: None,
            api_key: None,
            model: None,
            base_url: None,
            confirm_before_commit: Some(true),
        };
        assert!(on.confirm_before_commit());
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
