use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use crate::llm::{DEFAULT_PROVIDER, Provider};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub backend: Option<String>,
    pub api_key: Option<String>,
    pub model: Option<String>,
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
}

impl ResolvedConfig {
    pub fn resolve(config: Option<&Config>) -> Self {
        let cfg = config.cloned().unwrap_or(Config {
            backend: None,
            api_key: None,
            model: None,
        });

        let (backend, backend_source) =
            resolve_field("LLM_BACKEND", cfg.backend.as_deref(), DEFAULT_PROVIDER);

        let provider = Provider::from_name(&backend);

        let (api_key, api_key_source) = resolve_api_key(cfg.api_key.as_deref(), &provider);

        let (model, model_source) =
            resolve_field("LLM_MODEL", cfg.model.as_deref(), provider.default_model());

        ResolvedConfig {
            backend,
            backend_source,
            api_key,
            api_key_source,
            model,
            model_source,
        }
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

// --- Interactive setup ---

const PROVIDERS: &[&str] = &[
    "openai",
    "anthropic",
    "gemini",
    "deepseek",
    "groq",
    "ollama",
];

pub fn run_setup() -> Result<()> {
    println!("aic setup\n");

    println!("Select provider:");
    for (i, name) in PROVIDERS.iter().enumerate() {
        println!("  {}. {}", i + 1, name);
    }
    print!("> ");
    io::stdout().flush()?;

    let choice = read_line()?;
    let index: usize = choice.trim().parse().with_context(|| "invalid number")?;
    if index == 0 || index > PROVIDERS.len() {
        anyhow::bail!("invalid choice: must be 1-{}", PROVIDERS.len());
    }
    let backend = PROVIDERS[index - 1];
    let provider = Provider::from_name(backend);
    println!();

    let api_key = if provider.env_key().is_some() {
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
    } else {
        println!("Ollama does not require an API key.\n");
        None
    };

    let default_model = provider.default_model();
    println!("Model [{default_model}]:");
    print!("> ");
    io::stdout().flush()?;
    let model_input = read_line()?;
    let model = {
        let trimmed = model_input.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_string())
        }
    };

    let config = Config {
        backend: Some(backend.to_string()),
        api_key,
        model,
    };
    config.save()?;

    let path = config_path().context("could not determine config path")?;
    println!("Saved to {}\n", path.display());
    println!("  provider: {backend}");
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

    Ok(())
}

fn read_line() -> Result<String> {
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf)
}
