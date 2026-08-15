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

use crate::llm::{BaseUrlRequirement, DEFAULT_PROVIDER, LLM, Provider};

/// The CLI-agent Backend's four config fields — a unit (command + argv
/// template + timeout + stdout encoding) that travels together through
/// [`Config`], the setup `Draft`, and [`CliSpec`](crate::cli_agent::CliSpec). On disk they stay
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
    /// time. Defaults to `["{prompt}"]`.
    pub args: Option<Vec<String>>,
    /// Per-call timeout for the CLI backend, in seconds. Defaults to 240
    /// (see [`crate::cli_agent::DEFAULT_TIMEOUT_SECS`] for why it is far
    /// above the API path's latency budget).
    pub timeout_secs: Option<u64>,
    /// How [`Self::command`]'s stdout is encoded, so aic picks the right
    /// decoder. Stated explicitly by `aic setup` (each preset knows its
    /// encoding) — NOT inferred from `args` — so adding an envelope is one
    /// site (the preset), and config-load never re-derives it. Absent (a
    /// hand-edited or pre-field config) ⇒ [`Encoding::Plain`]
    /// ([`crate::cli_agent::Encoding::Plain`]): stdout is the answer
    /// verbatim, matching the documented "custom commands run plain"
    /// contract. An unknown value is rejected at config-parse time, like
    /// [`BackendKind`].
    pub encoding: Option<crate::cli_agent::Encoding>,
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
    /// [`Encoding`](crate::cli_agent::Encoding) is read from the explicit
    /// `encoding` field (ADR 0011; stated by `aic setup` from the preset),
    /// defaulting to [`Encoding::Plain`](crate::cli_agent::Encoding::Plain)
    /// when absent — never re-derived from `args`.
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
        let encoding = self.encoding.unwrap_or_default();
        crate::cli_agent::CliSpec {
            command,
            args,
            timeout_secs,
            encoding,
        }
    }
}

/// One remembered API-provider profile — the key/model/base-url bundle a user
/// configured once, kept around so switching providers (via `aic setup` or
/// `aic use`) restores them instead of asking again. The active provider's
/// values ALSO live as top-level [`Config`] fields (the on-disk shape released
/// configs already have); this list is the memory bank the active row is
/// projected from / swapped into.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderProfile {
    /// Canonical provider name (e.g. `"openai"`) — the key this profile is
    /// looked up by. Matches
    /// [`Provider::name`](crate::llm::Provider::name).
    pub backend: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
}

impl ProviderProfile {
    /// Build a profile from a provider name and its active key/model/base_url
    /// — the one shape every "remember the active provider" site captures
    /// (setup save, setup switch, `aic use` switch), so the field bundle is
    /// assembled in one place instead of three.
    pub fn new(
        backend: impl Into<String>,
        api_key: Option<String>,
        model: Option<String>,
        base_url: Option<String>,
    ) -> Self {
        Self {
            backend: backend.into(),
            api_key,
            model,
            base_url,
        }
    }

    /// This profile's key/model/base_url as a tuple ready to destructure onto
    /// the active fields of a [`Config`] (in `aic use`) or a `setup::Draft`
    /// (in the wizard's provider switch). The projection lives on the profile
    /// — which owns the fields — instead of each caller reaching in
    /// field-by-field.
    pub fn project_fields(&self) -> (Option<String>, Option<String>, Option<String>) {
        (
            self.api_key.clone(),
            self.model.clone(),
            self.base_url.clone(),
        )
    }

    /// Upsert by `backend` with full replace: overwrite the existing entry in
    /// place, or append. Use when the source is an explicit commit (the setup
    /// wizard's save), where a deliberately cleared field must be honoured.
    /// For transient switches use [`ProviderProfile::bank_active`].
    pub fn upsert(list: &mut Vec<Self>, profile: Self) {
        if let Some(slot) = list.iter_mut().find(|p| p.backend == profile.backend) {
            *slot = profile;
        } else {
            list.push(profile);
        }
    }

    /// Remember a provider into the bank with merge semantics: update an
    /// existing entry in place (or append), taking each field from `profile`
    /// only when it is `Some`, so an inattentive switch never erases a
    /// previously remembered key/model/base_url. Used by both switch paths
    /// (`aic use` and the wizard's provider step), where the active row is
    /// just passing through and a blank field is not a deliberate deletion.
    pub fn bank_active(list: &mut Vec<Self>, profile: Self) {
        if let Some(slot) = list.iter_mut().find(|p| p.backend == profile.backend) {
            if profile.api_key.is_some() {
                slot.api_key = profile.api_key;
            }
            if profile.model.is_some() {
                slot.model = profile.model;
            }
            if profile.base_url.is_some() {
                slot.base_url = profile.base_url;
            }
        } else {
            list.push(profile);
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
    /// Remembered API-provider profiles — the key/model/base-url bundle per
    /// provider, so switching providers restores them instead of re-asking.
    /// Written by `aic setup`, read by `aic use` and by setup's provider
    /// switch. `#[serde(default)]` so pre-bank configs load with an empty
    /// list and are folded in by [`setup::seed_draft`] on the next save.
    #[serde(default)]
    pub providers: Vec<ProviderProfile>,
}

pub fn config_path() -> Option<PathBuf> {
    // ADR 0012: fixed `~/.config/aic/config.toml` on every platform, resolved
    // from `home_dir()` rather than `dirs::config_dir()`. The OS-native
    // `config_dir()` put macOS configs at `~/Library/Application Support/aic`,
    // which contradicted what every doc already claimed; one fixed path makes
    // the docs truthful. `XDG_CONFIG_HOME` is deliberately ignored. Pre-0012
    // configs at the old default are adopted by [`Config::migrate_location`].
    dirs::home_dir().map(|p| p.join(".config").join("aic").join("config.toml"))
}

/// The legacy config path used before ADR 0012 — `dirs::config_dir()` joined
/// with `aic/config.toml`. On macOS this is `~/Library/Application
/// Support/aic/config.toml`; on plain Linux (no `XDG_CONFIG_HOME`) it coincides
/// with [`config_path`], so there is nothing to migrate. Used only by
/// [`Config::migrate_location`] to locate a pre-0012 config to adopt.
fn legacy_config_path() -> Option<PathBuf> {
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
        // Tighten a pre-existing config (created before the 0600 fix) down to
        // owner-only on first load — best-effort, never fatal: a chmod failure
        // must not block the read that just succeeded.
        restrict_file(&path);
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

        // The config file holds the provider API key, so its parent directory
        // and the file itself are created owner-only from the start. Opening
        // with the mode set directly means the key is never world-readable —
        // not even for the moment between `fs::write` and a later `chmod`.
        if let Some(parent) = path.parent() {
            create_secure_dir(parent)?;
        }

        let content = toml::to_string_pretty(self).context("failed to serialize config")?;
        write_secret_file(&path, &content)?;
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

    /// One-time **location migration** (ADR 0012): move a pre-0012 config
    /// written to the old OS-native default ([`legacy_config_path`], i.e.
    /// `~/Library/Application Support/aic/config.toml` on macOS) into the
    /// fixed [`config_path`] location (`~/.config/aic/config.toml`), so
    /// existing macOS users' configs follow the path the docs have always
    /// claimed.
    ///
    /// **Semantics (decided by grilling, see ADR 0012):**
    /// - old exists, new missing → **copy** old → new, then **delete** old
    ///   (move semantics — no stale duplicate that a later edit to the old
    ///   path would desync, per ADR 0008's single source of truth);
    /// - new already exists → **skip silently** (new wins; old file, if any,
    ///   is left untouched);
    /// - old and new resolve to the same path (plain Linux) → no-op;
    /// - old missing → no-op.
    ///
    /// Idempotent: after the first successful move the new path exists, so
    /// every later call takes the "skip silently" row. Prints one notice per
    /// file actually moved (transparency, matching [`Self::migrate_if_stale`]);
    /// a failure is returned for the caller to log without blocking the run.
    ///
    /// Designed to be called once early in `main`, **before**
    /// [`Self::migrate_if_stale`], so the file lands at its new path first and
    /// preset migration then runs on the relocated file.
    pub fn migrate_location() -> Result<Vec<String>> {
        let new_path = match config_path() {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };
        let old_path = match legacy_config_path() {
            Some(p) => p,
            None => return Ok(Vec::new()),
        };
        // Same path (plain Linux, no XDG) — nothing to migrate.
        if old_path == new_path {
            return Ok(Vec::new());
        }
        // New wins: if the destination already exists, skip silently. Do not
        // touch the old file in this case — the user (or a newer aic) owns the
        // new one.
        if new_path.exists() {
            return Ok(Vec::new());
        }
        if !old_path.exists() {
            return Ok(Vec::new());
        }
        if let Some(parent) = new_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        fs::copy(&old_path, &new_path).with_context(|| {
            format!(
                "failed to copy {} -> {}",
                old_path.display(),
                new_path.display()
            )
        })?;
        fs::remove_file(&old_path)
            .with_context(|| format!("failed to remove old {}", old_path.display()))?;
        Ok(vec![format!(
            "moved your config from {} to {} (the config location is now `~/.config/aic` on \
             all platforms; see ADR 0012).",
            old_path.display(),
            new_path.display()
        )])
    }
}

/// Create `path` (and missing parents) with owner-only permissions on Unix
/// (`0700`); default recursive `mkdir` elsewhere. Used for the config
/// directory, which holds the provider API key.
fn create_secure_dir(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(path)
            .with_context(|| format!("failed to create {}", path.display()))?;
    }
    Ok(())
}

/// Overwrite `path` with `content` at owner-only permissions on Unix (`0600`).
/// The file is opened with the mode set directly, so a secret is never
/// world-readable between the write and a later `chmod`. Truncates if present.
fn write_secret_file(path: &std::path::Path, content: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("failed to open {}", path.display()))?;
        file.write_all(content.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, content)
            .with_context(|| format!("failed to write {}", path.display()))?;
    }
    Ok(())
}

/// Best-effort: tighten an existing file to owner-only (`0600`) on Unix.
/// Errors are deliberately swallowed — this only fixes up config files that
/// pre-date the permission fix, and a failure to chmod must never block the
/// read that discovered the file. No-op off Unix.
fn restrict_file(path: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(path) {
            let mut perms = meta.permissions();
            // Only tighten — skip when already owner-only.
            if perms.mode() & 0o077 != 0 {
                perms.set_mode(0o600);
                let _ = std::fs::set_permissions(path, perms);
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
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

    /// Build a [`ResolvedConfig`] from already-effective values (the setup
    /// wizard's Verify step: the draft choice, else the provider default) —
    /// the same pipeline the Run path runs (`resolve` → `validate` →
    /// `to_llm`), minus the config-file read. Sources are all
    /// [`Source::Default`]: provenance is display-only, and the wizard shows
    /// sources itself via the `resolve_*` helpers.
    pub(crate) fn from_parts(
        backend: String,
        api_key: String,
        model: String,
        base_url: Option<String>,
    ) -> Self {
        Self {
            backend,
            backend_source: Source::Default,
            api_key,
            api_key_source: Source::Default,
            model,
            model_source: Source::Default,
            base_url,
            base_url_source: Source::Default,
        }
    }

    /// Build the API-backend [`LLM`] these resolved values select — the
    /// single construction seam, used by both the Run path
    /// ([`crate::llm::LlmConfig::load`]) and the setup wizard's Verify step
    /// (mirrors [`CliConfig::to_spec`]).
    pub fn to_llm(&self) -> LLM {
        LLM::new(
            Provider::from_name(&self.backend),
            self.model.clone(),
            self.api_key.clone(),
            self.base_url.clone(),
        )
    }

    /// Validate provider-specific requirements (a model or base URL the provider
    /// cannot default). Called when constructing an `LLM`, not when merely
    /// displaying resolved config (`aic list`).
    pub fn validate(&self) -> Result<()> {
        if !Provider::is_known_name(&self.backend) {
            anyhow::bail!(
                "unknown backend '{}'; run `aic setup` to pick one of: {}",
                self.backend,
                Provider::all()
                    .iter()
                    .map(|p| p.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
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

/// Pure: the lines `aic list` prints for a loaded config, by resolved backend.
/// Tested without IO so the CLI-backend branch — which re-resolves the
/// args/timeout with source tracking — is covered, not just the API path that
/// goes through [`ResolvedConfig`] (cf. [`apply_use`] for the same split).
fn list_lines(config: Option<&Config>) -> Result<Vec<String>> {
    let kind = config
        .map(|c| c.resolve_backend())
        .transpose()?
        .unwrap_or(BackendKind::Api);
    let mut lines = vec![format!("Backend:  {}", kind.display_name())];
    match kind {
        BackendKind::Cli => {
            // resolve_backend guarantees a command is set for the CLI backend.
            let c = config.expect("cli backend implies config present");
            let command = c
                .active_cli_command()
                .expect("cli backend implies command set");
            lines.push(format!("Command:  {command} (source: {})", Source::Config));
            let (args, args_src) = match &c.cli.args {
                Some(a) => (a.join(" "), Source::Config),
                None => (
                    crate::cli_agent::PROMPT_PLACEHOLDER.to_string(),
                    Source::Default,
                ),
            };
            lines.push(format!("Args:     {args} (source: {args_src})"));
            let (timeout, to_src) = match c.cli.timeout_secs {
                Some(t) => (t, Source::Config),
                None => (crate::cli_agent::DEFAULT_TIMEOUT_SECS, Source::Default),
            };
            lines.push(format!("Timeout:  {timeout}s (source: {to_src})"));
        }
        BackendKind::Api => {
            let resolved = ResolvedConfig::resolve(config);
            lines.push(format!(
                "Provider: {} (source: {})",
                resolved.backend, resolved.backend_source
            ));
            lines.push(format!(
                "Model:    {} (source: {})",
                resolved.model, resolved.model_source
            ));
            lines.push(format!(
                "API key:  {} (source: {})",
                resolved.mask_api_key(),
                resolved.api_key_source
            ));
            lines.push(format!(
                "Base URL: {} (source: {})",
                resolved.base_url.as_deref().unwrap_or("(none)"),
                resolved.base_url_source
            ));
            let saved: Vec<&str> = config
                .map(|c| c.providers.iter().map(|p| p.backend.as_str()).collect())
                .unwrap_or_default();
            if !saved.is_empty() {
                lines.push(format!(
                    "Saved:   {} (switch with `aic use <name>`)",
                    saved.join(", ")
                ));
            }
        }
    }
    Ok(lines)
}

/// `aic list` — print the resolved configuration. A thin load → format → print
/// shell over [`list_lines`]; all presentation lives there.
pub fn run_list() -> Result<()> {
    let config = Config::load()?;
    for line in list_lines(config.as_ref())? {
        println!("{line}");
    }
    Ok(())
}

/// Pure core of `aic use <name>`: validate the name, bank the currently
/// active provider's live top-level state into the memory bank, then activate
/// the target profile (restore its key/model/base_url and force the API
/// backend). Split from [`run_use`] (which owns the load/save/print IO) so the
/// switch contract — source banked, target restored, backend forced to API —
/// is unit-testable without the real config file.
///
/// Errors: unknown provider name; a known name with no banked profile (run
/// `aic setup` to add one).
fn apply_use(mut config: Config, name: &str) -> Result<Config> {
    if !Provider::is_known_name(name) {
        anyhow::bail!(
            "unknown provider '{name}'; pick one of: {}",
            Provider::all()
                .iter()
                .map(|p| p.name())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    // Normalize via from_name so aliases and case variants match the stored
    // `backend` key (the canonical name setup writes).
    let normalized = Provider::from_name(name).name();
    let target = config
        .providers
        .iter()
        .find(|p| p.backend == normalized)
        .cloned()
        .with_context(|| {
            format!(
                "provider '{normalized}' has not been configured yet — run `aic setup` to add it"
            )
        })?;

    // Bank the provider we're leaving first (its live top-level state), so a
    // later switch back restores what was active — not whatever `finalize`
    // last happened to write. Merge semantics, so a blank top-level field
    // never erases a previously remembered value.
    if let Some(leaving) = config.backend.as_deref()
        && leaving != normalized
    {
        ProviderProfile::bank_active(
            &mut config.providers,
            ProviderProfile::new(
                leaving.to_string(),
                config.api_key.clone(),
                config.model.clone(),
                config.base_url.clone(),
            ),
        );
    }

    (config.api_key, config.model, config.base_url) = target.project_fields();
    config.backend = Some(normalized.to_string());
    // `aic use` is an API-backend action; make it active. A stored CLI command
    // (if any) stays dormant, restorable via `aic setup` → Backend.
    config.backend_kind = Some(BackendKind::Api);
    Ok(config)
}

/// `aic use <provider>` — switch the active API provider by restoring a
/// remembered profile, without re-entering the key/model. The provider must
/// already have been configured via `aic setup` (so it has an entry in the
/// `providers` bank). Switches the active backend to API (a CLI-agent user
/// who runs `aic use` is asking for the API path); any stored CLI fields
/// stay dormant for a switch back via `aic setup`, per ADR 0011.
pub fn run_use(name: &str) -> Result<()> {
    let mut config = Config::load()
        .ok()
        .flatten()
        .context("no config found — run `aic setup` to configure a provider first")?;
    config = apply_use(config, name)?;

    // The activated profile's key (now in the top-level row after apply_use).
    let normalized = config.backend.as_deref().unwrap_or(name);
    let had_key = config
        .api_key
        .as_deref()
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    config.save()?;

    println!("Switched to {normalized}.");
    if !had_key {
        eprintln!(
            "note: {normalized} has no saved API key — run `aic setup` to add one if it's needed"
        );
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

    #[test]
    fn validate_rejects_unknown_backend() {
        let config = Config {
            backend: Some("anthpopic".into()),
            api_key: Some("k".into()),
            // backend_kind / cli / model / base_url / confirm default — the
            // typo'd `backend` is what validate() must catch on the API path.
            ..Default::default()
        };
        let resolved = ResolvedConfig::resolve(Some(&config));
        let err = resolved.validate().unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("unknown backend"), "got: {msg}");
        assert!(
            msg.contains("anthpopic"),
            "should echo the bad value: {msg}"
        );
        assert!(
            msg.contains("anthropic"),
            "should list valid names as a hint: {msg}"
        );
    }

    /// The two required-field branches of [`ResolvedConfig::validate`] (ported
    /// from the setup wizard's deleted `verify_preflight`): a provider that
    /// cannot default its base URL, and one that cannot default its model.
    #[test]
    fn validate_requires_base_url_and_model() {
        // openai-compatible requires a base URL it cannot default.
        let r =
            ResolvedConfig::from_parts("openai-compatible".into(), "k".into(), "m".into(), None);
        let msg = format!("{:#}", r.validate().unwrap_err());
        assert!(msg.contains("base URL"), "got: {msg}");

        // OpenRouter has no default model — an empty model fails with a hint.
        let r = ResolvedConfig::from_parts("openrouter".into(), "k".into(), String::new(), None);
        let msg = format!("{:#}", r.validate().unwrap_err());
        assert!(msg.contains("model"), "got: {msg}");

        // OpenAI needs no base URL; a present model (here the provider
        // default an effective-model resolve would supply) validates.
        let r = ResolvedConfig::from_parts("openai".into(), "k".into(), "gpt-5".into(), None);
        assert!(r.validate().is_ok());
    }

    /// The config file holds an API key, so the write helper must land it
    /// owner-only (0600) on Unix — never world-readable.
    #[cfg(unix)]
    #[test]
    fn write_secret_file_is_owner_only_on_unix() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        super::write_secret_file(&path, "backend = \"openai\"\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "secret file must be owner-only (0600), got {:o}",
            mode
        );
    }

    /// `restrict_file` pulls a world-readable file down to 0600 and leaves an
    /// already-tight file alone.
    #[cfg(unix)]
    #[test]
    fn restrict_file_tightens_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("existing.toml");
        std::fs::write(&path, "x").unwrap();
        // Force a permissive mode, then tighten.
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        super::restrict_file(&path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "should tighten 0644 → 0600, got {:o}",
            mode
        );

        // Already owner-only → unchanged.
        super::restrict_file(&path);
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
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
    fn provider_profile_upsert_replaces_or_appends() {
        let mut list = Vec::new();
        ProviderProfile::upsert(
            &mut list,
            ProviderProfile {
                backend: "openai".into(),
                api_key: Some("k1".into()),
                ..Default::default()
            },
        );
        ProviderProfile::upsert(
            &mut list,
            ProviderProfile {
                backend: "anthropic".into(),
                ..Default::default()
            },
        );
        // Replace openai in place, not append a second openai.
        ProviderProfile::upsert(
            &mut list,
            ProviderProfile {
                backend: "openai".into(),
                api_key: Some("k2".into()),
                ..Default::default()
            },
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].backend, "openai");
        assert_eq!(list[0].api_key.as_deref(), Some("k2"));
        assert_eq!(list[1].backend, "anthropic");
    }

    /// The `[[providers]]` bank round-trips through TOML so a config saved
    /// by one aic run loads back with every remembered provider intact — the
    /// on-disk contract `aic setup`/`aic use` depend on.
    #[test]
    fn providers_bank_round_trips_through_toml() {
        let c = Config {
            backend: Some("openai".into()),
            api_key: Some("sk-live".into()),
            model: Some("gpt-5".into()),
            providers: vec![
                ProviderProfile {
                    backend: "openai".into(),
                    api_key: Some("sk-live".into()),
                    model: Some("gpt-5".into()),
                    base_url: None,
                },
                ProviderProfile {
                    backend: "anthropic".into(),
                    api_key: Some("sk-ant".into()),
                    model: Some("claude-x".into()),
                    base_url: None,
                },
            ],
            ..Default::default()
        };
        let s = toml::to_string(&c).unwrap();
        assert!(s.contains("[[providers]]"));
        let back: Config = toml::from_str(&s).unwrap();
        assert_eq!(back.providers.len(), 2);
        assert_eq!(back.providers[1].backend, "anthropic");
        assert_eq!(back.providers[1].api_key.as_deref(), Some("sk-ant"));
    }

    /// A pre-bank config (no `[[providers]]` table) still loads: the field is
    /// `#[serde(default)]` and comes back empty, ready to be populated on the
    /// next save by `setup::seed_draft`'s legacy fold.
    #[test]
    fn config_without_providers_loads_as_empty() {
        let raw = r#"
backend = "openai"
api_key = "sk-x"
model = "gpt-5"
"#;
        let c: Config = toml::from_str(raw).unwrap();
        assert_eq!(c.backend.as_deref(), Some("openai"));
        assert!(c.providers.is_empty());
    }

    #[test]
    fn provider_profile_new_builds_from_active_fields() {
        let p = ProviderProfile::new("openai", Some("k".into()), Some("m".into()), None);
        assert_eq!(p.backend, "openai");
        assert_eq!(p.api_key.as_deref(), Some("k"));
        assert_eq!(p.model.as_deref(), Some("m"));
        assert!(p.base_url.is_none());
    }

    /// `bank_active` is the merge upsert the switch paths depend on: an
    /// existing entry is updated field-by-field only where the incoming value
    /// is set (so a blank never erases a remembered key/model), and an unknown
    /// provider is appended.
    #[test]
    fn provider_profile_bank_active_merges_and_appends() {
        let mut list = vec![ProviderProfile::new(
            "openai",
            Some("k1".into()),
            Some("m1".into()),
            None,
        )];
        // Merge: incoming key set (overwrites), incoming model None (keeps m1).
        ProviderProfile::bank_active(
            &mut list,
            ProviderProfile::new("openai", Some("k2".into()), None, None),
        );
        assert_eq!(list[0].api_key.as_deref(), Some("k2"));
        assert_eq!(list[0].model.as_deref(), Some("m1"));
        // Append when the backend is not in the bank.
        ProviderProfile::bank_active(
            &mut list,
            ProviderProfile::new("anthropic", Some("ka".into()), None, None),
        );
        assert_eq!(list.len(), 2);
        assert_eq!(list[1].backend, "anthropic");
    }

    #[test]
    fn provider_profile_project_fields_returns_active_tuple() {
        let p = ProviderProfile::new(
            "openai",
            Some("k".into()),
            Some("m".into()),
            Some("u".into()),
        );
        let (k, m, u) = p.project_fields();
        assert_eq!(k.as_deref(), Some("k"));
        assert_eq!(m.as_deref(), Some("m"));
        assert_eq!(u.as_deref(), Some("u"));
    }

    /// `aic use`'s pure core rejects an unknown provider name.
    #[test]
    fn apply_use_rejects_unknown_provider() {
        let c = cfg("openai", None, None, None);
        let err = super::apply_use(c, "not-a-provider").unwrap_err();
        assert!(err.to_string().contains("unknown provider"), "got: {err}");
    }

    /// A known name with no banked profile is unconfigured — `aic use` must
    /// refuse rather than activate blanks.
    #[test]
    fn apply_use_rejects_unconfigured_provider() {
        let c = cfg("openai", Some("sk"), None, None); // bank empty
        let err = super::apply_use(c, "anthropic").unwrap_err();
        assert!(
            err.to_string().contains("has not been configured"),
            "got: {err}"
        );
    }

    /// The headline `aic use` contract: activate the target (restore its
    /// key/model/base_url, force the API backend) AND bank the provider being
    /// left with its live top-level state — so a hand-edited key is not lost.
    #[test]
    fn apply_use_banks_source_and_activates_target() {
        // Active openai with a hand-edited top-level key not yet in the bank.
        let mut c = cfg("openai", Some("sk-handedited"), Some("gpt-5"), None);
        c.providers = vec![
            ProviderProfile::new("openai", Some("sk-old".into()), Some("gpt-5".into()), None),
            ProviderProfile::new(
                "anthropic",
                Some("sk-a".into()),
                Some("claude-x".into()),
                None,
            ),
        ];
        let out = super::apply_use(c, "anthropic").unwrap();
        // Source (openai) banked with the live top-level key, not left stale.
        let openai = out
            .providers
            .iter()
            .find(|p| p.backend == "openai")
            .expect("openai kept in bank");
        assert_eq!(openai.api_key.as_deref(), Some("sk-handedited"));
        // Target activated.
        assert_eq!(out.backend.as_deref(), Some("anthropic"));
        assert_eq!(out.api_key.as_deref(), Some("sk-a"));
        assert_eq!(out.model.as_deref(), Some("claude-x"));
        assert_eq!(out.backend_kind, Some(BackendKind::Api));
    }

    /// Merge contract on the switch path: a blank top-level field must not
    /// erase a value the bank already remembers (the blank-overwrite bug).
    #[test]
    fn apply_use_merge_keeps_banked_value_when_source_field_blank() {
        // Active openai with NO top-level key, but the bank remembers one.
        let mut c = cfg("openai", None, Some("gpt-5"), None);
        c.providers = vec![
            ProviderProfile::new(
                "openai",
                Some("sk-remembered".into()),
                Some("gpt-5".into()),
                None,
            ),
            ProviderProfile::new("anthropic", Some("sk-a".into()), None, None),
        ];
        let out = super::apply_use(c, "anthropic").unwrap();
        let openai = out
            .providers
            .iter()
            .find(|p| p.backend == "openai")
            .unwrap();
        assert_eq!(openai.api_key.as_deref(), Some("sk-remembered"));
    }

    #[test]
    fn list_lines_api_branch_shows_resolved_provider() {
        let c = cfg("openai", Some("sk-live-key"), Some("gpt-4o"), None);
        let lines = super::list_lines(Some(&c)).unwrap();
        assert_eq!(lines[0], "Backend:  API provider");
        assert!(
            lines.iter().any(|l| l.contains("Provider: openai")),
            "got {lines:?}"
        );
        assert!(
            lines.iter().any(|l| l.contains("Model:    gpt-4o")),
            "got {lines:?}"
        );
        // mask_api_key: first 3 … last 3 of an 11-char key.
        assert!(
            lines
                .iter()
                .any(|l| l.contains("API key:") && l.contains("sk-...key")),
            "got {lines:?}"
        );
    }

    #[test]
    fn list_lines_cli_branch_resolves_defaults_with_source() {
        // Command set, args/timeout absent → both resolve to defaults (source:
        // default), the previously-untested branch.
        let c = Config {
            backend_kind: Some(BackendKind::Cli),
            cli: CliConfig {
                command: Some("claude".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let lines = super::list_lines(Some(&c)).unwrap();
        assert_eq!(lines[0], "Backend:  CLI agent");
        assert!(
            lines
                .iter()
                .any(|l| l == "Command:  claude (source: config)"),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Args:") && l.contains("source: default")),
            "got {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("Timeout:") && l.contains("source: default")),
            "got {lines:?}"
        );
    }

    #[test]
    fn list_lines_no_config_defaults_to_api_backend() {
        // No config file at all ⇒ Api backend, every resolved value default-sourced.
        let lines = super::list_lines(None).unwrap();
        assert_eq!(lines[0], "Backend:  API provider");
        assert!(
            lines.iter().all(|l| !l.contains("source: config")),
            "nothing config-sourced: {lines:?}"
        );
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

    /// `CliConfig::to_spec` reads the stdout [`Encoding`] from the explicit
    /// `encoding` field (stated by `aic setup` from the preset) — never
    /// re-derived from `args`. Absent ⇒ `Encoding::Plain` (the documented
    /// "custom commands run plain" contract).
    #[test]
    fn cli_spec_uses_explicit_encoding_field() {
        use crate::cli_agent::Encoding;
        // A preset-written config states its encoding; to_spec uses it as-is,
        // regardless of the argv.
        let claude = CliConfig {
            command: Some("claude".into()),
            args: Some(vec!["-p".into(), "{prompt}".into()]),
            encoding: Some(Encoding::ClaudeStreamJson),
            ..Default::default()
        };
        assert_eq!(claude.to_spec().encoding, Encoding::ClaudeStreamJson);

        // The argv no longer selects encoding: a codex argv with no encoding
        // field yields Plain (the field is authoritative, the flags are not).
        let codex_argv_no_field = CliConfig {
            command: Some("codex".into()),
            args: Some(vec![
                "exec".into(),
                "--json".into(),
                "-s".into(),
                "read-only".into(),
                "{prompt}".into(),
            ]),
            ..Default::default()
        };
        assert_eq!(codex_argv_no_field.to_spec().encoding, Encoding::Plain);

        // Defaults: no args → `["{prompt}"]`; no timeout → 240s; no encoding
        // → Plain.
        let defaulted = CliConfig {
            command: Some("my-agent".into()),
            ..Default::default()
        };
        let spec = defaulted.to_spec();
        assert_eq!(spec.args, vec![crate::cli_agent::PROMPT_PLACEHOLDER]);
        assert_eq!(spec.timeout_secs, crate::cli_agent::DEFAULT_TIMEOUT_SECS);
        assert_eq!(spec.encoding, Encoding::Plain);
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

    #[test]
    fn cli_encoding_round_trips_through_toml() {
        // The `encoding` field serializes to the snake_case variant name and
        // deserializes back. Each preset's encoding round-trips.
        use crate::cli_agent::Encoding;
        for enc in [
            Encoding::Plain,
            Encoding::ClaudeStreamJson,
            Encoding::PiStreamJson,
            Encoding::OpenCodeJson,
            Encoding::CodexJson,
        ] {
            let c = Config {
                backend_kind: Some(BackendKind::Cli),
                cli: CliConfig {
                    command: Some("x".into()),
                    encoding: Some(enc),
                    ..Default::default()
                },
                ..Default::default()
            };
            let s = toml::to_string(&c).unwrap();
            let back: Config = toml::from_str(&s).unwrap();
            assert_eq!(
                back.cli.encoding,
                Some(enc),
                "round-trip failed for {enc:?}"
            );
        }
    }

    #[test]
    fn unknown_cli_encoding_is_rejected_at_parse() {
        // Like backend_kind, the encoding is typed — an unknown value fails at
        // TOML parse time and can never exist in a parsed Config.
        let err = toml::from_str::<Config>(
            "backend_kind = \"cli\"\ncommand = \"x\"\nencoding = \"telepathy\"\n",
        );
        assert!(err.is_err());
        let msg = err.unwrap_err().to_string();
        assert!(msg.contains("encoding") || msg.contains("telepathy"));
    }
}
