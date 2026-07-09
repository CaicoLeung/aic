use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{self, Write};
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
    println!("aic setup\n");

    let providers = Provider::all();
    println!("Select provider:");
    for (i, provider) in providers.iter().enumerate() {
        let suffix = match provider.default_model() {
            "" => "(model required)".to_string(),
            m => format!("({m})"),
        };
        println!("  {}. {} {}", i + 1, provider.display(), suffix);
    }
    print!("> ");
    io::stdout().flush()?;

    let choice = read_line()?;
    let index: usize = choice.trim().parse().with_context(|| "invalid number")?;
    if index == 0 || index > providers.len() {
        anyhow::bail!("invalid choice: must be 1-{}", providers.len());
    }
    let provider = providers[index - 1];
    let backend = provider.name().to_string();
    println!();

    // API key — required for cloud providers, optional for OpenAI-compatible,
    // unused for Ollama.
    let api_key = match provider.env_key() {
        Some(_) => {
            println!("API key:");
            print!("> ");
            io::stdout().flush()?;
            let key = read_line()?;
            let key = key.trim().to_string();
            if key.is_empty() {
                anyhow::bail!("API key cannot be empty for {backend}");
            }
            println!();
            Some(key)
        }
        None if provider == Provider::OpenAiCompatible => {
            println!("API key (optional — leave blank for keyless servers):");
            print!("> ");
            io::stdout().flush()?;
            let key = read_line()?;
            let key = key.trim().to_string();
            println!();
            if key.is_empty() { None } else { Some(key) }
        }
        None => {
            println!("{} does not require an API key.\n", provider.display());
            None
        }
    };

    // Base URL — required for OpenAI-compatible, optional with a default for
    // Ollama, unused for cloud providers.
    let base_url = match provider.base_url_requirement() {
        BaseUrlRequirement::Required => {
            println!("Base URL (required — e.g. http://localhost:1234/v1):");
            print!("> ");
            io::stdout().flush()?;
            let url = read_line()?;
            let url = url.trim().to_string();
            if url.is_empty() {
                anyhow::bail!("base URL cannot be empty for {backend}");
            }
            println!();
            Some(url)
        }
        BaseUrlRequirement::Optional(default) => {
            println!("Base URL [{default}]:");
            print!("> ");
            io::stdout().flush()?;
            let url = read_line()?;
            let trimmed = url.trim().to_string();
            println!();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        BaseUrlRequirement::None => None,
    };

    // Model — required when the provider has no default (OpenRouter,
    // OpenAI-compatible); otherwise the default is offered.
    let default_model = provider.default_model();
    let model = if default_model.is_empty() {
        println!("Model (required):");
        print!("> ");
        io::stdout().flush()?;
        let m = read_line()?;
        let m = m.trim().to_string();
        if m.is_empty() {
            anyhow::bail!("model cannot be empty for {backend}");
        }
        println!();
        Some(m)
    } else {
        println!("Model [{default_model}]:");
        print!("> ");
        io::stdout().flush()?;
        let m = read_line()?;
        let trimmed = m.trim().to_string();
        println!();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    };

    let config = Config {
        backend: Some(backend.clone()),
        api_key,
        model,
        base_url,
    };
    config.save()?;

    let path = config_path().context("could not determine config path")?;
    println!("Saved to {}\n", path.display());
    println!("  provider: {backend}");
    if let Some(b) = &config.base_url {
        println!("  base url: {b}");
    }
    println!(
        "  model:    {}",
        config.model.as_deref().unwrap_or(default_model)
    );

    Ok(())
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

fn read_line() -> Result<String> {
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf)
}
