//! Draft↔Config conversion: seeding the in-session draft from the existing
//! config, the pure provider-switch banking core, masking helpers, and the
//! final Config the wizard writes on save.

use super::*;

/// Seed the in-session draft from the existing config so untouched fields
/// survive `Save & exit` (a full-file write, see [`Config::save`]). A fresh
/// install leaves the draft empty — [`Self::finalize`] then falls back to the OpenAI
/// default, as before.
pub(super) fn seed_draft(existing: &Option<Config>) -> Draft {
    let mut draft = Draft::default();
    if let Some(c) = existing {
        draft.provider = c.backend.as_deref().map(Provider::from_name);
        draft.backend_kind = c.backend_kind;
        draft.api_key = c.api_key.clone();
        draft.base_url = c.base_url.clone();
        draft.model = c.model.clone();
        draft.confirm_before_commit = c.confirm_before_commit;
        draft.cli = c.cli.clone();
        draft.known_providers = c.providers.clone();
        // Pre-bank configs (written before the `providers` memory bank) carry
        // the active provider only as top-level fields; fold it into the bank
        // so the first save persists it and a later switch restores it. A
        // no-op when the active provider is already in the list.
        if let Some(name) = c.backend.as_deref()
            && !draft.known_providers.iter().any(|p| p.backend == name)
        {
            draft.known_providers.push(ProviderProfile {
                backend: name.to_string(),
                api_key: c.api_key.clone(),
                model: c.model.clone(),
                base_url: c.base_url.clone(),
            });
        }
    }
    draft
}
pub(super) fn finalize(draft: Draft) -> Config {
    // Both backends keep their configured fields, so switching the active
    // Backend never wipes what was entered for the other. `backend_kind`
    // selects which Backend a Run actually uses; the inactive one's fields
    // stay dormant on disk and are restored when you switch back (ADR 0011).
    let active = draft.active_backend();
    let command = draft.active_cli_command().map(str::to_owned);
    let has_cli = command.is_some();

    // `backend_kind` is written whenever it is non-default (CLI) or a dormant
    // CLI command is present, so the discriminator always disambiguates a
    // config that carries both backends' fields. For a pure API config (API
    // active, no command) it stays absent — byte-identical to before this
    // field existed, so released configs need no migration.
    let backend_kind = match active {
        BackendKind::Cli => Some(BackendKind::Cli),
        BackendKind::Api if has_cli => Some(BackendKind::Api),
        BackendKind::Api => None,
    };

    // When the API Backend is active, default a missing provider to OpenAI
    // (historical behavior). When it is dormant (CLI active), preserve the
    // draft's value verbatim so switching back restores it.
    let backend = match active {
        BackendKind::Api => Some(draft.provider.unwrap_or_default().name().to_string()),
        BackendKind::Cli => draft.provider.map(|p| p.name().to_string()),
    };

    // CLI fields are a unit (command + args + timeout + encoding); only
    // persist them when a command is set, so an unconfigured CLI leaves no
    // orphaned keys.
    let cli = if has_cli {
        CliConfig {
            command,
            args: draft.cli.args,
            timeout_secs: draft.cli.timeout_secs,
            encoding: draft.cli.encoding,
        }
    } else {
        CliConfig::default()
    };

    // Persist the memory bank: the active API provider's in-session fields
    // are upserted into the bank, then the bank is written as `providers`.
    // Under the CLI backend the API bank is left untouched (dormant), so a
    // later switch back restores every remembered provider.
    let mut providers = draft.known_providers;
    if active == BackendKind::Api
        && let Some(p) = draft.provider
    {
        ProviderProfile::upsert(
            &mut providers,
            ProviderProfile::new(
                p.name().to_string(),
                draft.api_key.clone(),
                draft.model.clone(),
                draft.base_url.clone(),
            ),
        );
    }

    Config {
        backend_kind,
        backend,
        api_key: draft.api_key,
        model: draft.model,
        base_url: draft.base_url,
        confirm_before_commit: draft.confirm_before_commit,
        cli,
        providers,
    }
}

/// Whether `Save & exit` would write a different config from what an
/// immediate save of the re-seeded disk state would produce — i.e. whether
/// this wizard session has unsaved changes (drives the Esc guard and the
/// Save-row marker). Compared as serialized TOML so cosmetic equality (field
/// order, omitted defaults) never trips the guard. The baseline is
/// `finalize(seed_draft(existing))`, not `existing` itself: seeding folds
/// migrations (e.g. pre-bank configs into `providers`), and a session that
/// changed nothing must not fire the guard for migration noise alone.
pub(super) fn draft_dirty(draft: &Draft, existing: &Option<Config>) -> bool {
    let cur = toml::to_string(&finalize(seed_draft(existing))).expect("config is valid TOML");
    let next = toml::to_string(&finalize(draft.clone())).expect("config is valid TOML");
    cur != next
}

/// Effective initial value for one field, in precedence order: the in-session
/// draft value first, then the existing-config value when the provider is
/// unchanged, else none. `field` selects which `Config` column to read, so the
/// one shared body replaces the old per-field `key/base_url/model_initial`
/// triple that were byte-for-byte apart from the field they touched.
pub(super) fn field_initial(
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

/// Pure core of the provider-switch step: bank the provider being left (its
/// in-session key/model/base_url, merged so a blank never erases a remembered
/// value), then restore the target's remembered fields into the draft. Split
/// from `step_provider` (which does the interactive pick) so the
/// restore-on-switch contract is unit-testable.
pub(super) fn switch_provider(draft: &mut Draft, chosen: Provider) {
    if draft.provider == Some(chosen) {
        return;
    }
    // Remember the provider we're leaving before restoring the target, so a
    // round-trip back to it brings up the key/model/base_url again instead of
    // blanks. A first-time choice (current is None) has nothing to save.
    if let Some(current) = draft.provider {
        ProviderProfile::bank_active(
            &mut draft.known_providers,
            ProviderProfile::new(
                current.name().to_string(),
                draft.api_key.clone(),
                draft.model.clone(),
                draft.base_url.clone(),
            ),
        );
    }
    // Restore the target's remembered fields; None where it was never banked.
    let restored = draft
        .known_providers
        .iter()
        .find(|p| p.backend == chosen.name())
        .cloned();
    (draft.api_key, draft.model, draft.base_url) =
        restored.map(|p| p.project_fields()).unwrap_or_default();
}
/// Mask a secret for the prompt hint and pre-save summary. Never reveals the
/// full value.
pub(super) fn mask_key(k: &str) -> String {
    if k.is_empty() {
        return "(empty)".to_string();
    }
    "•".repeat(k.len().min(12))
}
