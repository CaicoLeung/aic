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
    ConfirmCommit,
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
        let nav = match step {
            Step::Provider => step_provider(theme, existing_provider, &mut draft)?,
            Step::ApiKey => step_api_key(&existing, existing_provider, &mut draft)?,
            Step::BaseUrl => step_base_url(&existing, existing_provider, &mut draft)?,
            Step::Model => step_model(theme, &existing, existing_provider, &mut draft)?,
            Step::ConfirmCommit => step_confirm_commit(theme, &existing, &mut draft)?,
            Step::Confirm => step_confirm(theme, &draft)?,
        };
        // Recompute after the step runs: `step_provider` may change the provider
        // (clearing key/url/model, which changes which steps apply). `step` is
        // always a member here — navigation only moves to a step already in the
        // list, and the provider can only change while `step == Provider`.
        let steps = applicable_steps(draft.provider.unwrap_or(Provider::OpenAI));
        let idx = steps
            .iter()
            .position(|s| *s == step)
            .expect("current step is always applicable");
        match nav {
            Nav::Cancel => return Ok(None),
            Nav::Back => {
                if idx == 0 {
                    return Ok(None);
                }
                step = steps[idx - 1];
            }
            Nav::Next => {
                if idx + 1 == steps.len() {
                    return Ok(Some(finalize(draft)));
                }
                step = steps[idx + 1];
            }
        }
    }
}

fn key_applies(p: Provider) -> bool {
    p.env_key().is_some() || p == Provider::OpenAiCompatible
}

fn base_url_applies(p: Provider) -> bool {
    !matches!(p.base_url_requirement(), BaseUrlRequirement::None)
}

/// Ordered wizard steps that actually apply to `p`. Steps that would be a
/// no-op — an API key for local Ollama, a base URL for a cloud provider — are
/// absent, so forward/back never lands on one. `Provider` always starts the
/// list and `Confirm` always ends it. Navigation is just index ±1 off this
/// list (see [`wizard`]), replacing the two symmetric `next`/`prev` match
/// trees that previously had to be kept in lock-step.
fn applicable_steps(p: Provider) -> Vec<Step> {
    let mut steps = vec![Step::Provider];
    if key_applies(p) {
        steps.push(Step::ApiKey);
    }
    if base_url_applies(p) {
        steps.push(Step::BaseUrl);
    }
    steps.push(Step::Model);
    steps.push(Step::ConfirmCommit);
    steps.push(Step::Confirm);
    steps
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

fn step_api_key(existing: &Option<Config>, ep: Option<Provider>, draft: &mut Draft) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before api key");
    let initial = field_initial(
        draft.api_key.as_deref(),
        existing,
        ep,
        draft.provider,
        |c| c.api_key.as_ref(),
    );
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
    let initial = field_initial(draft.model.as_deref(), existing, ep, draft.provider, |c| {
        c.model.as_ref()
    });

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

/// Yes/No toggle for requiring confirmation before each commit (issue #78).
/// Unlike the provider-scoped steps, this one is not provider-dependent: the
/// initial value is the in-session draft choice, else the existing config's
/// value, else `false` (the default — behavior unchanged until the user opts
/// in).
fn step_confirm_commit(
    theme: &ColorfulTheme,
    existing: &Option<Config>,
    draft: &mut Draft,
) -> Result<Nav> {
    let initial = confirm_initial(draft, existing);
    match opt_nav(
        Confirm::with_theme(theme)
            .with_prompt("Require confirmation before each commit?")
            .default(initial)
            .interact_opt(),
    )? {
        OptNav::Value(v) => {
            draft.confirm_before_commit = Some(v);
            Ok(Nav::Next)
        }
        OptNav::Back => Ok(Nav::Back),
        OptNav::Cancel => Ok(Nav::Cancel),
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
    println!(
        "  confirm:  {}",
        if draft.confirm_before_commit.unwrap_or(false) {
            "yes — before each commit"
        } else {
            "no"
        }
    );
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
    // stdout (not stderr) so this stays in lock-step with dialoguer's
    // Select/Confirm rendering — prompts and menus never interleave when the
    // streams are split.
    let term = Term::stdout();
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
            vec![
                Step::Provider,
                Step::ApiKey,
                Step::Model,
                Step::ConfirmCommit,
                Step::Confirm
            ]
        );
        // Ollama has no key but a base URL -> BaseUrl present, ApiKey absent.
        assert_eq!(
            applicable_steps(Provider::Ollama),
            vec![
                Step::Provider,
                Step::BaseUrl,
                Step::Model,
                Step::ConfirmCommit,
                Step::Confirm
            ]
        );
        // OpenAI-compatible needs both.
        assert_eq!(
            applicable_steps(Provider::OpenAiCompatible),
            vec![
                Step::Provider,
                Step::ApiKey,
                Step::BaseUrl,
                Step::Model,
                Step::ConfirmCommit,
                Step::Confirm,
            ]
        );
    }

    #[test]
    fn applicable_steps_always_bracketed_and_unique() {
        // Every provider's list starts at Provider and ends at Confirm with
        // Model present, so back never escapes past the first step and forward
        // always reaches the save gate. No step repeats.
        for p in Provider::all() {
            let steps = applicable_steps(*p);
            assert_eq!(
                steps.first(),
                Some(&Step::Provider),
                "{p:?} missing Provider"
            );
            assert_eq!(steps.last(), Some(&Step::Confirm), "{p:?} missing Confirm");
            assert!(steps.contains(&Step::Model), "{p:?} missing Model");
            assert!(
                steps.contains(&Step::ConfirmCommit),
                "{p:?} missing ConfirmCommit"
            );
            assert_eq!(
                steps.iter().filter(|s| **s == Step::Provider).count(),
                1,
                "{p:?} has a duplicate Provider"
            );
        }
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
