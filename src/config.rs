use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{self, IsTerminal};
use std::path::PathBuf;

use dialoguer::{Confirm, Input, Password, Select, theme::ColorfulTheme};

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

    println!("aic setup — configure your AI provider\n");

    let theme = ColorfulTheme::default();
    match collect_config(&theme)? {
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

/// Gather config field-by-field via dialoguer prompts. Returns `None` when the
/// user cancels at any step (Esc / Ctrl-C / EOF); the caller prints the cancel
/// notice, so this function is silent on the cancel path.
fn collect_config(theme: &ColorfulTheme) -> Result<Option<Config>> {
    let existing = Config::load().unwrap_or(None);
    let existing_provider = existing
        .as_ref()
        .and_then(|c| c.backend.as_deref())
        .map(Provider::from_name);

    // --- Provider -----------------------------------------------------------
    let providers = Provider::all();
    let items: Vec<String> = providers
        .iter()
        .map(|p| match p.default_model() {
            "" => format!("{}  (no default — you'll pick a model)", p.display()),
            m => format!("{}  ({m})", p.display()),
        })
        .collect();
    let default_idx = existing_provider
        .and_then(|ep| providers.iter().position(|p| *p == ep))
        .unwrap_or(0);

    let idx = match cancel(
        Select::with_theme(theme)
            .with_prompt("Choose your AI provider (↑/↓ to move, Enter to select)")
            .items(&items)
            .default(default_idx)
            .interact(),
    )? {
        Some(i) => i,
        None => return Ok(None),
    };
    let provider = providers[idx];

    // Reuse values from the existing config only when the provider is
    // unchanged — switching provider resets key/base-url/model prefill because
    // the old values are almost certainly invalid for the new provider.
    let same_provider = existing_provider == Some(provider);
    let prev_key = same_provider
        .then(|| existing.as_ref().and_then(|c| c.api_key.clone()))
        .flatten();
    let prev_base_url = same_provider
        .then(|| existing.as_ref().and_then(|c| c.base_url.clone()))
        .flatten();
    let prev_model = same_provider
        .then(|| existing.as_ref().and_then(|c| c.model.clone()))
        .flatten();

    // --- API key ------------------------------------------------------------
    let api_key: Option<String> = match provider.env_key() {
        // Cloud provider — key required.
        Some(_) => match prev_key.as_deref() {
            Some(k) => {
                let prompt = format!(
                    "API key (Enter to keep {masked}, or paste a new one)",
                    masked = mask_key(k)
                );
                let entered = match cancel(
                    Password::with_theme(theme)
                        .with_prompt(&prompt)
                        .allow_empty_password(true)
                        .interact(),
                )? {
                    Some(v) => v,
                    None => return Ok(None),
                };
                let trimmed = entered.trim().to_string();
                Some(if trimmed.is_empty() {
                    k.to_string()
                } else {
                    trimmed
                })
            }
            None => {
                let entered = match cancel(
                    Password::with_theme(theme)
                        .with_prompt("API key (paste your key — input stays hidden)")
                        .allow_empty_password(false)
                        .interact(),
                )? {
                    Some(v) => v,
                    None => return Ok(None),
                };
                let trimmed = entered.trim().to_string();
                if trimmed.is_empty() {
                    anyhow::bail!("API key cannot be empty for {}", provider.name());
                }
                Some(trimmed)
            }
        },
        // OpenAI-compatible — key optional (keyless servers allowed).
        None if provider == Provider::OpenAiCompatible => {
            let prompt = match prev_key.as_deref() {
                Some(k) => format!(
                    "API key (Enter to keep {masked}, or leave blank for a keyless server)",
                    masked = mask_key(k)
                ),
                None => "API key (optional — leave blank for a keyless server)".to_string(),
            };
            let entered = match cancel(
                Password::with_theme(theme)
                    .with_prompt(&prompt)
                    .allow_empty_password(true)
                    .interact(),
            )? {
                Some(v) => v,
                None => return Ok(None),
            };
            let trimmed = entered.trim().to_string();
            match (trimmed.is_empty(), prev_key) {
                (true, Some(k)) => Some(k),
                (true, None) => None,
                (false, _) => Some(trimmed),
            }
        }
        // Ollama — runs locally, no key needed.
        None => {
            println!(
                "ℹ {} runs locally and needs no API key.",
                provider.display()
            );
            None
        }
    };

    // --- Base URL -----------------------------------------------------------
    let base_url: Option<String> = match provider.base_url_requirement() {
        BaseUrlRequirement::Required => {
            let mut input = Input::<String>::with_theme(theme)
                .with_prompt("Base URL (e.g. http://localhost:1234/v1)");
            if let Some(prev) = prev_base_url.as_deref() {
                input = input.default(prev.to_string());
            }
            let entered = match cancel(input.interact())? {
                Some(v) => v,
                None => return Ok(None),
            };
            let trimmed = entered.trim().to_string();
            if trimmed.is_empty() {
                anyhow::bail!("base URL cannot be empty for {}", provider.name());
            }
            Some(trimmed)
        }
        BaseUrlRequirement::Optional(default) => {
            let dflt = prev_base_url.as_deref().unwrap_or(default).to_string();
            let entered = match cancel(
                Input::<String>::with_theme(theme)
                    .with_prompt("Base URL")
                    .default(dflt)
                    .interact(),
            )? {
                Some(v) => v,
                None => return Ok(None),
            };
            let trimmed = entered.trim().to_string();
            if trimmed == default {
                None
            } else {
                Some(trimmed)
            }
        }
        BaseUrlRequirement::None => None,
    };

    // --- Model --------------------------------------------------------------
    let default_model = provider.default_model();
    let model: Option<String> = if default_model.is_empty() {
        // No provider default — model is required.
        let mut input = Input::<String>::with_theme(theme)
            .with_prompt("Model (required)")
            .validate_with(|s: &String| {
                if s.trim().is_empty() {
                    Err("model cannot be empty")
                } else {
                    Ok(())
                }
            });
        if let Some(prev) = prev_model.as_deref() {
            input = input.default(prev.to_string());
        }
        let entered = match cancel(input.interact())? {
            Some(v) => v,
            None => return Ok(None),
        };
        Some(entered.trim().to_string())
    } else {
        let dflt = prev_model.as_deref().unwrap_or(default_model).to_string();
        let entered = match cancel(
            Input::<String>::with_theme(theme)
                .with_prompt("Model")
                .default(dflt)
                .interact(),
        )? {
            Some(v) => v,
            None => return Ok(None),
        };
        let trimmed = entered.trim().to_string();
        if trimmed == default_model {
            None
        } else {
            Some(trimmed)
        }
    };

    // --- Summary + confirm --------------------------------------------------
    println!();
    println!("  provider: {}", provider.display());
    match &base_url {
        Some(u) => println!("  base url: {u}"),
        None => match provider.base_url_requirement() {
            BaseUrlRequirement::Optional(d) => println!("  base url: {d}  (default)"),
            _ => println!("  base url:  (provider default)"),
        },
    }
    let model_display =
        model
            .as_deref()
            .filter(|m| !m.is_empty())
            .unwrap_or(if default_model.is_empty() {
                "(none)"
            } else {
                default_model
            });
    println!("  model:    {model_display}");
    match &api_key {
        Some(k) if !k.is_empty() => println!("  api key:  {}", mask_key(k)),
        _ => match provider.env_key() {
            Some(_) => println!("  api key:  (not set)"),
            None => println!("  api key:  (not required)"),
        },
    }
    println!();

    let confirmed = match cancel(
        Confirm::with_theme(theme)
            .with_prompt("Save this config?")
            .default(true)
            .interact(),
    )? {
        Some(v) => v,
        None => return Ok(None),
    };
    if !confirmed {
        return Ok(None);
    }

    Ok(Some(Config {
        backend: Some(provider.name().to_string()),
        api_key,
        model,
        base_url,
    }))
}

/// Mask a secret for the prompt hint and pre-save summary. Never reveals the
/// full value.
fn mask_key(k: &str) -> String {
    if k.is_empty() {
        return "(empty)".to_string();
    }
    "•".repeat(k.len().min(12))
}

/// Translate a dialoguer `interact()` result into `Option<T>`: `None` means the
/// user cancelled (Esc / Ctrl-C / EOF). Any other error is propagated.
fn cancel<T>(res: std::result::Result<T, dialoguer::Error>) -> Result<Option<T>> {
    match res {
        Ok(v) => Ok(Some(v)),
        Err(dialoguer::Error::IO(e))
            if matches!(
                e.kind(),
                io::ErrorKind::Interrupted | io::ErrorKind::UnexpectedEof
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err::<Option<T>, dialoguer::Error>(e).context("could not read terminal input"),
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
