//! The AI-provider sub-flow's pure logic: which steps apply to a provider
//! (ADR 0019), the sub-menu row labels, the model-list cache key, and the
//! model picker rows. Split from the wizard core so the screen state
//! machine (in [`super::app`]) and the provider vocabulary read
//! independently. No IO lives here.

use super::finalize::mask_key;
use super::*;
use crate::core::config::Source;
use crate::llm::BaseUrlRequirement;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    Provider,
    ApiKey,
    BaseUrl,
    Model,
}
pub(super) fn key_applies(p: Provider) -> bool {
    p.requires_key() || p == Provider::OpenAiCompatible
}

pub(super) fn base_url_applies(p: Provider) -> bool {
    !matches!(p.base_url_requirement(), BaseUrlRequirement::None)
}

/// Ordered provider-scoped steps that actually apply to `p`. Steps that would
/// be a no-op — an API key for local Ollama, a base URL for a cloud provider —
/// are absent, so forward/back never lands on one. `Provider` always starts
/// the list and `Model` always ends it. `ConfirmCommit` is a top-level menu
/// entry, not part of the provider path, so it never appears here.
pub(super) fn applicable_steps(p: Provider) -> Vec<Step> {
    let mut steps = vec![Step::Provider];
    if key_applies(p) {
        steps.push(Step::ApiKey);
    }
    if base_url_applies(p) {
        steps.push(Step::BaseUrl);
    }
    steps.push(Step::Model);
    steps
}

/// One provider's preview model for the `Choose your AI provider` list: the
/// active draft choice first, then the remembered bank entry. There is no
/// default model (ADR 0019) — a provider with neither reads `(model not
/// set)`. The detail sub-menu reads the same bank (via [`switch_provider`]'s
/// restore-on-switch into `draft.model`), so the list and the detail never
/// disagree on which model a provider will use.
pub(super) fn preview_model(p: Provider, draft: &Draft) -> String {
    if draft.provider == Some(p) {
        return draft.effective_model();
    }
    draft
        .known_providers
        .iter()
        .find(|kp| kp.backend == p.name())
        .and_then(|kp| {
            kp.model
                .as_deref()
                .filter(|m| !m.is_empty())
                .map(String::from)
        })
        .unwrap_or_default()
}

/// A configurable field inside the AI-provider sub-menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderEntry {
    ApiKey,
    BaseUrl,
    Model,
    Done,
}

/// `API key` sub-menu row: the masked effective key (or "(not set)"). An API
/// key has no provider default, so — unlike [`base_url_label`] /
/// [`model_label`] — it takes no [`Source`].
pub(super) fn api_key_label(key: &str) -> String {
    if key.is_empty() {
        return "(not set)".to_string();
    }
    mask_key(key)
}

/// `Base URL` sub-menu row: the effective URL (config > provider default),
/// annotated with its source.
pub(super) fn base_url_label(url: Option<&str>, source: Source) -> String {
    match (url, source) {
        (Some(u), Source::Default) => format!("{u} (default)"),
        (Some(u), _) => u.to_string(),
        (None, _) => "(not set)".to_string(),
    }
}

/// `Model` sub-menu row: the chosen model, or `(not set)` — there is no
/// default model to fall back to (ADR 0019).
pub(super) fn model_label(model: &str) -> String {
    if model.is_empty() {
        "(not set)".to_string()
    } else {
        model.to_string()
    }
}

/// Session model-cache key (ADR 0019 §5): provider + resolved base URL +
/// masked key — any change re-fetches.
pub(super) fn cache_key(provider: Provider, base_url: Option<&str>, api_key: &str) -> String {
    format!(
        "{}|{}|{}",
        provider.name(),
        base_url.unwrap_or(""),
        mask_key(api_key)
    )
}

/// One row of the Model picker (ADR 0019 §6): a listed model, or the
/// `Custom model…` escape hatch. The tag travels with the row so the pick
/// handler dispatches on what the row *is*, never on its position.
#[derive(Clone, PartialEq, Debug)]
pub(super) enum ModelRow {
    Model(String),
    Custom,
}

impl ModelRow {
    /// The row's display label (the `·` bullet lives in the label, matching
    /// every other menu row).
    pub(super) fn label(&self) -> String {
        match self {
            ModelRow::Model(m) => format!("{DOT} {m}"),
            ModelRow::Custom => format!("{ICON_PENCIL}  Custom model…"),
        }
    }
}

/// Picker rows over a fetched list (ADR 0019 §6): already sorted by the
/// fetcher, capped at the first 50 — no search, no pagination — plus the
/// `Custom model…` escape hatch. Pure.
pub(super) fn picker_rows(models: &[String]) -> Vec<ModelRow> {
    // ponytail: first-50 truncation; add search/pagination if providers
    // start shipping lists where 50 isn't enough to find anything.
    let mut items: Vec<ModelRow> = models
        .iter()
        .take(50)
        .map(|m| ModelRow::Model(m.clone()))
        .collect();
    items.push(ModelRow::Custom);
    items
}
