//! The AI-provider sub-flow of the setup wizard: choosing the provider,
//! editing its fields (key / base URL / model), and the sub-menu that
//! surfaces them. Split from the wizard core so the menu state machine
//! and the provider path read independently.

use super::finalize::{field_initial, mask_key, switch_provider};
use super::verify::step_verify;
use super::*;
use crate::core::config::{Source, resolve_api_key, resolve_base_url, resolve_field};
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
/// entry, not part of the provider path, so it never appears here. Navigation
/// is just index ±1 off this list (see [`run_provider_flow`])
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
/// active draft choice first, then the remembered bank entry, then the
/// provider default. The detail sub-menu reads the same bank (via
/// [`step_provider`]'s restore-on-switch into `draft.model`), so the list and
/// the detail never disagree on which model a provider will use.
fn preview_model(p: Provider, draft: &Draft) -> String {
    if draft.provider == Some(p) {
        return draft.effective_model(p);
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
        .unwrap_or_else(|| p.default_model().to_string())
}

/// One row in the provider picker (the screen *before* this provider's
/// submenu). For the currently selected provider, show the model the user
/// actually chose (`draft.model`, seeded from the existing config) rather than
/// the bare default — otherwise re-entering setup makes the selection read as
/// lost (AIC-15). For every other provider, show its default so the options
/// stay comparable at a glance.
pub(super) fn provider_choice_label(p: Provider, draft: &Draft) -> String {
    let model = preview_model(p, draft);
    if model.is_empty() {
        format!("{}  (no default — you'll pick a model)", p.display())
    } else {
        format!("{}  ({model})", p.display())
    }
}
/// A configurable field inside the AI-provider sub-menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProviderEntry {
    ApiKey,
    BaseUrl,
    Model,
    Verify,
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

/// `Model` sub-menu row: the effective model (config > provider default),
/// annotated with its source.
pub(super) fn model_label(model: &str, source: Source) -> String {
    if model.is_empty() {
        return "(not set)".to_string();
    }
    match source {
        Source::Default => format!("{model} (default)"),
        _ => model.to_string(),
    }
}

/// The AI-provider sub-menu: one entry per applicable field (based on the
/// current provider) plus `Done`, each with its current value inline. The
/// provider itself is chosen on the screen before this menu.
pub(super) fn provider_submenu_items(draft: &Draft) -> (Vec<ProviderEntry>, Vec<String>) {
    let p = draft.provider.unwrap_or_default();
    let mut entries = Vec::new();
    let mut labels = Vec::new();
    for step in applicable_steps(p) {
        match step {
            Step::Provider => {} // chosen before this menu
            Step::ApiKey => {
                // Effective key for display: the draft (a user edit or the
                // seeded config value).
                let (key, _) = resolve_api_key(draft.api_key.as_deref().filter(|k| !k.is_empty()));
                entries.push(ProviderEntry::ApiKey);
                labels.push(format!("{ICON_API_KEY} API key — {}", api_key_label(&key)));
            }
            Step::BaseUrl => {
                let (url, source) =
                    resolve_base_url(draft.base_url.as_deref().filter(|u| !u.is_empty()), &p);
                entries.push(ProviderEntry::BaseUrl);
                labels.push(format!(
                    "{ICON_BASE_URL} Base URL — {}",
                    base_url_label(url.as_deref(), source)
                ));
            }
            Step::Model => {
                let (model, source) = resolve_field(
                    draft.model.as_deref().filter(|m| !m.is_empty()),
                    p.default_model(),
                );
                entries.push(ProviderEntry::Model);
                labels.push(format!(
                    "{ICON_MODEL} Model — {}",
                    model_label(&model, source)
                ));
            }
        }
    }
    entries.push(ProviderEntry::Verify);
    labels.push(format!(
        "{ICON_VERIFY} Verify — test this provider with a sample request"
    ));
    entries.push(ProviderEntry::Done);
    labels.push(format!("{ICON_DONE} Done — back to main menu"));
    (entries, labels)
}
/// Walk the provider-scoped path (Provider → key → base URL → model) until it
/// completes or the user backs out of its first step. Returns `true` when a
/// Ctrl-C cancels the whole setup; `false` means "return to the menu".
/// Entry into the AI-provider screen. First the provider is chosen (which
/// fields apply depends on it), then a sub-menu offers each applicable field —
/// API key, base URL, model — as a separate entry plus `Done`. Nothing is
/// forced: Esc on the sub-menu returns to the provider choice, Esc on the
/// provider choice returns to the main menu. Returns `true` on Ctrl-C (cancel
/// the whole setup); `false` returns to the main menu.
pub(super) fn run_provider_flow(
    existing: &Option<Config>,
    existing_provider: Option<Provider>,
    draft: &mut Draft,
) -> Result<bool> {
    loop {
        // Screen 1: choose the provider. Switching clears key/url/model
        // (step_provider), which changes which sub-menu entries apply.
        match step_provider(existing_provider, draft)? {
            Nav::Next => {}
            Nav::Back => return Ok(false), // Esc on the provider choice -> main menu
            Nav::Cancel => return Ok(true),
        }
        // Screen 2: configure the provider's fields independently.
        loop {
            show_screen()?;
            let (entries, labels) = provider_submenu_items(draft);
            match opt_nav("Configure AI provider", &labels, 0)? {
                OptNav::Value(i) => {
                    let nav = match entries[i] {
                        ProviderEntry::ApiKey => step_api_key(draft)?,
                        ProviderEntry::BaseUrl => {
                            step_base_url(existing, existing_provider, draft)?
                        }
                        ProviderEntry::Model => step_model(existing, existing_provider, draft)?,
                        ProviderEntry::Verify => step_verify(draft)?,
                        ProviderEntry::Done => return Ok(false),
                    };
                    // Any non-cancel outcome returns to the sub-menu; the field
                    // steps manage their own Esc-back-to-options internally.
                    match nav {
                        Nav::Next | Nav::Back => {}
                        Nav::Cancel => return Ok(true),
                    }
                }
                OptNav::Back => break, // Esc on the sub-menu -> re-choose provider
                OptNav::Cancel => return Ok(true),
            }
        }
    }
}
fn step_provider(existing_provider: Option<Provider>, draft: &mut Draft) -> Result<Nav> {
    show_screen()?;
    let providers = Provider::all();
    let items: Vec<String> = providers
        .iter()
        .map(|&p| provider_choice_label(p, draft))
        .collect();
    let default_idx = draft
        .provider
        .or(existing_provider)
        .and_then(|ep| providers.iter().position(|p| *p == ep))
        .unwrap_or(0);

    match opt_nav("Choose your AI provider", &items, default_idx)? {
        OptNav::Value(i) => {
            let chosen = providers[i];
            switch_provider(draft, chosen);
            draft.provider = Some(chosen);
            Ok(Nav::Next)
        }
        OptNav::Back => Ok(Nav::Back),
        OptNav::Cancel => Ok(Nav::Cancel),
    }
}

fn step_api_key(draft: &mut Draft) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before api key");
    // Effective key for editing + keep semantics: the draft (a user edit or
    // the seeded config value).
    let (key, _) = resolve_api_key(draft.api_key.as_deref().filter(|k| !k.is_empty()));
    match provider.requires_key() {
        // Cloud provider — key required. A single visible, editable prompt:
        // type a new key (shown in the clear) or leave it blank to keep the
        // current one. No masked input and no keep/replace menu, so the user
        // always sees what they enter — the masked Password left no visible
        // field and gave no feedback after pasting.
        true => {
            let prompt = if key.is_empty() {
                "API key".to_string()
            } else {
                format!(
                    "API key (current: {} — leave blank to keep)",
                    mask_key(&key)
                )
            };
            // Blank is accepted only when there is a current key to keep;
            // otherwise the validator enforces a non-empty entry.
            let allow_empty = !key.is_empty();
            show_screen()?;
            match prompt_text(&prompt, None, allow_empty, "API key cannot be empty")? {
                TextAct::Value(v) => {
                    let v = if v.is_empty() { key } else { v };
                    draft.api_key = Some(v);
                    Ok(Nav::Next)
                }
                TextAct::Back => Ok(Nav::Back),
                TextAct::Cancel => Ok(Nav::Cancel),
            }
        }
        // OpenAI-compatible — key optional (keyless servers allowed). "No key"
        // is a first-class choice a single text field cannot express, so keep
        // it as a menu option; but enter the key in the clear (not masked) so
        // the user sees what they type.
        false if provider == Provider::OpenAiCompatible => {
            let has_key = !key.is_empty();
            let items: Vec<String> = if has_key {
                vec![
                    format!("Keep current key ({})", mask_key(&key)),
                    "Enter a new key…".to_string(),
                    "No API key (keyless server)".to_string(),
                ]
            } else {
                vec![
                    "No API key (keyless server)".to_string(),
                    "Enter API key…".to_string(),
                ]
            };
            let no_key_idx = if has_key { 2 } else { 0 };
            let enter_idx = 1;
            // Row 0 is "Keep current key" only when a key is already set; with
            // no key, row 0 is the "No API key" option instead.
            let keep_idx = if has_key { Some(0) } else { None };
            loop {
                show_screen()?;
                match opt_nav("API key", &items, 0)? {
                    OptNav::Value(i) if keep_idx == Some(i) => {
                        // Keep current key — draft.api_key already holds it.
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == no_key_idx => {
                        draft.api_key = None;
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == enter_idx => {
                        show_screen()?;
                        match prompt_text("API key (blank = keyless server)", None, true, "")? {
                            TextAct::Value(v) => {
                                draft.api_key = if v.is_empty() { None } else { Some(v) };
                                return Ok(Nav::Next);
                            }
                            TextAct::Back => continue,
                            TextAct::Cancel => return Ok(Nav::Cancel),
                        }
                    }
                    OptNav::Value(_) => unreachable!("api key options"),
                    OptNav::Back => return Ok(Nav::Back),
                    OptNav::Cancel => return Ok(Nav::Cancel),
                }
            }
        }
        // Ollama — unreachable (step skipped via key_applies); defensive.
        false => {
            draft.api_key = None;
            Ok(Nav::Next)
        }
    }
}

fn step_base_url(
    existing: &Option<Config>,
    ep: Option<Provider>,
    draft: &mut Draft,
) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before base url");
    let initial = field_initial(
        draft.base_url.as_deref(),
        existing,
        ep,
        draft.provider,
        |c| c.base_url.as_ref(),
    );
    match provider.base_url_requirement() {
        BaseUrlRequirement::Required => {
            show_screen()?;
            match prompt_text(
                "Base URL (e.g. http://localhost:1234/v1)",
                initial.as_deref(),
                false,
                "base URL cannot be empty",
            )? {
                TextAct::Value(v) => {
                    draft.base_url = Some(v);
                    Ok(Nav::Next)
                }
                TextAct::Back => Ok(Nav::Back),
                TextAct::Cancel => Ok(Nav::Cancel),
            }
        }
        BaseUrlRequirement::Optional(default) => {
            let (effective, source) = resolve_base_url(
                draft.base_url.as_deref().filter(|u| !u.is_empty()),
                &provider,
            );
            let has_url = matches!(source, Source::Config);
            let current = effective.as_deref().unwrap_or(default);
            let items: Vec<String> = if has_url {
                vec![
                    format!("Keep current URL ({current})"),
                    format!("Use default ({default})"),
                    "Enter custom URL…".to_string(),
                ]
            } else {
                vec![
                    format!("Use default ({default})"),
                    "Enter custom URL…".to_string(),
                ]
            };
            let use_default_idx = if has_url { 1 } else { 0 };
            let custom_idx = if has_url { 2 } else { 1 };
            // Row 0 is "Keep current URL" only when a URL is already set; with
            // none, row 0 is the "Use default" option instead.
            let keep_idx = if has_url { Some(0) } else { None };
            loop {
                show_screen()?;
                match opt_nav("Base URL", &items, 0)? {
                    OptNav::Value(i) if keep_idx == Some(i) => {
                        // Keep current URL — draft.base_url already holds it.
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == use_default_idx => {
                        draft.base_url = None;
                        return Ok(Nav::Next);
                    }
                    OptNav::Value(i) if i == custom_idx => {
                        show_screen()?;
                        match prompt_text(
                            &format!("Custom base URL (e.g. {default})"),
                            None,
                            false,
                            "base URL cannot be empty",
                        )? {
                            TextAct::Value(v) => {
                                draft.base_url = Some(v);
                                return Ok(Nav::Next);
                            }
                            TextAct::Back => continue,
                            TextAct::Cancel => return Ok(Nav::Cancel),
                        }
                    }
                    OptNav::Value(_) => unreachable!("base url options"),
                    OptNav::Back => return Ok(Nav::Back),
                    OptNav::Cancel => return Ok(Nav::Cancel),
                }
            }
        }
        // Unreachable (step skipped via base_url_applies); defensive.
        BaseUrlRequirement::None => Ok(Nav::Next),
    }
}

fn step_model(existing: &Option<Config>, ep: Option<Provider>, draft: &mut Draft) -> Result<Nav> {
    let provider = draft.provider.expect("provider chosen before model");
    let default_model = provider.default_model();
    let models = provider.models();
    let initial = field_initial(draft.model.as_deref(), existing, ep, draft.provider, |c| {
        c.model.as_ref()
    });

    // No curated list (OpenRouter, OpenAI-compatible) -> required free text.
    if models.is_empty() {
        show_screen()?;
        return match prompt_text(
            "Model (required)",
            initial.as_deref(),
            false,
            "model cannot be empty",
        )? {
            TextAct::Value(v) => {
                draft.model = Some(v);
                Ok(Nav::Next)
            }
            TextAct::Back => Ok(Nav::Back),
            TextAct::Cancel => Ok(Nav::Cancel),
        };
    }

    let mut items: Vec<String> = models.iter().map(|m| (*m).to_string()).collect();
    items.push("✏️  Custom model…".to_string());
    let custom_idx = items.len() - 1;
    let highlight = initial
        .as_deref()
        .and_then(|v| models.iter().position(|m| *m == v))
        .unwrap_or_else(|| {
            if default_model.is_empty() {
                0
            } else {
                models.iter().position(|m| *m == default_model).unwrap_or(0)
            }
        });

    loop {
        show_screen()?;
        match opt_nav("Model", &items, highlight)? {
            OptNav::Value(i) if i == custom_idx => {
                show_screen()?;
                match prompt_text("Custom model", None, false, "model cannot be empty")? {
                    TextAct::Value(v) => {
                        draft.model = Some(v);
                        return Ok(Nav::Next);
                    }
                    // "Custom model…" is a sub-mode of the Model step, not a
                    // separate step: Esc cancels the custom entry and returns to
                    // the model picker (Esc there then leaves the step), so the
                    // "Esc goes back on every step" invariant holds at step
                    // granularity.
                    TextAct::Back => continue,
                    TextAct::Cancel => return Ok(Nav::Cancel),
                }
            }
            OptNav::Value(i) => {
                let m = models[i];
                draft.model = if !default_model.is_empty() && m == default_model {
                    None
                } else {
                    Some(m.to_string())
                };
                return Ok(Nav::Next);
            }
            OptNav::Back => return Ok(Nav::Back),
            OptNav::Cancel => return Ok(Nav::Cancel),
        }
    }
}
