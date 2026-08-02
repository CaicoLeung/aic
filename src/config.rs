use anyhow::{Context, Result};
use console::{Key, Term};
use dialoguer::{Confirm, Select, theme::ColorfulTheme};
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
            "  • write the config file at {path} (TOML keys: backend, api_key, model, base_url), or"
        );
        eprintln!(
            "  • set environment variables: LLM_BACKEND, LLM_API_KEY, LLM_MODEL, LLM_BASE_URL"
        );
        anyhow::bail!("cannot run interactive setup without a TTY");
    }

    println!("aic setup — configure your AI provider");
    println!("  ↑/↓ move · Enter confirm · Esc back · Ctrl-C cancel\n");

    let theme = ColorfulTheme::default();
    match wizard(&theme)? {
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

/// Per-step outcome for the setup state machine.
enum Nav {
    Next,
    Back,
    Cancel,
}

/// In-progress wizard selections. Values persist across back-navigation so
/// the user can edit one field without losing the others.
#[derive(Default)]
struct Draft {
    provider: Option<Provider>,
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
}

#[derive(Clone, Copy)]
enum Step {
    Provider,
    ApiKey,
    BaseUrl,
    Model,
    Confirm,
}

/// Run the setup wizard. Returns `None` when the user cancels (Esc on the
/// first step, or Ctrl-C anywhere). Silent on the cancel path — the caller
/// prints the notice.
fn wizard(theme: &ColorfulTheme) -> Result<Option<Config>> {
    let existing = Config::load().unwrap_or(None);
    let existing_provider = existing
        .as_ref()
        .and_then(|c| c.backend.as_deref())
        .map(Provider::from_name);

    let mut draft = Draft::default();
    let mut step = Step::Provider;
    loop {
        let provider = draft.provider.unwrap_or(Provider::OpenAI);
        let nav = match step {
            Step::Provider => step_provider(theme, existing_provider, &mut draft)?,
            Step::ApiKey => step_api_key(theme, &existing, existing_provider, &mut draft)?,
            Step::BaseUrl => step_base_url(theme, &existing, existing_provider, &mut draft)?,
            Step::Model => step_model(theme, &existing, existing_provider, &mut draft)?,
            Step::Confirm => step_confirm(theme, &draft)?,
        };
        match nav {
            Nav::Cancel => return Ok(None),
            Nav::Back => match prev_step(step, provider) {
                Some(prev) => step = prev,
                None => return Ok(None),
            },
            Nav::Next => match next_step(step, provider) {
                Some(next) => step = next,
                None => return Ok(Some(finalize(draft))),
            },
        }
    }
}

fn key_applies(p: Provider) -> bool {
    p.env_key().is_some() || p == Provider::OpenAiCompatible
}

fn base_url_applies(p: Provider) -> bool {
    !matches!(p.base_url_requirement(), BaseUrlRequirement::None)
}

fn next_step(s: Step, p: Provider) -> Option<Step> {
    match s {
        Step::Provider => Some(if key_applies(p) {
            Step::ApiKey
        } else if base_url_applies(p) {
            Step::BaseUrl
        } else {
            Step::Model
        }),
        Step::ApiKey => Some(if base_url_applies(p) {
            Step::BaseUrl
        } else {
            Step::Model
        }),
        Step::BaseUrl => Some(Step::Model),
        Step::Model => Some(Step::Confirm),
        Step::Confirm => None,
    }
}

fn prev_step(s: Step, p: Provider) -> Option<Step> {
    match s {
        Step::Provider => None,
        Step::ApiKey => Some(Step::Provider),
        Step::BaseUrl => Some(if key_applies(p) {
            Step::ApiKey
        } else {
            Step::Provider
        }),
        Step::Model => Some(if base_url_applies(p) {
            Step::BaseUrl
        } else if key_applies(p) {
            Step::ApiKey
        } else {
            Step::Provider
        }),
        Step::Confirm => Some(Step::Model),
    }
}

fn finalize(draft: Draft) -> Config {
    let provider = draft.provider.unwrap_or(Provider::OpenAI);
    Config {
        backend: Some(provider.name().to_string()),
        api_key: draft.api_key,
        model: draft.model,
        base_url: draft.base_url,
    }
}

/// Effective initial value for a field: in-session draft value first, else the
/// existing config value when the provider is unchanged, else none.
fn key_initial(draft: &Draft, existing: &Option<Config>, ep: Option<Provider>) -> Option<String> {
    if let Some(v) = &draft.api_key {
        return Some(v.clone());
    }
    if ep.is_some() && ep == draft.provider {
        return existing.as_ref().and_then(|c| c.api_key.clone());
    }
    None
}

fn base_url_initial(
    draft: &Draft,
    existing: &Option<Config>,
    ep: Option<Provider>,
) -> Option<String> {
    if let Some(v) = &draft.base_url {
        return Some(v.clone());
    }
    if ep.is_some() && ep == draft.provider {
        return existing.as_ref().and_then(|c| c.base_url.clone());
    }
    None
}

fn model_initial(draft: &Draft, existing: &Option<Config>, ep: Option<Provider>) -> Option<String> {
    if let Some(v) = &draft.model {
        return Some(v.clone());
    }
    if ep.is_some() && ep == draft.provider {
        return existing.as_ref().and_then(|c| c.model.clone());
    }
    None
}

fn step_provider(
    theme: &ColorfulTheme,
    existing_provider: Option<Provider>,
    draft: &mut Draft,
) -> Result<Nav> {
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

    match opt_nav(
        Select::with_theme(theme)
            .with_prompt("Choose your AI provider")
            .items(&items)
            .default(default_idx)
            .interact_opt(),
    )? {
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

fn step_api_key(
    _theme: &ColorfulTheme,
    existing: &Option<Config>,
    ep: Option<Provider>,
    draft: &mut Draft,
) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before api key");
    let initial = key_initial(draft, existing, ep);
    match provider.env_key() {
        // Cloud provider — key required.
        Some(_) => {
            let prompt = match initial.as_deref() {
                Some(_) => format!(
                    "API key (Enter keeps {masked}, or paste a new one)",
                    masked = mask_key(initial.as_deref().unwrap_or(""))
                ),
                None => "API key (paste — input stays hidden)".to_string(),
            };
            match prompt_text(
                &prompt,
                true,
                initial.as_deref(),
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
            let prompt = match initial.as_deref() {
                Some(_) => "API key (Enter keeps, blank = keyless server)",
                None => "API key (optional — blank for a keyless server)",
            };
            match prompt_text(prompt, true, initial.as_deref(), true, "")? {
                TextAct::Value(v) => {
                    draft.api_key = if v.is_empty() { None } else { Some(v) };
                    Ok(Nav::Next)
                }
                TextAct::Back => Ok(Nav::Back),
                TextAct::Cancel => Ok(Nav::Cancel),
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
    _theme: &ColorfulTheme,
    existing: &Option<Config>,
    ep: Option<Provider>,
    draft: &mut Draft,
) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before base url");
    let initial = base_url_initial(draft, existing, ep);
    match provider.base_url_requirement() {
        BaseUrlRequirement::Required => match prompt_text(
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
        },
        BaseUrlRequirement::Optional(default) => {
            let dflt = initial.as_deref().unwrap_or(default);
            match prompt_text(
                &format!("Base URL (Enter for default: {default})"),
                false,
                Some(dflt),
                true,
                "",
            )? {
                TextAct::Value(v) => {
                    draft.base_url = if v == default { None } else { Some(v) };
                    Ok(Nav::Next)
                }
                TextAct::Back => Ok(Nav::Back),
                TextAct::Cancel => Ok(Nav::Cancel),
            }
        }
        // Unreachable (step skipped via base_url_applies); defensive.
        BaseUrlRequirement::None => Ok(Nav::Next),
    }
}

fn step_model(
    theme: &ColorfulTheme,
    existing: &Option<Config>,
    ep: Option<Provider>,
    draft: &mut Draft,
) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before model");
    let default_model = provider.default_model();
    let models = provider.models();
    let initial = model_initial(draft, existing, ep);

    // No curated list (OpenRouter, OpenAI-compatible) -> required free text.
    if models.is_empty() {
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
        match opt_nav(
            Select::with_theme(theme)
                .with_prompt("Model")
                .items(&items)
                .default(highlight)
                .interact_opt(),
        )? {
            OptNav::Value(i) if i == custom_idx => {
                match prompt_text("Custom model", false, None, false, "model cannot be empty")? {
                    TextAct::Value(v) => {
                        draft.model = Some(v);
                        return Ok(Nav::Next);
                    }
                    // Esc from custom returns to the model select (re-loop), not
                    // to the previous step.
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

fn step_confirm(theme: &ColorfulTheme, draft: &Draft) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before confirm");
    let default_model = provider.default_model();

    println!();
    println!("  provider: {}", provider.display());
    match &draft.base_url {
        Some(u) => println!("  base url: {u}"),
        None => match provider.base_url_requirement() {
            BaseUrlRequirement::Optional(d) => println!("  base url: {d}  (default)"),
            _ => println!("  base url:  (provider default)"),
        },
    }
    let model_display =
        draft
            .model
            .as_deref()
            .filter(|m| !m.is_empty())
            .unwrap_or(if default_model.is_empty() {
                "(none)"
            } else {
                default_model
            });
    println!("  model:    {model_display}");
    match &draft.api_key {
        Some(k) if !k.is_empty() => println!("  api key:  {}", mask_key(k)),
        _ => match provider.env_key() {
            Some(_) => println!("  api key:  (not set)"),
            None => println!("  api key:  (not required)"),
        },
    }
    println!();

    match opt_nav(
        Confirm::with_theme(theme)
            .with_prompt("Save this config?  (y = save · Esc/n = go back)")
            .default(true)
            .interact_opt(),
    )? {
        OptNav::Value(true) => Ok(Nav::Next),
        OptNav::Value(false) | OptNav::Back => Ok(Nav::Back),
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

/// Map a dialoguer `interact_opt()` result onto the wizard's nav: a value is
/// `Next`-bound, `None` (Esc) is `Back`, and Ctrl-C / EOF is `Cancel`.
enum OptNav<T> {
    Value(T),
    Back,
    Cancel,
}

fn opt_nav<T>(res: std::result::Result<Option<T>, dialoguer::Error>) -> Result<OptNav<T>> {
    match res {
        Ok(Some(v)) => Ok(OptNav::Value(v)),
        Ok(None) => Ok(OptNav::Back),
        Err(dialoguer::Error::IO(e))
            if matches!(
                e.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
            ) =>
        {
            Ok(OptNav::Cancel)
        }
        Err(e) => Err::<OptNav<T>, dialoguer::Error>(e).context("could not read terminal input"),
    }
}

/// Outcome of a raw-mode text prompt.
enum TextAct {
    Value(String),
    Back,
    Cancel,
}

/// Read a line of text in raw mode so we can intercept Esc (back) and Ctrl-C
/// (cancel) — which dialoguer's `Input`/`Password` can't do. `masked` hides
/// each typed char (for secrets). `initial` is offered as the kept value when
/// the user presses Enter on an empty line.
fn prompt_text(
    prompt: &str,
    masked: bool,
    initial: Option<&str>,
    allow_empty: bool,
    empty_hint: &str,
) -> Result<TextAct> {
    let term = Term::stderr();
    let init_hint = match (initial, masked) {
        (Some(_), true) => "  [current: ••••]".to_string(),
        (Some(d), false) => format!("  [current: {d}]"),
        _ => String::new(),
    };
    term.write_line(&format!("{prompt}{init_hint}"))?;
    term.write_line("  Enter = confirm · Esc = back · Ctrl-C = cancel")?;

    // `read_key_raw` puts the terminal in raw mode for each keypress (console
    // restores cooked mode automatically) and returns `Key::CtrlC` on Ctrl-C
    // instead of raising SIGINT, so we can treat it as a graceful cancel.
    let mut buf = String::new();
    loop {
        let key = term.read_key_raw().context("could not read keypress")?;
        match key {
            Key::Enter => {
                let _ = term.write_str("\r\n");
                let trimmed = buf.trim().to_string();
                if trimmed.is_empty() {
                    if let Some(d) = initial {
                        return Ok(TextAct::Value(d.to_string()));
                    }
                    if allow_empty {
                        return Ok(TextAct::Value(String::new()));
                    }
                    // Required + empty: flash the hint and keep reading.
                    let _ = term.write_str(&format!("  {empty_hint} — try again: "));
                    continue;
                }
                return Ok(TextAct::Value(trimmed));
            }
            Key::Escape => return Ok(TextAct::Back),
            Key::CtrlC => return Ok(TextAct::Cancel),
            Key::Backspace => {
                if buf.pop().is_some() {
                    let _ = term.write_str("\u{8} \u{8}");
                }
            }
            Key::Char(c) if !c.is_control() => {
                buf.push(c);
                if masked {
                    let _ = term.write_str("•");
                } else {
                    let _ = term.write_str(&c.to_string());
                }
            }
            _ => {}
        }
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
