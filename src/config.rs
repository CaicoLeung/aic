//! Persistent TOML config (`~/.config/aic/config.toml`) and its resolution
//! into the values an [`crate::llm::LLM`] actually uses.
//!
//! This module owns the **deep** config concept: the on-disk [`Config`] shape,
//! [`ResolvedConfig`] precedence (config file > provider default),
//! and the field-level resolution helpers. The interactive `aic setup` wizard
//! that *writes* this config lives in [`crate::setup`]; the generic
//! interactive-input primitives (menus, text prompts, IO-cancel classifier)
//! live in [`crate::input`]. Both were extracted out of this module (AIC-17)
//! so resolution is no longer buried under ~900 lines of TUI machinery.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

use crate::llm::{BaseUrlRequirement, DEFAULT_PROVIDER, Provider};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// External coding-agent CLI to drive instead of an API key (ADR 0010).
    /// When set (non-empty), aic runs in **CLI backend** mode: it shells out
    /// to `command` in headless/print mode and reuses the CLI's own auth, so
    /// no `api_key` is needed (and setting both is rejected). Mutually
    /// exclusive with the API fields below.
    pub command: Option<String>,
    /// Argv template for [`Config::command`]. Each element may contain the
    /// literal `{prompt}` placeholder, which is replaced with the full
    /// (system + user) prompt at run time. Defaults to `["{prompt}"]`.
    pub args: Option<Vec<String>>,
    /// Per-call timeout for the CLI backend, in seconds. Defaults to 60.
    pub timeout_secs: Option<u64>,
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

    /// The CLI-agent `command` when the CLI backend is active: the `command`
    /// field, trimmed and non-empty. `None` means the API provider path is
    /// active (ADR 0010 — selection is purely "`command` is set"). Centralizes
    /// the trim-and-non-empty test so every call site agrees on what "set"
    /// means (used by `aic list`, `LlmConfig::load`, `resolve_cli`).
    pub fn active_cli_command(&self) -> Option<&str> {
        self.command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
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
    Config,
    Default,
}

impl std::fmt::Display for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
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
            command: None,
            args: None,
            timeout_secs: None,
        });

        let (backend, backend_source) = resolve_field(cfg.backend.as_deref(), DEFAULT_PROVIDER);

        let provider = Provider::from_name(&backend);

        let (api_key, api_key_source) = resolve_api_key(cfg.api_key.as_deref());

        let (model, model_source) = resolve_field(cfg.model.as_deref(), provider.default_model());

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
                "provider '{}' requires a base URL — set `base_url` in config (run `aic setup`)",
                provider.name()
            );
        }
        if self.model.trim().is_empty() {
            anyhow::bail!(
                "provider '{}' has no default model — set `model` in config (run `aic setup`)",
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

/// Resolve one scalar field by precedence: config file > default. aic reads
/// only the config file — environment variables are intentionally NOT
/// consulted, so `aic setup` is the single source of truth. `pub(crate)` so
/// the setup wizard can show the effective value/source of a field the user
/// has not edited yet (config- or default-sourced).
pub(crate) fn resolve_field(config_value: Option<&str>, default: &str) -> (String, Source) {
    if let Some(v) = config_value {
        return (v.to_string(), Source::Config);
    }
    (default.to_string(), Source::Default)
}

/// Resolve the API key by precedence: config file > none. Environment
/// variables are not consulted (the config file is the single source of
/// truth). `pub(crate)` for the same setup-wizard reason as [`resolve_field`].
pub(crate) fn resolve_api_key(config_value: Option<&str>) -> (String, Source) {
    if let Some(v) = config_value {
        return (v.to_string(), Source::Config);
    }
    (String::new(), Source::Default)
}

/// Resolve the base URL by precedence: config file > provider default.
/// Environment variables are not consulted. `pub(crate)` for the same
/// setup-wizard reason as [`resolve_field`].
pub(crate) fn resolve_base_url(
    config_value: Option<&str>,
    provider: &Provider,
) -> (Option<String>, Source) {
    if let Some(v) = config_value {
        return (Some(v.to_string()), Source::Config);
    }
    match provider.base_url_requirement() {
        BaseUrlRequirement::Optional(default) => (Some((*default).to_string()), Source::Default),
        BaseUrlRequirement::None | BaseUrlRequirement::Required => (None, Source::Default),
    }
}

pub fn run_list() -> Result<()> {
    let config = Config::load()?;

    // CLI backend (ADR 0010): when `command` is set, it wins over the API
    // provider fields, so show it instead of the rig-resolved defaults.
    let cli_command = config.as_ref().and_then(Config::active_cli_command);
    if let Some(command) = cli_command {
        let c = config.as_ref().expect("command present implies config");
        println!("Backend:  CLI agent");
        println!("Command:  {command} (source: {})", Source::Config);
        let (args, args_src) = match &c.args {
            Some(a) => (a.join(" "), Source::Config),
            None => (
                crate::cli_agent::PROMPT_PLACEHOLDER.to_string(),
                Source::Default,
            ),
        };
        println!("Args:     {args} (source: {args_src})");
        let (timeout, to_src) = match c.timeout_secs {
            Some(t) => (t, Source::Config),
            None => (crate::cli_agent::DEFAULT_TIMEOUT_SECS, Source::Default),
        };
        println!("Timeout:  {timeout}s (source: {to_src})");
        return Ok(());
    }

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
            ..Default::default()
        }
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
            ..Default::default()
        };
        assert!(on.confirm_before_commit());
    }

    #[test]
    fn resolve_field_config_over_default() {
        // Default when nothing is set.
        let (v, s) = resolve_field(None, "def");
        assert_eq!(v, "def");
        assert_eq!(s, Source::Default);

        // Config value beats default.
        let (v, s) = resolve_field(Some("from-cfg"), "def");
        assert_eq!(v, "from-cfg");
        assert_eq!(s, Source::Config);
    }

    #[test]
    fn resolve_base_url_none_when_provider_has_no_default() {
        // A provider with BaseUrlRequirement::None yields no URL from defaults.
        let (url, s) = resolve_base_url(None, &Provider::OpenAI);
        assert_eq!(url, None);
        assert_eq!(s, Source::Default);
    }

    #[test]
    fn resolve_base_url_optional_provider_defaults() {
        // Ollama exposes an optional default URL when nothing else is set.
        let (url, s) = resolve_base_url(None, &Provider::Ollama);
        assert!(url.is_some());
        assert_eq!(s, Source::Default);
    }

    // Silence the otherwise-unused `cfg` helper if every test using it is
    // compiled out; kept because follow-up config tests will reuse it.
    #[test]
    fn cfg_helper_builds_a_config() {
        let c = cfg("openai", Some("k"), Some("m"), None);
        assert_eq!(c.backend.as_deref(), Some("openai"));
        assert_eq!(c.api_key.as_deref(), Some("k"));
        assert_eq!(c.model.as_deref(), Some("m"));
    }
}
