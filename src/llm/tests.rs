use super::*;
use crate::cli_agent::{CliSpec, Encoding};

#[test]
fn cold_start_program_behind_the_backend_seam() {
    // The loading frame crosses ONE seam: cold_start_program composes the
    // envelope's streams_reasoning_live policy with the CLI command name.
    // A live-token-streaming CLI (claude/pi) labels the cold-start notice;
    // anything else (whole-at-completion codex/opencode, plain, or the
    // API/rig path) returns None → the silent notice.
    let claude = LlmConfig::Cli(CliSpec {
        command: "claude".into(),
        args: vec!["-p".into(), "{prompt}".into()],
        timeout_secs: 10,
        encoding: Encoding::ClaudeStreamJson,
    });
    assert_eq!(claude.cold_start_program().as_deref(), Some("claude"));

    let pi = LlmConfig::Cli(CliSpec {
        command: "pi".into(),
        args: vec!["-p".into(), "{prompt}".into()],
        timeout_secs: 10,
        encoding: Encoding::PiStreamJson,
    });
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

#[test]
fn default_models_are_refreshed() {
    assert_eq!(Provider::OpenAI.default_model(), "gpt-5-mini");
    assert_eq!(Provider::Anthropic.default_model(), "claude-haiku-4-5");
    assert_eq!(Provider::Gemini.default_model(), "gemini-2.5-flash");
    assert_eq!(Provider::DeepSeek.default_model(), "deepseek-v4-flash");
    assert_eq!(Provider::Ollama.default_model(), "llama3.3");
    assert_eq!(Provider::Mistral.default_model(), "mistral-small-latest");
}

/// AIC-12: the five Phase-1 providers (xAI, Mistral, OpenRouter,
/// Perplexity, Together) plus the OpenAI-compatible escape hatch must each
/// resolve a canonical name, an API-key requirement where the provider
/// needs one, a sensible default model, and a correct base-URL requirement.
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
    assert!(Provider::Xai.requires_key());
    assert!(Provider::Mistral.requires_key());
    assert!(Provider::OpenRouter.requires_key());
    assert!(Provider::Perplexity.requires_key());
    assert!(Provider::Together.requires_key());
    assert!(!Provider::OpenAiCompatible.requires_key());

    // Defaults: fast, low-cost models; the routers have none by design.
    assert_eq!(Provider::Xai.default_model(), "grok-4.3");
    assert_eq!(Provider::Mistral.default_model(), "mistral-small-latest");
    assert_eq!(Provider::Perplexity.default_model(), "sonar");
    assert_eq!(
        Provider::Together.default_model(),
        "meta-llama/Llama-3.3-70B-Instruct-Turbo"
    );
    assert!(Provider::OpenRouter.default_model().is_empty());
    assert!(Provider::OpenAiCompatible.default_model().is_empty());

    // Base URL: built-in endpoints for the five; required for the escape
    // hatch (config `base_url`).
    assert_eq!(
        Provider::Xai.base_url_requirement(),
        BaseUrlRequirement::None
    );
    assert_eq!(
        Provider::Mistral.base_url_requirement(),
        BaseUrlRequirement::None
    );
    assert_eq!(
        Provider::OpenRouter.base_url_requirement(),
        BaseUrlRequirement::None
    );
    assert_eq!(
        Provider::Perplexity.base_url_requirement(),
        BaseUrlRequirement::None
    );
    assert_eq!(
        Provider::Together.base_url_requirement(),
        BaseUrlRequirement::None
    );
    assert_eq!(
        Provider::OpenAiCompatible.base_url_requirement(),
        BaseUrlRequirement::Required
    );

    // Setup picker lists: curated models exist for the four with defaults;
    // OpenRouter and the escape hatch intentionally expose none.
    assert!(!Provider::Xai.models().is_empty());
    assert!(!Provider::Mistral.models().is_empty());
    assert!(Provider::OpenRouter.models().is_empty());
    assert!(!Provider::Perplexity.models().is_empty());
    assert!(!Provider::Together.models().is_empty());
    assert!(Provider::OpenAiCompatible.models().is_empty());
}

#[test]
fn routers_have_no_default_model() {
    // OpenRouter and the OpenAI-compatible escape hatch require an explicit model.
    assert!(Provider::OpenRouter.default_model().is_empty());
    assert!(Provider::OpenAiCompatible.default_model().is_empty());
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
    assert_eq!(
        Provider::OpenAiCompatible.base_url_requirement(),
        BaseUrlRequirement::Required
    );
}

#[test]
fn ollama_and_openai_compatible_do_not_require_keys() {
    assert!(!Provider::Ollama.requires_key());
    assert!(!Provider::OpenAiCompatible.requires_key());
    assert!(Provider::OpenAI.requires_key());
    assert!(Provider::Xai.requires_key());
}
