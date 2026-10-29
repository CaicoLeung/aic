use super::*;
use crate::llm::cli_agent::{CliSpec, Encoding};

fn cli(command: &str, encoding: Encoding) -> LlmConfig {
    LlmConfig::Cli(CliSpec {
        command: command.into(),
        args: vec!["-p".into(), "{prompt}".into()],
        timeout_secs: 10,
        encoding,
    })
}

#[test]
fn cold_start_program_behind_the_backend_seam() {
    // The loading frame crosses ONE seam: cold_start_program composes the
    // envelope's streams_reasoning_live policy with the CLI command name.
    // A live-token-streaming CLI (claude/pi) labels the cold-start notice;
    // anything else (whole-at-completion codex/opencode, plain, or the
    // API/rig path) returns None → the silent notice.
    let claude = cli("claude", Encoding::ClaudeStreamJson);
    assert_eq!(claude.cold_start_program().as_deref(), Some("claude"));

    let pi = cli("pi", Encoding::PiStreamJson);
    assert_eq!(pi.cold_start_program().as_deref(), Some("pi"));

    // codex reasons whole-at-completion (no live stream) → None.
    let codex = LlmConfig::Cli(CliSpec {
        command: "codex".into(),
        args: vec!["exec".into(), "{prompt}".into()],
        timeout_secs: 10,
        encoding: Encoding::CodexJson,
    });
    assert_eq!(codex.cold_start_program(), None);

    // The API/rig path never cold-starts a reasoning feed → None.
    let rig = LlmConfig::Rig(LLM::new(
        Provider::OpenAI,
        String::new(),
        String::new(),
        None,
    ));
    assert_eq!(rig.cold_start_program(), None);
}

#[test]
fn all_providers_have_a_registry_row() {
    // meta() panics if a variant is missing from REGISTRY.
    for provider in ALL_PROVIDERS {
        assert!(!provider.name().is_empty());
        assert!(!provider.display().is_empty());
        let _ = provider.base_url_requirement();
    }
}

#[test]
fn registry_order_matches_all_providers() {
    assert_eq!(REGISTRY.len(), ALL_PROVIDERS.len());
    for (m, p) in REGISTRY.iter().zip(ALL_PROVIDERS.iter()) {
        assert_eq!(m.provider, *p);
    }
}

#[test]
fn from_name_resolves_canonical_names_and_aliases() {
    assert_eq!(Provider::from_name("openai"), Provider::OpenAI);
    assert_eq!(Provider::from_name("Anthropic"), Provider::Anthropic);
    assert_eq!(Provider::from_name("claude"), Provider::Anthropic);
    assert_eq!(Provider::from_name("google"), Provider::Gemini);
    assert_eq!(Provider::from_name("grok"), Provider::Xai);
    assert_eq!(Provider::from_name("together-ai"), Provider::Together);
    assert_eq!(Provider::from_name("custom"), Provider::OpenAiCompatible);
    assert_eq!(
        Provider::from_name("openai-compatible"),
        Provider::OpenAiCompatible
    );
}

#[test]
fn from_name_unknown_falls_back_to_openai() {
    assert_eq!(Provider::from_name("nope"), Provider::OpenAI);
}

#[test]
fn is_known_name_recognizes_canonical_names_and_aliases() {
    assert!(Provider::is_known_name("openai"));
    assert!(Provider::is_known_name("Anthropic"));
    assert!(Provider::is_known_name("claude"));
    assert!(Provider::is_known_name("openai-compatible"));
    assert!(Provider::is_known_name("custom"));
    // The typo the strict check exists to catch.
    assert!(!Provider::is_known_name("anthpopic"));
    assert!(!Provider::is_known_name("nope"));
    assert!(!Provider::is_known_name(""));
}

#[test]
fn name_round_trips_for_every_provider() {
    for provider in ALL_PROVIDERS {
        assert_eq!(Provider::from_name(provider.name()), *provider);
    }
}

/// AIC-12: the five Phase-1 providers (xAI, Mistral, OpenRouter,
/// Perplexity, Together) plus the OpenAI-compatible escape hatch must each
/// resolve a canonical name, an API-key requirement where the provider
/// needs one, and a correct base-URL requirement. Model data is never
/// hardcoded — the wizard fetches it live (ADR 0019).
#[test]
fn new_provider_metadata_is_complete() {
    // Canonical backend values (README tables mirror these).
    assert_eq!(Provider::Xai.name(), "xai");
    assert_eq!(Provider::Mistral.name(), "mistral");
    assert_eq!(Provider::OpenRouter.name(), "openrouter");
    assert_eq!(Provider::Perplexity.name(), "perplexity");
    assert_eq!(Provider::Together.name(), "together");
    assert_eq!(Provider::OpenAiCompatible.name(), "openai-compatible");

    // API keys: cloud providers require one; Ollama and the openai-compatible
    // escape hatch do not.
    assert!(Provider::OpenAI.requires_key());
    assert!(Provider::Xai.requires_key());
    assert!(Provider::Mistral.requires_key());
    assert!(Provider::OpenRouter.requires_key());
    assert!(Provider::Perplexity.requires_key());
    assert!(Provider::Together.requires_key());
    assert!(!Provider::Ollama.requires_key());
    assert!(!Provider::OpenAiCompatible.requires_key());

    // Base URL: built-in endpoints for the five; required for the escape
    // hatch (config `base_url`).
    for p in [
        Provider::Xai,
        Provider::Mistral,
        Provider::OpenRouter,
        Provider::Perplexity,
        Provider::Together,
    ] {
        assert_eq!(p.base_url_requirement(), BaseUrlRequirement::None);
    }
    assert_eq!(
        Provider::OpenAiCompatible.base_url_requirement(),
        BaseUrlRequirement::Required
    );
}

#[test]
fn base_url_requirements() {
    assert_eq!(
        Provider::OpenAI.base_url_requirement(),
        BaseUrlRequirement::None
    );
    assert_eq!(
        Provider::Ollama.base_url_requirement(),
        BaseUrlRequirement::Optional(OLLAMA_DEFAULT_BASE_URL)
    );
}

/// [`LLMAgent::provider_error`] collapses an error chain to one line: the
/// provider's name plus the ROOT cause only. rig's error `Display` embeds
/// its source's text at every level, so without the collapse an HTTP
/// failure prints its JSON body once per anyhow "Caused by" level.
#[test]
fn provider_error_collapses_chain_to_root_cause() {
    let agent = LLM::new(Provider::OpenAI, "gpt-5".into(), "k".into(), None).agent("p");
    // root → wrapped twice, each layer's Display containing the inner text.
    let root = "Invalid status code 401 Unauthorized with message: {code:invalid_api_key}";
    let err = anyhow::anyhow!(root)
        .context("HttpError: inner")
        .context("CompletionError: HttpError: inner");
    let flat = format!("{:#}", agent.provider_error(err));
    assert!(flat.starts_with("OpenAI request failed: "), "got: {flat}");
    assert_eq!(flat.matches("401").count(), 1, "root cause once: {flat}");
    assert!(!flat.contains("HttpError"), "wrappers dropped: {flat}");
}
