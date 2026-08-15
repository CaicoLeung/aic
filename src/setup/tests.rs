use super::cli_flow::*;
use super::finalize::*;
use super::provider::*;
use super::*;
use crate::cli_agent::PRESETS;
use crate::config::{Source, resolve_api_key};

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
        ..Default::default()
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
        ..Default::default()
    });
    let d = seed_draft(&existing);
    assert_eq!(d.confirm_before_commit, Some(true));
    assert_eq!(d.provider, None);
}

#[test]
fn provider_label_defaults_to_openai_for_api_backend() {
    // API backend (the default) with no provider chosen shows the OpenAI
    // default + its default model, mirroring `finalize` — never "(not set)"
    // while API is active.
    let d = draft(None, None, None, None);
    assert_eq!(provider_label(&d), "OpenAI · gpt-5-mini");

    // Explicit API backend, still no provider -> same default.
    let mut d = draft(None, None, None, None);
    d.backend_kind = Some(BackendKind::Api);
    assert_eq!(provider_label(&d), "OpenAI · gpt-5-mini");

    // CLI backend ignores the provider -> "(not set)" until one is set.
    let mut d = draft(None, None, None, None);
    d.backend_kind = Some(BackendKind::Cli);
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
    // API key: masked, (not set) when empty.
    assert_eq!(api_key_label("sk-123"), "••••••");
    assert_eq!(api_key_label(""), "(not set)");

    // Model: value, (default) when provider-default-sourced, (not set) when empty.
    assert_eq!(model_label("gpt-5", Source::Config), "gpt-5");
    assert_eq!(
        model_label("deepseek-v4-flash", Source::Default),
        "deepseek-v4-flash (default)"
    );
    assert_eq!(model_label("", Source::Default), "(not set)");

    // Base URL: value, annotated by source, (not set) when none.
    assert_eq!(
        base_url_label(Some("http://h:1"), Source::Config),
        "http://h:1"
    );
    assert_eq!(
        base_url_label(Some("http://localhost:11434"), Source::Default),
        "http://localhost:11434 (default)"
    );
    assert_eq!(base_url_label(None, Source::Default), "(not set)");
}

#[test]
fn provider_submenu_entries_follow_applicability() {
    // OpenAI: API key + Model + Verify + Done (no base URL).
    let d = draft(Some(Provider::OpenAI), None, None, None);
    let (entries, _) = provider_submenu_items(&d);
    assert_eq!(
        entries,
        vec![
            ProviderEntry::ApiKey,
            ProviderEntry::Model,
            ProviderEntry::Verify,
            ProviderEntry::Done
        ]
    );

    // Ollama: Base URL + Model + Verify + Done (no API key).
    let d = draft(Some(Provider::Ollama), None, None, None);
    let (entries, _) = provider_submenu_items(&d);
    assert_eq!(
        entries,
        vec![
            ProviderEntry::BaseUrl,
            ProviderEntry::Model,
            ProviderEntry::Verify,
            ProviderEntry::Done
        ]
    );

    // OpenAI-compatible: API key + Base URL + Model + Verify + Done.
    let d = draft(Some(Provider::OpenAiCompatible), None, None, None);
    let (entries, _) = provider_submenu_items(&d);
    assert_eq!(
        entries,
        vec![
            ProviderEntry::ApiKey,
            ProviderEntry::BaseUrl,
            ProviderEntry::Model,
            ProviderEntry::Verify,
            ProviderEntry::Done
        ]
    );
}

#[test]
fn submenu_labels_show_effective_value() {
    // The in-session draft (a user choice or a seeded config value) is the
    // effective value and is shown as-is — re-entering setup must not read
    // as if the choice was lost (AIC-15).
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
            ProviderEntry::Verify,
            ProviderEntry::Done
        ]
    );
    assert_eq!(labels[0], "🔑 API key — ••••••");
    assert_eq!(labels[1], "🧠 Model — deepseek-v4-pro");
    assert_eq!(
        labels[2],
        "🔌 Verify — test this provider with a sample request"
    );
    assert_eq!(labels[3], "↩️ Done — back to main menu");
}

#[test]
fn effective_model_prefers_draft_then_default() {
    // Draft model wins over the provider default.
    let d = draft(Some(Provider::OpenAI), None, Some("gpt-5"), None);
    assert_eq!(d.effective_model(Provider::OpenAI), "gpt-5");
    // No draft model -> provider default.
    let d = draft(Some(Provider::OpenAI), None, None, None);
    assert_eq!(d.effective_model(Provider::OpenAI), "gpt-5-mini");
    // Provider with no default -> empty string.
    let d = draft(Some(Provider::OpenRouter), None, None, None);
    assert_eq!(d.effective_model(Provider::OpenRouter), "");
}

#[test]
fn provider_choice_label_shows_chosen_model_for_selected_provider() {
    // The selected provider shows the user's chosen model, not the bare
    // default — re-entering setup must not read as if the choice was lost
    // (AIC-15).
    let mut d = draft(Some(Provider::DeepSeek), None, None, None);
    d.model = Some("deepseek-v4-pro".into());
    assert_eq!(
        provider_choice_label(Provider::DeepSeek, &d),
        "DeepSeek  (deepseek-v4-pro)"
    );

    // Other providers still show their default for comparison.
    assert_eq!(
        provider_choice_label(Provider::OpenAI, &d),
        "OpenAI  (gpt-5-mini)"
    );

    // No chosen model -> the provider default for the selected provider.
    let d = draft(Some(Provider::DeepSeek), None, None, None);
    assert_eq!(
        provider_choice_label(Provider::DeepSeek, &d),
        "DeepSeek  (deepseek-v4-flash)"
    );

    // Selected provider with no default and a chosen model -> chosen model.
    let mut d = draft(Some(Provider::OpenRouter), None, None, None);
    d.model = Some("meta-llama/llama-4-scout".into());
    assert_eq!(
        provider_choice_label(Provider::OpenRouter, &d),
        "OpenRouter  (meta-llama/llama-4-scout)"
    );

    // Selected provider with no default and no chosen model -> the hint.
    let d = draft(Some(Provider::OpenRouter), None, None, None);
    assert_eq!(
        provider_choice_label(Provider::OpenRouter, &d),
        "OpenRouter  (no default — you'll pick a model)"
    );
}

#[test]
fn field_initial_precedence() {
    let existing: Option<Config> = Some(cfg("openai", Some("old-key"), Some("old-model"), None));
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
        ..Default::default()
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

#[test]
fn resolve_api_key_uses_config_value() {
    // aic reads only the config file: the config value is used as-is.
    let (key, source) = resolve_api_key(Some("sk-config"));
    assert_eq!(key, "sk-config");
    assert_eq!(source, Source::Config);
    // No config value -> empty, default source.
    let (key, source) = resolve_api_key(None);
    assert_eq!(key, "");
    assert_eq!(source, Source::Default);
}

fn draft_with_cli(command: Option<&str>) -> Draft {
    Draft {
        backend_kind: command.map(|_| BackendKind::Cli),
        provider: Some(Provider::OpenAI),
        api_key: Some("sk-stale".into()),
        model: Some("gpt-5".into()),
        base_url: None,
        confirm_before_commit: Some(true),
        cli: CliConfig {
            command: command.map(String::from),
            args: Some(vec!["-p".into(), "{prompt}".into()]),
            timeout_secs: Some(90),
            ..Default::default()
        },
        ..Default::default()
    }
}

#[test]
fn finalize_cli_backend_preserves_dormant_provider_fields() {
    // backend_kind = cli is active, but the API-provider fields are kept
    // dormant on disk so switching back to the API Backend restores them
    // (ADR 0011) — switching never wipes the other Backend's config.
    let cfg = finalize(draft_with_cli(Some("claude")));
    // CLI Backend is active:
    assert_eq!(cfg.cli.command.as_deref(), Some("claude"));
    assert_eq!(
        cfg.cli.args.as_deref(),
        Some(&["-p".to_string(), "{prompt}".to_string()][..])
    );
    assert_eq!(cfg.cli.timeout_secs, Some(90));
    assert_eq!(cfg.backend_kind, Some(BackendKind::Cli));
    // API-provider fields preserved dormant (not dropped):
    assert_eq!(cfg.backend.as_deref(), Some("openai"));
    assert_eq!(cfg.api_key.as_deref(), Some("sk-stale"));
    assert_eq!(cfg.model.as_deref(), Some("gpt-5"));
    assert!(cfg.base_url.is_none());
    // confirm_before_commit is orthogonal and survives.
    assert_eq!(cfg.confirm_before_commit, Some(true));
    // Under the CLI backend the API memory bank is left untouched (the
    // active provider is not re-recorded) — a later switch back restores
    // whatever was already remembered.
    assert!(cfg.providers.is_empty());
}

#[test]
fn finalize_provider_backend_clears_cli_fields() {
    // No CLI command → provider path; any stale CLI fields are cleared.
    let cfg = finalize(draft_with_cli(None));
    assert_eq!(cfg.backend.as_deref(), Some("openai"));
    assert_eq!(cfg.api_key.as_deref(), Some("sk-stale"));
    assert!(cfg.cli.command.is_none());
    assert!(cfg.cli.args.is_none());
    assert!(cfg.cli.timeout_secs.is_none());
    assert!(cfg.backend_kind.is_none());
}

/// Regression for the list-vs-detail model mismatch: a provider that was
/// configured once (so it has a remembered model in the bank) but is not
/// currently active must preview its *remembered* model on the
/// `Choose your AI provider` list — the same value the detail sub-menu
/// shows after switching to it. Before the fix the list showed the bare
/// provider default, so picking the provider "changed" the model on entry.
#[test]
fn provider_choice_label_previews_banked_model_for_non_active() {
    let mut draft = draft_with_cli(None); // API backend, active = openai
    draft.provider = Some(Provider::OpenAI);
    draft.model = Some("gpt-5".into());
    draft.known_providers.push(ProviderProfile {
        backend: "anthropic".into(),
        model: Some("claude-sonnet-4-remembered".into()),
        ..Default::default()
    });
    let label = provider_choice_label(Provider::Anthropic, &draft);
    assert!(
        label.contains("claude-sonnet-4-remembered"),
        "list should preview the remembered model, got: {label}"
    );
}

/// `finalize` records the active API provider into the `providers` memory
/// bank, so a later switch away and back restores its key/model/base_url
/// without re-asking.
#[test]
fn finalize_records_active_provider_into_bank() {
    let mut draft = draft_with_cli(None); // API backend (no command)
    draft.provider = Some(Provider::OpenAI);
    draft.api_key = Some("sk-aaa".into());
    draft.model = Some("gpt-5".into());
    let cfg = finalize(draft);
    assert_eq!(cfg.providers.len(), 1);
    assert_eq!(cfg.providers[0].backend, "openai");
    assert_eq!(cfg.providers[0].api_key.as_deref(), Some("sk-aaa"));
    assert_eq!(cfg.providers[0].model.as_deref(), Some("gpt-5"));
}

/// `finalize` upserts: a pre-existing bank entry for the active provider
/// is updated in place rather than duplicated.
#[test]
fn finalize_upserts_active_provider_over_existing_bank_entry() {
    let mut draft = draft_with_cli(None);
    draft.provider = Some(Provider::OpenAI);
    draft.api_key = Some("sk-new".into());
    draft.known_providers.push(ProviderProfile {
        backend: "openai".into(),
        api_key: Some("sk-old".into()),
        model: Some("gpt-4o".into()),
        base_url: None,
    });
    draft.known_providers.push(ProviderProfile {
        backend: "anthropic".into(),
        api_key: Some("sk-ant".into()),
        model: None,
        base_url: None,
    });
    let cfg = finalize(draft);
    // One openai entry (updated), anthropic preserved — no duplicate.
    let openai = cfg
        .providers
        .iter()
        .filter(|p| p.backend == "openai")
        .count();
    assert_eq!(openai, 1);
    assert_eq!(cfg.providers[0].api_key.as_deref(), Some("sk-new"));
    assert_eq!(cfg.providers[0].model.as_deref(), Some("gpt-5"));
    assert_eq!(cfg.providers.len(), 2);
    assert!(cfg.providers.iter().any(|p| p.backend == "anthropic"));
}

/// `seed_draft` folds a pre-bank config's top-level fields into the bank
/// so the first save persists the active provider. Without this, a config
/// written before the bank existed would lose its active provider on the
/// next switch (the bank would be empty, so the restore would blank it).
#[test]
fn seed_draft_folds_legacy_top_level_into_bank() {
    let existing = Some(Config {
        backend: Some("openai".into()),
        api_key: Some("sk-x".into()),
        model: Some("gpt-5".into()),
        ..Default::default()
    });
    let draft = seed_draft(&existing);
    assert_eq!(draft.known_providers.len(), 1);
    assert_eq!(draft.known_providers[0].backend, "openai");
    assert_eq!(draft.known_providers[0].api_key.as_deref(), Some("sk-x"));
}

/// `seed_draft` does not duplicate the active provider when it is already
/// in the bank (e.g. a config written by a newer aic).
#[test]
fn seed_draft_does_not_duplicate_banked_active() {
    let existing = Some(Config {
        backend: Some("openai".into()),
        api_key: Some("sk-x".into()),
        model: Some("gpt-5".into()),
        providers: vec![ProviderProfile {
            backend: "openai".into(),
            api_key: Some("sk-x".into()),
            model: Some("gpt-5".into()),
            base_url: None,
        }],
        ..Default::default()
    });
    let draft = seed_draft(&existing);
    assert_eq!(draft.known_providers.len(), 1);
}

/// The provider-switch step banks the provider being left (its in-session
/// key/model) and restores the target's remembered fields — the
/// restore-on-switch contract `step_provider` depends on.
#[test]
fn switch_provider_banks_leaving_and_restores_target() {
    let mut draft = draft_with_cli(None); // active OpenAI, key sk-stale, model gpt-5
    draft.known_providers.push(ProviderProfile::new(
        "anthropic",
        Some("sk-ant".into()),
        Some("claude-x".into()),
        None,
    ));
    switch_provider(&mut draft, Provider::Anthropic);
    // OpenAI banked with the in-session key/model.
    let openai = draft
        .known_providers
        .iter()
        .find(|p| p.backend == "openai")
        .unwrap();
    assert_eq!(openai.api_key.as_deref(), Some("sk-stale"));
    assert_eq!(openai.model.as_deref(), Some("gpt-5"));
    // Anthropic restored into the active fields.
    assert_eq!(draft.api_key.as_deref(), Some("sk-ant"));
    assert_eq!(draft.model.as_deref(), Some("claude-x"));
}

/// A first-time choice (no current provider) has nothing to save, and a
/// target with no bank entry leaves the fields blank.
#[test]
fn switch_provider_first_choice_has_nothing_to_bank() {
    let mut draft = draft_with_cli(None);
    draft.provider = None;
    draft.known_providers.clear();
    switch_provider(&mut draft, Provider::OpenAI);
    assert!(draft.known_providers.is_empty());
    assert!(draft.api_key.is_none());
}

/// Merge contract on the switch path: a cleared in-session field must not
/// erase a value the bank already remembers (the blank-overwrite bug).
#[test]
fn switch_provider_merge_keeps_banked_key_when_field_blank() {
    let mut draft = draft_with_cli(None); // active OpenAI
    draft.api_key = None; // cleared in-session
    draft.known_providers.push(ProviderProfile::new(
        "openai",
        Some("sk-remembered".into()),
        Some("gpt-5".into()),
        None,
    ));
    draft.known_providers.push(ProviderProfile::new(
        "anthropic",
        Some("sk-a".into()),
        None,
        None,
    ));
    switch_provider(&mut draft, Provider::Anthropic);
    let openai = draft
        .known_providers
        .iter()
        .find(|p| p.backend == "openai")
        .unwrap();
    assert_eq!(openai.api_key.as_deref(), Some("sk-remembered"));
}

#[test]
fn cli_label_shows_command_or_not_configured() {
    assert_eq!(
        cli_label(&draft_with_cli(Some("claude"))),
        "claude -p {prompt}"
    );
    assert_eq!(cli_label(&draft_with_cli(None)), "(not configured)");
}

#[test]
fn cli_label_ignores_blank_command() {
    let mut d = draft_with_cli(Some("   "));
    d.cli.command = Some("   ".into());
    assert_eq!(
        cli_label(&d),
        "(not configured)",
        "whitespace-only command is treated as unset"
    );
}

#[test]
fn cli_menu_rows_have_no_unmappable_header() {
    // Regression: the CLI-agent menu once had a "Choose a preset:" *label*
    // as row 0 with no action — selecting it panicked the TUI. Now every
    // row is an actionable CliRow paired with its label, so there is no
    // separate index to leave unmapped. There is also no "Custom command…"
    // row: a free-form command has no decoder for its stdout envelope, so
    // it would silently run in plain-text mode — only the four presets
    // (each with a dedicated decoder) are offered.
    for command_set in [false, true] {
        let rows = cli_menu_rows(command_set);
        // Row 0 must be a real preset, never a bare label.
        assert!(
            matches!(rows.first(), Some(CliRow::Preset(_))),
            "row 0 must be an actionable preset, not a header (command_set={command_set})"
        );
        // Presets lead, then (only when set) Verify, then Done — every
        // expected variant present, nothing extra, and no Custom row.
        assert_eq!(rows.len(), PRESETS.len() + usize::from(command_set) + 1);
        assert!(!rows.contains(&CliRow::Verify) || command_set);
        assert_eq!(rows.contains(&CliRow::Verify), command_set);
        assert!(rows.contains(&CliRow::Done));
        // Done is always last.
        assert!(matches!(rows.last(), Some(CliRow::Done)));
    }
}
