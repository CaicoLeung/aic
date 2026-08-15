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
    let r = ResolvedConfig::from_parts("openai-compatible".into(), "k".into(), "m".into(), None);
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

/// `aic use claude` switches to the CLI-agent backend (claude code), not the
/// Anthropic API provider — preset names win over provider aliases. The API
/// row stays dormant-but-intact for a later switch back (ADR 0011).
#[test]
fn apply_use_preset_name_switches_to_cli_agent() {
    let c = cfg("openai", Some("sk"), Some("gpt-5"), None);
    let out = super::apply_use(c, "claude").unwrap();
    assert_eq!(out.backend_kind, Some(BackendKind::Cli));
    // The full preset spec lands in config — command, args, timeout_secs,
    // encoding — pinned against `cli_preset` so a preset change that
    // `aic use` fails to propagate fails here too.
    let spec = crate::llm::cli_agent::cli_preset("claude").expect("claude preset");
    assert_eq!(out.cli.active_command(), Some(spec.command.as_str()));
    assert_eq!(out.cli.args.as_deref(), Some(spec.args.as_slice()));
    assert_eq!(out.cli.timeout_secs, Some(spec.timeout_secs));
    assert_eq!(out.cli.encoding, Some(spec.encoding));
    // The API row is untouched, just dormant.
    assert_eq!(out.backend.as_deref(), Some("openai"));
    assert_eq!(out.api_key.as_deref(), Some("sk"));
    assert_eq!(out.model.as_deref(), Some("gpt-5"));
}

/// Preset matching mirrors the clap arg's `ignore_case`: `aic use Codex` is
/// the CLI agent, not an unknown-provider error.
#[test]
fn apply_use_preset_match_is_case_insensitive() {
    let c = cfg("openai", None, None, None);
    let out = super::apply_use(c, "Codex").unwrap();
    assert_eq!(out.backend_kind, Some(BackendKind::Cli));
    assert_eq!(out.cli.active_command(), Some("codex"));
}

/// `run_use`'s print contracts, pinned on the pure core: the CLI-agent arm
/// names the agent and never emits the API-key note (the agent reuses its
/// own auth — there is no key to warn about).
#[test]
fn use_messages_cli_arm_names_agent_and_skips_api_key_note() {
    let out = super::apply_use(cfg("openai", Some("sk"), None, None), "claude").unwrap();
    let (line, note) = super::use_messages(&out);
    assert_eq!(line, "Switched to CLI agent claude.");
    assert!(note.is_none(), "CLI arm must not print the API-key note");
}

/// The provider arm warns exactly when the restored profile has no key:
/// a keyed profile stays silent, a keyless one gets the setup hint.
#[test]
fn use_messages_provider_arm_notes_only_a_missing_key() {
    let mut keyed = cfg("openai", Some("sk"), None, None);
    keyed.providers.push(ProviderProfile::new(
        "anthropic",
        Some("sk-a".into()),
        None,
        None,
    ));
    let out = super::apply_use(keyed, "anthropic").unwrap();
    let (line, note) = super::use_messages(&out);
    assert_eq!(line, "Switched to anthropic.");
    assert!(note.is_none());

    let mut keyless = cfg("openai", Some("sk"), None, None);
    keyless
        .providers
        .push(ProviderProfile::new("anthropic", None, None, None));
    let out = super::apply_use(keyless, "anthropic").unwrap();
    let (line, note) = super::use_messages(&out);
    assert_eq!(line, "Switched to anthropic.");
    let note = note.expect("keyless profile must produce the note");
    assert!(note.contains("no saved API key"), "got: {note}");
}

/// `aic use <preset>` works on a fresh machine — no config file at all —
/// because the CLI agent reuses its own auth (the documented "no setup
/// needed" promise). End-to-end through `run_use` against an isolated
/// `$HOME`, so the load/default/save path is exercised for real.
/// Unix-only: `config_path` resolves from `$HOME` there (ADR 0012).
#[cfg(unix)]
#[test]
fn run_use_preset_creates_config_on_fresh_machine() {
    let dir = tempfile::tempdir().unwrap();
    temp_env::with_var("HOME", Some(dir.path()), || {
        super::run_use("claude").unwrap();
        let saved = super::Config::load().unwrap().expect("config written");
        assert_eq!(saved.backend_kind, Some(BackendKind::Cli));
        assert_eq!(saved.cli.active_command(), Some("claude"));
    });
}

/// A provider switch still requires an existing config (banked profiles
/// come from `aic setup`); a fresh machine gets the pointed error.
/// Unix-only, same `$HOME` reason as above.
#[cfg(unix)]
#[test]
fn run_use_provider_without_config_errors() {
    let dir = tempfile::tempdir().unwrap();
    temp_env::with_var("HOME", Some(dir.path()), || {
        let err = super::run_use("openai").unwrap_err().to_string();
        assert!(err.contains("no config found"), "got: {err}");
    });
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
    use crate::llm::cli_agent::Encoding;
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
    assert_eq!(spec.args, vec![crate::llm::cli_agent::PROMPT_PLACEHOLDER]);
    assert_eq!(
        spec.timeout_secs,
        crate::llm::cli_agent::DEFAULT_TIMEOUT_SECS
    );
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
    use crate::llm::cli_agent::Encoding;
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
