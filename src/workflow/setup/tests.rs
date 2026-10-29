use super::cli_flow::*;
use super::finalize::*;
use super::provider::*;
use super::*;
use crate::core::config::Source;
use crate::llm::cli_agent::PRESETS;

use crate::core::config::test_util::cfg;

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
        ..Default::default()
    }
}

/// [`draft_dirty`] is the Esc guard's truth: a session that changed nothing
/// is not dirty (against a fresh install OR against an existing config,
/// including migration noise folded into seeding), and any entered field is.
#[test]
fn draft_dirty_tracks_unsaved_changes() {
    // Fresh install, nothing entered: an immediate save would write the same
    // config the empty draft finalizes to.
    assert!(!draft_dirty(&Draft::default(), &None));

    // Any entered field makes Save write something new.
    assert!(draft_dirty(&draft(None, Some("k"), None, None), &None));

    // Re-opening the wizard and changing nothing is not dirty — the baseline
    // is the re-seeded-and-finalized config, not the raw disk bytes, so
    // migrations folded into seeding never fire the guard.
    let existing = finalize(Draft::default());
    assert!(!draft_dirty(
        &seed_draft(&Some(existing.clone())),
        &Some(existing)
    ));
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
        confirm_before_commit: Some(true),
        ..Default::default()
    });
    let d = seed_draft(&existing);
    assert_eq!(d.confirm_before_commit, Some(true));
    assert_eq!(d.provider, None);
}

#[test]
fn seed_draft_carries_review_batch_plan() {
    let existing = Some(Config {
        review_batch_plan: Some(true),
        ..Default::default()
    });
    let d = seed_draft(&existing);
    assert_eq!(d.review_batch_plan, Some(true));
}

#[test]
fn provider_label_defaults_to_openai_for_api_backend() {
    // API backend (the default) with no provider chosen shows the OpenAI
    // default provider, mirroring `finalize` — never "(not set)" while API
    // is active. No model is chosen, and there is no default model
    // (ADR 0019), so the row carries the provider alone.
    let d = draft(None, None, None, None);
    assert_eq!(provider_label(&d), "OpenAI");

    // Explicit API backend, still no provider -> same default.
    let mut d = draft(None, None, None, None);
    d.backend_kind = Some(BackendKind::Api);
    assert_eq!(provider_label(&d), "OpenAI");

    // CLI backend ignores the provider -> "(not set)" until one is set.
    let mut d = draft(None, None, None, None);
    d.backend_kind = Some(BackendKind::Cli);
    assert_eq!(provider_label(&d), "(not set)");
}

#[test]
fn provider_label_shows_provider_and_model() {
    // Explicit model rides along.
    let d = draft(Some(Provider::OpenAI), None, Some("gpt-5"), None);
    assert_eq!(provider_label(&d), "OpenAI · gpt-5");
    // No model chosen -> provider name only (no default to show).
    let d = draft(Some(Provider::OpenRouter), None, None, None);
    assert_eq!(provider_label(&d), "OpenRouter");
}

#[test]
fn submenu_labels_reflect_value_and_source() {
    // API key: masked, (not set) when empty.
    assert_eq!(api_key_label("sk-123"), "••••••");
    assert_eq!(api_key_label(""), "(not set)");

    // Model: value, (not set) when empty — no source annotation exists
    // (ADR 0019: no default model).
    assert_eq!(model_label("gpt-5"), "gpt-5");
    assert_eq!(model_label(""), "(not set)");

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
fn effective_bundle_carries_the_draft_choice_only() {
    // Draft values are the whole story (ADR 0019: no provider default).
    let d = draft(Some(Provider::OpenAI), Some("sk-1"), Some("gpt-5"), None);
    let eff = d.effective();
    assert_eq!(eff.provider, Provider::OpenAI);
    assert_eq!(eff.api_key, "sk-1");
    assert_eq!(eff.model, "gpt-5");
    // No model chosen -> empty, whatever the provider.
    assert_eq!(
        draft(Some(Provider::OpenAI), None, None, None)
            .effective()
            .model,
        ""
    );
    assert_eq!(
        draft(Some(Provider::OpenRouter), None, None, None)
            .effective()
            .model,
        ""
    );
}

#[test]
fn preview_model_shows_chosen_model_for_selected_provider() {
    // The selected provider shows the user's chosen model — re-entering
    // setup must not read as if the choice was lost (AIC-15).
    let mut d = draft(Some(Provider::DeepSeek), None, None, None);
    d.model = Some("deepseek-v4-pro".into());
    assert_eq!(preview_model(Provider::DeepSeek, &d), "deepseek-v4-pro");

    // Another provider shows its remembered bank entry, never a default.
    let mut d = draft(Some(Provider::DeepSeek), None, None, None);
    d.model = Some("deepseek-v4-pro".into());
    d.known_providers.push(ProviderProfile {
        backend: "openai".into(),
        api_key: None,
        model: Some("gpt-5".into()),
        base_url: None,
    });
    assert_eq!(preview_model(Provider::OpenAI, &d), "gpt-5");

    // No chosen model anywhere -> empty (rendered as "(model not set)").
    let d = draft(Some(Provider::DeepSeek), None, None, None);
    assert_eq!(preview_model(Provider::DeepSeek, &d), "");
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
fn finalize_carries_review_batch_plan() {
    // Untouched (None) when the user never visited the step — config stays
    // absent, so the runtime default remains off.
    let out = finalize(draft(Some(Provider::OpenAI), None, None, None));
    assert_eq!(out.review_batch_plan, None);

    // Explicit choice round-trips into the written config.
    let mut d = draft(Some(Provider::OpenAI), None, None, None);
    d.review_batch_plan = Some(true);
    let out = finalize(d);
    assert_eq!(out.review_batch_plan, Some(true));
}

fn draft_with_cli(command: Option<&str>) -> Draft {
    Draft {
        backend_kind: command.map(|_| BackendKind::Cli),
        provider: Some(Provider::OpenAI),
        api_key: Some("sk-stale".into()),
        model: Some("gpt-5".into()),
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
    let preview = preview_model(Provider::Anthropic, &draft);
    assert!(
        preview.contains("claude-sonnet-4-remembered"),
        "list should preview the remembered model, got: {preview}"
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
fn cli_label_shows_command_name_or_not_configured() {
    // Name only — args live in the picker rows and the config, not the
    // selected display (owner call: the main-menu row reads cleaner).
    assert_eq!(cli_label(&draft_with_cli(Some("claude"))), "claude");
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
    // separate index to leave unmapped. There is no "Custom command…" row
    // (a free-form command has no decoder) and no manual Verify row (R3 —
    // the probe runs automatically when the flow completes).
    let rows = cli_menu_rows();
    // Row 0 must be a real preset, never a bare label.
    assert!(matches!(rows.first(), Some(CliRow::Preset(_))));
    // Presets lead, then Done — nothing else, regardless of whether a
    // command is already set (Done just re-probes when one is, R3).
    assert_eq!(rows.len(), PRESETS.len() + 1);
    assert!(matches!(rows.last(), Some(CliRow::Done)));
}

/// Picker rows (ADR 0019 §6): 50-entry cap plus the `Custom model…` escape
/// hatch — no search, no pagination.
#[test]
fn picker_rows_cap_at_fifty_and_append_custom() {
    let models: Vec<String> = (0..80).map(|i| format!("model-{i:02}")).collect();
    let rows = picker_rows(&models);
    assert_eq!(rows.len(), 51);
    assert_eq!(rows[0], ModelRow::Model("model-00".into()));
    assert_eq!(rows[49], ModelRow::Model("model-49".into()));
    assert_eq!(rows[50], ModelRow::Custom);
    // Labels keep the ADR 0018 marker vocabulary.
    assert_eq!(rows[0].label(), format!("{DOT} model-00"));
    assert_eq!(rows[50].label(), format!("{ICON_PENCIL}  Custom model…"));
    // A short list passes through untouched.
    let rows = picker_rows(&["a".to_string(), "b".to_string()]);
    assert_eq!(rows.len(), 3);
}

/// The cache key (ADR 0019 §5) folds provider, base URL, and masked key:
/// any one changing must produce a different key. Equal-length keys mask
/// identically (`••••••`) — that collision is the accepted ceiling of a
/// masked key (never holds raw key material), noted at `cache_key`.
#[test]
fn cache_key_separates_provider_base_and_key() {
    let a = cache_key(Provider::OpenAI, None, "sk-123456789");
    assert_ne!(a, cache_key(Provider::Mistral, None, "sk-123456789"));
    assert_ne!(
        a,
        cache_key(Provider::OpenAI, Some("http://h:1"), "sk-123456789")
    );
    // Different key *length* changes the mask, hence the key.
    assert_ne!(a, cache_key(Provider::OpenAI, None, "sk-12345678"));
    assert_eq!(a, cache_key(Provider::OpenAI, None, "sk-123456789"));
}

// --- Auto-verify on Done (R3) ---------------------------------------------

/// A provider probe against an incomplete draft fails validation — the
/// Run-path setup hint, not an opaque provider error — without touching the
/// network: `validate` rejects the draft before any request is built. This
/// is the Err that drives the failure screen when Done is reached with a
/// missing key or model.
#[test]
fn provider_probe_reports_validation_hint_without_network() {
    // Key-requiring provider with neither key nor model set.
    let d = draft(Some(Provider::OpenAI), None, None, None);
    let err = super::verify::probe_provider(&d.effective())
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("aic setup"),
        "validation error must point at setup, got: {err}"
    );
}

/// A CLI probe with no configured command errs instead of spawning
/// anything (the defensive guard; callers only auto-verify once a command
/// is set).
#[test]
fn cli_probe_without_command_errs_cleanly() {
    let d = draft(None, None, None, None);
    assert!(super::verify::probe_cli(&d).is_err());
}
