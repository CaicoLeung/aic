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

/// The CLI-agent Backend's three config fields — a unit (command + argv
/// template + timeout) that travels together through [`Config`], the setup
/// `Draft`, and [`CliSpec`](crate::cli_agent::CliSpec). On disk they stay
/// **flat** top-level TOML keys via `#[serde(flatten)]` (ADR 0011: the
/// `backend_kind` discriminator carries the grouping; a nested table would
/// duplicate it).
///
/// Grouping the trio (instead of three loose `Option` fields redeclared in
/// each context) gives them one owner and one `active_command` test, and lets
/// resolution ([`Self::to_spec`]) live on the data it reads — fixing both the
/// data-clump duplication and the feature-envy free function that reached
/// across into [`Config`]'s fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CliConfig {
    /// External coding-agent CLI to drive instead of an API key (ADR 0010).
    /// When `backend_kind = "cli"`, aic shells out to `command` in
    /// headless/print mode and reuses the CLI's own auth, so no `api_key` is
    /// needed.
    pub command: Option<String>,
    /// Argv template for [`Self::command`]. Each element may contain the
    /// literal `{prompt}` placeholder, replaced with the full prompt at run
    /// time. Defaults to `["{prompt}"]`. The flags here also select the
    /// stdout [`Encoding`](crate::cli_agent::Encoding) via
    /// [`Encoding::from_args`](crate::cli_agent::Encoding::from_args).
    pub args: Option<Vec<String>>,
    /// Per-call timeout for the CLI backend, in seconds. Defaults to 240
    /// (see [`crate::cli_agent::DEFAULT_TIMEOUT_SECS`] for why it is far
    /// above the API path's latency budget).
    pub timeout_secs: Option<u64>,
}

impl CliConfig {
    /// The CLI-agent `command` when set: the `command` field, trimmed and
    /// non-empty; `None` when unset. The single "is the CLI backend
    /// configured?" test, shared by [`Config`] and the setup `Draft` so every
    /// reader agrees. NOTE (ADR 0011): this only *reads* the value — which
    /// Backend is active is decided by [`Config::resolve_backend`] reading
    /// `backend_kind`, not by command presence.
    pub fn active_command(&self) -> Option<&str> {
        self.command
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }

    /// Resolve this config into a runnable
    /// [`CliSpec`](crate::cli_agent::CliSpec), applying the default args
    /// template (`["{prompt}"]`) and the default timeout. The stdout
    /// [`Encoding`](crate::cli_agent::Encoding) is **inferred from the argv**
    /// via [`Encoding::from_args`](crate::cli_agent::Encoding::from_args) —
    /// the preset's own flags are the single source of truth, shared with
    /// `aic setup` verify, so run-time and setup can never disagree on which
    /// decoder runs (the regression that left a claude-preset verify decoding
    /// raw NDJSON as plain text).
    ///
    /// Only call this when `backend_kind = "cli"` and `active_command` is
    /// `Some` (guaranteed by [`Config::resolve_backend`]).
    pub fn to_spec(&self) -> crate::cli_agent::CliSpec {
        let command = self
            .active_command()
            .expect("CliConfig::to_spec only called when a command is set")
            .to_string();
        let args = self
            .args
            .clone()
            .unwrap_or_else(|| vec![crate::cli_agent::PROMPT_PLACEHOLDER.to_string()]);
        let timeout_secs = self
            .timeout_secs
            .unwrap_or(crate::cli_agent::DEFAULT_TIMEOUT_SECS);
        let encoding = crate::cli_agent::Encoding::from_args(&args);
        crate::cli_agent::CliSpec {
            command,
            args,
            timeout_secs,
            encoding,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    pub backend: Option<String>,
    /// Which Backend is active: `"api"` (the API-provider Backend, default
    /// when absent) or `"cli"` (the CLI-agent Backend). Authoritative — see
    /// [`BackendKind`] / [`Config::resolve_backend`] (ADR 0011). Typed (not a
    /// raw string) so an unknown value is rejected at config-parse time and an
    /// invalid discriminator can never exist in a parsed [`Config`].
    pub backend_kind: Option<BackendKind>,
    pub api_key: Option<String>,
    pub model: Option<String>,
    pub base_url: Option<String>,
    /// When `true`, `aic` shows the drafted commit message and the files it
    /// would land, then offers a Commit / Re-generate / Edit / Abort menu
    /// before each commit. Absent (or `false`) keeps the original
    /// generate-and-commit behavior.
    pub confirm_before_commit: Option<bool>,
    /// The CLI-agent Backend's fields. Flattened to top-level TOML keys
    /// (`command` / `args` / `timeout_secs`) so the on-disk shape is unchanged
    /// — ADR 0011 keeps the grouping in `backend_kind`, not in a nested
    /// table. See [`CliConfig`].
    #[serde(flatten)]
    pub cli: CliConfig,
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

    /// The CLI-agent `command` value when set (trimmed, non-empty); `None`
    /// when unset. Thin delegator over [`CliConfig::active_command`] for
    /// call-site stability; the single "is the CLI backend configured?" test
    /// (ADR 0011: `backend_kind` selects the active Backend; this only reads
    /// the value).
    pub fn active_cli_command(&self) -> Option<&str> {
        self.cli.active_command()
    }

    /// Resolve the active [`BackendKind`] from the `backend_kind` discriminator
    /// (ADR 0011). The discriminator is authoritative — it alone decides which
    /// Backend a Run uses; the inactive Backend's fields may be present in the
    /// file as **dormant** config (preserved across backend switches) and are
    /// simply ignored at run time. Absent ⇒ [`BackendKind::Api`] (the
    /// historical default, so released configs need no migration).
    ///
    /// Two cases still hard-error, both about *ambiguity*, not dormant fields:
    /// `backend_kind = "cli"` with no `command` (selected but unconfigured),
    /// and a `command` with no `backend_kind` (the wizard always writes
    /// `backend_kind` when a command is present, so this only arises from a
    /// manual edit — refuse rather than guess which Backend is active).
    pub fn resolve_backend(&self) -> Result<BackendKind> {
        // Typed discriminator: serde already rejected any unknown variant at
        // config-parse time, so there is no "unknown value" branch here.
        // Absent ⇒ Api (the historical default; released configs unchanged).
        let kind = self.backend_kind.unwrap_or_default();
        let command_set = self.active_cli_command().is_some();
        match kind {
            BackendKind::Cli if !command_set => anyhow::bail!(
                "`backend_kind = \"cli\"` but no `command` is set. Add one via `aic setup` \
                 → CLI agent."
            ),
            BackendKind::Api if self.backend_kind.is_none() && command_set => anyhow::bail!(
                "`command` is set but `backend_kind` is absent. Set `backend_kind = \"cli\"` to \
                 use the CLI-agent backend, or `backend_kind = \"api\"` to keep the command \
                 dormant (it will not run until you switch)."
            ),
            // `backend_kind` is authoritative: the inactive Backend's fields
            // (an `api_key` under CLI, a `command` under API) are dormant and
            // ignored — never an error.
            _ => Ok(kind),
        }
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

    /// Auto-migrate a stale CLI-agent config written by an older aic to the
    /// current preset shape, so a preset improvement (e.g. claude's switch to
    /// `stream-json` for a live reasoning feed) reaches existing users
    /// instead of stranding them on the args they set up once and forgot.
    /// Returns one notice string per migration performed, for the caller to
    /// print — the user's file is rewritten under them, so transparency is
    /// non-negotiable.
    ///
    /// **Idempotent and conservative.** A config already on the current
    /// preset matches no legacy fingerprint and is a no-op. A custom command
    /// (one that matches no preset, current or legacy) is never touched — the
    /// user owns it. Only the `args` field is rewritten; `command`,
    /// `timeout_secs`, `backend_kind`, and all API fields are preserved
    /// verbatim. Runs only when the CLI backend is active
    /// (`backend_kind = "cli"`) — a dormant CLI config under the API backend
    /// is left for an explicit `aic setup` to refresh, so a backend the user
    /// is not using is not rewritten out from under them.
    ///
    /// Designed to be called once early in `main` on every invocation; the
    /// cost is a config read plus an exact fingerprint compare, negligible.
    pub fn migrate_if_stale() -> Result<Vec<String>> {
        let mut config = match Self::load()? {
            Some(c) => c,
            None => return Ok(Vec::new()),
        };
        // Only migrate the active CLI backend's spec — a dormant CLI config
        // under `backend_kind = "api"` is none of our business until the user
        // switches back via `aic setup`.
        if config.backend_kind != Some(BackendKind::Cli) {
            return Ok(Vec::new());
        }
        let command = match config.active_cli_command() {
            Some(c) => c.to_string(),
            None => return Ok(Vec::new()),
        };
        let args = config
            .cli
            .args
            .clone()
            .unwrap_or_else(|| vec![crate::cli_agent::PROMPT_PLACEHOLDER.to_string()]);
        let (name, new_args) = match crate::cli_agent::cli_preset_migration(&command, &args) {
            Some(m) => m,
            None => return Ok(Vec::new()),
        };
        config.cli.args = Some(new_args);
        config.save()?;
        Ok(vec![format!(
            "auto-migrated the `{name}` CLI preset to its current shape (added the \
             streaming/reasoning flags). Your stored args were a snapshot from an earlier \
             aic; preset improvements now reach you automatically. command, timeout_secs, \
             backend_kind, and API fields were left unchanged."
        )])
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

/// Which Backend a Run uses to obtain LLM answers (ADR 0011). The active kind
/// is the `backend_kind` config value, resolved and validated by
/// [`Config::resolve_backend`]. Serialized as the lowercase variant name
/// (`"api"` / `"cli"`); an unknown value is rejected at config-parse time, so
/// an invalid `backend_kind` cannot exist in a parsed [`Config`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackendKind {
    /// The API-provider Backend: calls a [`Provider`](crate::llm::Provider) over
    /// HTTP, authenticated by an `api_key`. The default when `backend_kind` is
    /// absent.
    #[default]
    Api,
    /// The CLI-agent Backend: shells out to an external coding-agent CLI in
    /// headless/print mode, reusing that CLI's own auth (ADR 0010).
    Cli,
}

impl BackendKind {
    /// Human-facing name for banners / run indicators.
    pub fn display_name(self) -> &'static str {
        match self {
            Self::Api => "API provider",
            Self::Cli => "CLI agent",
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
        // `Config` derives `Default` (every field is `Option`), so the
        // no-config case is just the default — no hand-maintained field list
        // that must be edited whenever a field is added.
        let cfg = config.cloned().unwrap_or_default();

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
    let kind = config
        .as_ref()
        .map(|c| c.resolve_backend())
        .transpose()?
        .unwrap_or(BackendKind::Api);

    println!("Backend:  {}", kind.display_name());
    match kind {
        BackendKind::Cli => {
            // resolve_backend guarantees a command is set for the CLI backend.
            let c = config.as_ref().expect("cli backend implies config present");
            let command = c
                .active_cli_command()
                .expect("cli backend implies command set");
            println!("Command:  {command} (source: {})", Source::Config);
            let (args, args_src) = match &c.cli.args {
                Some(a) => (a.join(" "), Source::Config),
                None => (
                    crate::cli_agent::PROMPT_PLACEHOLDER.to_string(),
                    Source::Default,
                ),
            };
            println!("Args:     {args} (source: {args_src})");
            let (timeout, to_src) = match c.cli.timeout_secs {
                Some(t) => (t, Source::Config),
                None => (crate::cli_agent::DEFAULT_TIMEOUT_SECS, Source::Default),
            };
            println!("Timeout:  {timeout}s (source: {to_src})");
        }
        BackendKind::Api => {
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
        }
    }

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

    #[test]
    fn resolve_backend_uses_discriminator_and_allows_dormant_fields() {
        // ADR 0011: `backend_kind` is authoritative — it alone picks the active
        // Backend. The inactive Backend's fields may sit dormant in the file
        // (preserved across switches) and are ignored, never an error. Two
        // cases still hard-error: a CLI selected but unconfigured, and a
        // `command` with no discriminator (ambiguous; the wizard always writes
        // `backend_kind` when a command is present).
        assert_eq!(
            cfg("openai", None, None, None).resolve_backend().unwrap(),
            BackendKind::Api
        );

        let explicit_api = Config {
            backend_kind: Some(BackendKind::Api),
            ..cfg("openai", Some("k"), None, None)
        };
        assert_eq!(explicit_api.resolve_backend().unwrap(), BackendKind::Api);

        // Cli with a command resolves to Cli.
        let cli = Config {
            backend_kind: Some(BackendKind::Cli),
            cli: CliConfig {
                command: Some("claude".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(cli.resolve_backend().unwrap(), BackendKind::Cli);

        // CLI selected but never configured — can't run.
        assert!(
            Config {
                backend_kind: Some(BackendKind::Cli),
                ..Default::default()
            }
            .resolve_backend()
            .is_err()
        );

        // Dormant fields are fine: explicit Api + a CLI command kept from a
        // previous switch resolves to Api (command dormant), and Cli + an
        // api_key kept from a previous switch resolves to Cli (api_key
        // dormant). Switching back restores them.
        assert_eq!(
            Config {
                backend_kind: Some(BackendKind::Api),
                cli: CliConfig {
                    command: Some("claude".into()),
                    ..Default::default()
                },
                ..Default::default()
            }
            .resolve_backend()
            .unwrap(),
            BackendKind::Api
        );
        assert_eq!(
            Config {
                backend_kind: Some(BackendKind::Cli),
                cli: CliConfig {
                    command: Some("claude".into()),
                    ..Default::default()
                },
                api_key: Some("sk-x".into()),
                ..Default::default()
            }
            .resolve_backend()
            .unwrap(),
            BackendKind::Cli
        );

        // Absent backend_kind + command — the crux of ADR 0011: the lenient
        // "infer CLI from command" rule is deliberately rejected so the config
        // cannot lie about which Backend is active. A regression here would
        // silently reintroduce the invisible-mode confusion the discriminator
        // exists to fix.
        assert!(
            Config {
                backend_kind: None,
                cli: CliConfig {
                    command: Some("claude".into()),
                    ..Default::default()
                },
                ..Default::default()
            }
            .resolve_backend()
            .is_err()
        );
    }

    /// `CliConfig::to_spec` infers the stdout [`Encoding`] from the argv
    /// template — the single source of truth shared with `aic setup` verify,
    /// so a config written by the claude preset (carrying
    /// `--output-format stream-json`) routes stdout through the NDJSON decoder.
    #[test]
    fn cli_spec_infers_encoding_from_args() {
        use crate::cli_agent::Encoding;
        // claude preset argv → stream-json decoder.
        let claude = CliConfig {
            command: Some("claude".into()),
            args: Some(vec![
                "-p".into(),
                "{prompt}".into(),
                "--output-format".into(),
                "stream-json".into(),
                "--include-partial-messages".into(),
            ]),
            ..Default::default()
        };
        assert_eq!(claude.to_spec().encoding, Encoding::ClaudeStreamJson);

        // pi `--mode json` → pi decoder.
        let pi = CliConfig {
            command: Some("pi".into()),
            args: Some(vec![
                "--no-tools".into(),
                "--mode".into(),
                "json".into(),
                "-p".into(),
                "{prompt}".into(),
            ]),
            ..Default::default()
        };
        assert_eq!(pi.to_spec().encoding, Encoding::PiStreamJson);

        // opencode `--format json` → opencode decoder.
        let oc = CliConfig {
            command: Some("opencode".into()),
            args: Some(vec![
                "run".into(),
                "--format".into(),
                "json".into(),
                "{prompt}".into(),
            ]),
            ..Default::default()
        };
        assert_eq!(oc.to_spec().encoding, Encoding::OpenCodeJson);

        // Plain argv (pre-streaming claude, codex, or any custom non-streaming
        // command) → plain: stdout is the answer verbatim, no decoder.
        let plain = CliConfig {
            command: Some("claude".into()),
            args: Some(vec!["-p".into(), "{prompt}".into()]),
            ..Default::default()
        };
        assert_eq!(plain.to_spec().encoding, Encoding::Plain);

        // Defaults: no args → `["{prompt}"]`; no timeout → 240s.
        let defaulted = CliConfig {
            command: Some("my-agent".into()),
            ..Default::default()
        };
        let spec = defaulted.to_spec();
        assert_eq!(spec.args, vec![crate::cli_agent::PROMPT_PLACEHOLDER]);
        assert_eq!(spec.timeout_secs, crate::cli_agent::DEFAULT_TIMEOUT_SECS);
    }

    #[test]
    fn unknown_backend_kind_variant_is_rejected_at_parse() {
        // The discriminator is typed, so an invalid value fails at TOML parse
        // time (it cannot exist in a parsed Config) rather than being deferred
        // to `resolve_backend`.
        let err = toml::from_str::<Config>("backend_kind = \"ollama\"\n");
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("backend_kind") || msg.contains("ollama"));
    }

    #[test]
    fn backend_kind_round_trips_through_toml() {
        // "cli" serializes + deserializes; absent stays absent (⇒ default Api).
        let c = Config {
            backend_kind: Some(BackendKind::Cli),
            ..Default::default()
        };
        let s = toml::to_string(&c).unwrap();
        assert!(s.contains("backend_kind = \"cli\""));
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.backend_kind, Some(BackendKind::Cli));
    }
}
