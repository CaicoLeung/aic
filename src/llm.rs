use anyhow::Result;
use futures::StreamExt;
use rig::agent::{MultiTurnStreamItem, Text};
use rig::client::CompletionClient;
use rig::completion::{Prompt, StructuredOutputError, TypedPrompt};
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use std::future::Future;
use std::io::Write;
use std::time::Duration;

pub const DEFAULT_PROVIDER: &str = "openai";

/// Default endpoint for a locally-run Ollama server.
pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    OpenAI,
    Anthropic,
    Gemini,
    DeepSeek,
    Groq,
    Ollama,
    Xai,
    Mistral,
    OpenRouter,
    Perplexity,
    Together,
    OpenAiCompatible,
}

/// How a provider treats its endpoint base URL.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BaseUrlRequirement {
    /// Cloud provider — rig's built-in endpoint is used; a base URL is ignored.
    None,
    /// Local provider — base URL is optional and falls back to the default.
    Optional(&'static str),
    /// User-defined endpoint — base URL is mandatory (OpenAI-compatible servers).
    Required,
}

/// Identity metadata for one provider. The `REGISTRY` table below is the single
/// source of truth for a provider's canonical name, aliases, API-key env var,
/// and base-URL requirement. Adding a provider = one registry row + one
/// `default_model` arm + one `with_agent!` arm. See docs/adr/0003.
struct ProviderMeta {
    provider: Provider,
    name: &'static str,
    display: &'static str,
    aliases: &'static [&'static str],
    env_key: Option<&'static str>,
    base_url: BaseUrlRequirement,
}

/// Provider registry in `aic setup` presentation order.
///
/// NOTE: the `aic-web` marketing site parses the `Provider` enum and the
/// `default_model()` match arms out of this file at build time (aic-web
/// ADR-0003). Keep the enum and those arms here and string-literal shaped.
const REGISTRY: &[ProviderMeta] = &[
    ProviderMeta {
        provider: Provider::OpenAI,
        name: "openai",
        display: "OpenAI",
        aliases: &[],
        env_key: Some("OPENAI_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Anthropic,
        name: "anthropic",
        display: "Anthropic",
        aliases: &["claude"],
        env_key: Some("ANTHROPIC_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Gemini,
        name: "gemini",
        display: "Gemini",
        aliases: &["google"],
        env_key: Some("GEMINI_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::DeepSeek,
        name: "deepseek",
        display: "DeepSeek",
        aliases: &[],
        env_key: Some("DEEPSEEK_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Groq,
        name: "groq",
        display: "Groq",
        aliases: &[],
        env_key: Some("GROQ_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Ollama,
        name: "ollama",
        display: "Ollama",
        aliases: &[],
        env_key: None,
        base_url: BaseUrlRequirement::Optional(OLLAMA_DEFAULT_BASE_URL),
    },
    ProviderMeta {
        provider: Provider::Xai,
        name: "xai",
        display: "xAI",
        aliases: &["grok"],
        env_key: Some("XAI_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Mistral,
        name: "mistral",
        display: "Mistral",
        aliases: &[],
        env_key: Some("MISTRAL_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::OpenRouter,
        name: "openrouter",
        display: "OpenRouter",
        aliases: &[],
        env_key: Some("OPENROUTER_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Perplexity,
        name: "perplexity",
        display: "Perplexity",
        aliases: &[],
        env_key: Some("PERPLEXITY_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Together,
        name: "together",
        display: "Together",
        aliases: &["together-ai"],
        env_key: Some("TOGETHER_API_KEY"),
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::OpenAiCompatible,
        name: "openai-compatible",
        display: "OpenAI-compatible",
        aliases: &["custom"],
        env_key: None,
        base_url: BaseUrlRequirement::Required,
    },
];

/// All providers in setup/presentation order.
pub const ALL_PROVIDERS: &[Provider] = &[
    Provider::OpenAI,
    Provider::Anthropic,
    Provider::Gemini,
    Provider::DeepSeek,
    Provider::Groq,
    Provider::Ollama,
    Provider::Xai,
    Provider::Mistral,
    Provider::OpenRouter,
    Provider::Perplexity,
    Provider::Together,
    Provider::OpenAiCompatible,
];

impl Provider {
    fn meta(&self) -> &'static ProviderMeta {
        REGISTRY
            .iter()
            .find(|m| m.provider == *self)
            .expect("every Provider variant has a registry row")
    }

    pub fn from_name(s: &str) -> Self {
        let lower = s.to_lowercase();
        for m in REGISTRY {
            if m.name == lower || m.aliases.iter().any(|a| *a == lower) {
                return m.provider;
            }
        }
        Provider::OpenAI
    }

    pub fn name(&self) -> &'static str {
        self.meta().name
    }

    pub fn display(&self) -> &'static str {
        self.meta().display
    }

    pub fn env_key(&self) -> Option<&'static str> {
        self.meta().env_key
    }

    pub fn base_url_requirement(&self) -> BaseUrlRequirement {
        self.meta().base_url
    }

    pub fn all() -> &'static [Provider] {
        ALL_PROVIDERS
    }

    /// Default model for a provider. An empty string means the provider has no
    /// default and the user must supply one (OpenRouter, OpenAI-compatible).
    ///
    /// The `aic-web` site parses these match arms at build time, so keep this a
    /// `match self` with string-literal arms (aic-web ADR-0003).
    pub fn default_model(&self) -> &'static str {
        match self {
            Self::OpenAI => "gpt-5-mini",
            Self::Anthropic => "claude-haiku-4-5",
            Self::Gemini => "gemini-2.5-flash",
            Self::DeepSeek => "deepseek-v4-flash",
            Self::Groq => "llama-3.3-70b-versatile",
            Self::Ollama => "llama3.3",
            Self::Xai => "grok-4.3",
            Self::Mistral => "mistral-small-latest",
            Self::OpenRouter => "",
            Self::Perplexity => "sonar",
            Self::Together => "meta-llama/Llama-3.3-70B-Instruct-Turbo",
            Self::OpenAiCompatible => "",
        }
    }
}

#[derive(Clone)]
pub struct LLM {
    pub provider: Provider,
    pub model: String,
    pub api_key: String,
    pub base_url: Option<String>,
}

impl LLM {
    pub fn from_env() -> Result<Self> {
        let config = crate::config::Config::load().ok().flatten();
        let resolved = crate::config::ResolvedConfig::resolve(config.as_ref());
        resolved.validate()?;
        Ok(Self {
            provider: Provider::from_name(&resolved.backend),
            model: resolved.model,
            api_key: resolved.api_key,
            base_url: resolved.base_url,
        })
    }

    pub fn agent(&self, system_prompt: impl Into<String>) -> LLMAgent {
        LLMAgent {
            llm: self.clone(),
            system_prompt: system_prompt.into(),
        }
    }
}

pub struct LLMAgent {
    llm: LLM,
    system_prompt: String,
}

macro_rules! with_agent {
    ($self:expr, $agent:ident, $body:expr) => {
        match &$self.llm.provider {
            Provider::OpenAI => {
                let client = rig::providers::openai::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::Anthropic => {
                let client = rig::providers::anthropic::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::Gemini => {
                let client = rig::providers::gemini::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::DeepSeek => {
                let client = rig::providers::deepseek::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::Groq => {
                let client = rig::providers::groq::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::Xai => {
                let client = rig::providers::xai::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::Mistral => {
                let client = rig::providers::mistral::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::OpenRouter => {
                let client = rig::providers::openrouter::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::Perplexity => {
                let client = rig::providers::perplexity::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::Together => {
                let client = rig::providers::together::Client::new(&$self.llm.api_key)?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::Ollama => {
                let url = $self
                    .llm
                    .base_url
                    .as_deref()
                    .unwrap_or(OLLAMA_DEFAULT_BASE_URL);
                let api_key = if $self.llm.api_key.is_empty() {
                    rig::providers::ollama::OllamaApiKey::default()
                } else {
                    rig::providers::ollama::OllamaApiKey::from($self.llm.api_key.clone())
                };
                let client = rig::providers::ollama::Client::builder()
                    .api_key(api_key)
                    .base_url(url)
                    .build()?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
            Provider::OpenAiCompatible => {
                let base_url = $self.llm.base_url.as_deref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "the openai-compatible provider requires a base URL — set LLM_BASE_URL or \
                         `base_url` in config"
                    )
                })?;
                // Local OpenAI-compatible servers often need no key; pass a
                // placeholder so rig's required api-key builder field is satisfied.
                let api_key = if $self.llm.api_key.is_empty() {
                    String::from("no-key")
                } else {
                    $self.llm.api_key.clone()
                };
                let client = rig::providers::openai::Client::builder()
                    .api_key(&api_key)
                    .base_url(base_url)
                    .build()?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
        }
    };
}

// --- LLM call retry --------------------------------------------------------
//
// Two orthogonal failure modes are retried under one budget of
// [`LLM_MAX_ATTEMPTS`] attempts with exponential backoff:
//
//   1. Budget-exhausted model output. Reasoning models (e.g. DeepSeek's
//      `deepseek-v4-flash`) sometimes spend their entire output budget on
//      `reasoning_content` and come back empty or truncated. rig surfaces
//      those as [`StructuredOutputError::EmptyResponse`] /
//      [`DeserializationError`]; a plain [`Prompt`] instead returns `Ok("")`.
//      Retrying re-rolls the model's reasoning path and usually succeeds.
//
//   2. Transient transport/provider errors (rate limits, network blips, 5xx).
//
// Permanent failures (HTTP 4xx auth / bad request, except 408 and 429) fail
// fast — pointlessly retrying them only wastes time. Classification is driven
// by the *typed* HTTP status reachable in the error chain (rig's
// `http_client::Error` carries a `StatusCode` for every non-2xx response), not
// by substring-sniffing the message, so an incidental token like a port number
// or path can never flip a transient failure into a permanent one. Errors with
// no typed status (network blip, EOF, an unrecognized failure) default to
// transient — retrying is always safer than aborting a whole multi-batch Run.

/// Total attempts per LLM call (the first try plus retries).
const LLM_MAX_ATTEMPTS: u32 = 3;
/// Base backoff, doubled per attempt (1s, 2s, 4s) plus up to 500ms of jitter.
const LLM_BASE_DELAY: Duration = Duration::from_secs(1);

/// Whether `err` is a "model returned no usable content" failure worth
/// retrying: rig's [`StructuredOutputError::EmptyResponse`] (no content at all)
/// or [`StructuredOutputError::DeserializationError`] (content truncated
/// mid-generation so it won't parse). Both are the signature of a reasoning
/// model blowing its output budget on `reasoning_content`.
fn is_empty_response(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<StructuredOutputError>(),
        Some(StructuredOutputError::EmptyResponse | StructuredOutputError::DeserializationError(_))
    )
}

/// Whether `err` is a transient transport/provider failure worth retrying.
///
/// Walks the error chain for rig's typed HTTP status and classifies on the real
/// code (see [`is_transient_status`]); an error with no typed status anywhere
/// in the chain (network blip, EOF, an unrecognized failure) defaults to
/// transient — retrying is always safer than aborting a whole multi-batch Run
/// on an unrecognized failure.
fn is_transient(err: &anyhow::Error) -> bool {
    match http_status(err) {
        Some(code) => is_transient_status(code),
        None => true,
    }
}

/// Extract the first HTTP status code reachable in `err`'s cause chain. rig
/// surfaces every non-2xx response as `http_client::Error::InvalidStatusCode`
/// or `InvalidStatusCodeWithMessage`, so this finds real provider statuses
/// without string-sniffing the formatted message.
fn http_status(err: &anyhow::Error) -> Option<u16> {
    for cause in err.chain() {
        if let Some(code) = cause
            .downcast_ref::<rig::http_client::Error>()
            .and_then(status_from_http_error)
        {
            return Some(code);
        }
    }
    None
}

fn status_from_http_error(e: &rig::http_client::Error) -> Option<u16> {
    match e {
        rig::http_client::Error::InvalidStatusCode(s) => Some(s.as_u16()),
        rig::http_client::Error::InvalidStatusCodeWithMessage(s, _) => Some(s.as_u16()),
        _ => None,
    }
}

/// Standard retry classification: permanent client errors fail fast (4xx,
/// except 408 Request Timeout and 429 Too Many Requests, which are transient);
/// everything else (5xx, network, unknown status) is transient.
fn is_transient_status(code: u16) -> bool {
    if (400..500).contains(&code) {
        code == 408 || code == 429
    } else {
        true
    }
}

/// Retry when the model returned no usable content OR a transient error
/// occurred. Only a clear permanent failure (auth / bad request) fails fast.
fn should_retry(err: &anyhow::Error) -> bool {
    is_empty_response(err) || is_transient(err)
}

fn retry_notice(attempt: u32, max: u32, backoff: Duration, err: &anyhow::Error) {
    eprintln!("aic: LLM call failed (attempt {attempt}/{max}); retrying in {backoff:?}: {err:#}");
}

/// Up to 500ms of jitter so concurrent retries don't synchronize.
fn jitter() -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    Duration::from_millis((nanos % 500) as u64)
}

/// Classify `err` and, if it is retryable, return the backoff to wait before
/// the next attempt (printing the retry notice). `None` means stop: out of
/// budget, or a permanent error. The single source of truth for retry *policy*
/// — shared by [`with_retry`] (the non-streaming call sites) and the streaming
/// loop in [`LLMAgent::stream_with_reasoning`], which keeps its own outer loop
/// only because its `&mut` reasoning callback can't escape a `FnMut` closure.
fn retry_backoff(attempt: u32, err: &anyhow::Error) -> Option<Duration> {
    if attempt >= LLM_MAX_ATTEMPTS || !should_retry(err) {
        return None;
    }
    let backoff = LLM_BASE_DELAY * 2u32.pow(attempt - 1);
    retry_notice(attempt, LLM_MAX_ATTEMPTS, backoff, err);
    Some(backoff)
}

/// Run `f`, retrying on retryable failures (empty/budget output or a transient
/// error) with exponential backoff. A permanent error fails fast. A
/// best-effort notice is printed to stderr on each retry.
async fn with_retry<T, F, Fut>(mut f: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempt = 0;
    loop {
        attempt += 1;
        match f().await {
            Ok(v) => return Ok(v),
            Err(e) => match retry_backoff(attempt, &e) {
                Some(backoff) => {
                    tokio::time::sleep(backoff + jitter()).await;
                    continue;
                }
                None => return Err(e),
            },
        }
    }
}

impl LLMAgent {
    pub async fn call(&self, prompt: &str) -> Result<String> {
        let prompt = prompt.to_string();
        // rig's `prompt` returns raw assistant text, so a budget-starved
        // completion surfaces as `Ok("")` rather than an error (e.g. an empty
        // commit message, or an empty conflict-resolution file). Treat empty
        // output as an `EmptyResponse` failure so `with_retry` retries it;
        // after the budget the run aborts with the actionable error.
        with_retry(|| {
            let prompt = prompt.clone();
            async move {
                match with_agent!(self, agent, Ok(agent.prompt(&prompt).await?)) {
                    Ok(text) if !text.trim().is_empty() => Ok(text),
                    Ok(_) => Err(anyhow::Error::new(StructuredOutputError::EmptyResponse)),
                    Err(err) => Err(err),
                }
            }
        })
        .await
    }

    pub async fn stream(&self, prompt: &str) -> Result<String> {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let mut output = String::new();

        with_agent!(self, agent, {
            let mut stream = agent.stream_prompt(prompt).await;
            while let Some(item) = stream.next().await {
                match item {
                    Ok(MultiTurnStreamItem::StreamAssistantItem(
                        StreamedAssistantContent::Text(Text { text }),
                    )) => {
                        write!(lock, "{text}")?;
                        output.push_str(&text);
                    }
                    Ok(_) => {}
                    Err(e) => anyhow::bail!("Stream error: {e}"),
                }
            }
        });

        writeln!(lock)?;
        lock.flush()?;
        Ok(output)
    }

    /// Like [`Self::stream`], but routes the model's "thinking"/reasoning
    /// deltas to `on_reasoning` instead of printing them, and returns only the
    /// accumulated assistant text. Providers that emit no reasoning (e.g. plain
    /// completions, Ollama) simply produce text and never call `on_reasoning`.
    pub async fn stream_with_reasoning(
        &self,
        prompt: &str,
        mut on_reasoning: impl FnMut(&str),
    ) -> Result<String> {
        let prompt = prompt.to_string();
        // The retry *policy* is shared with the non-streaming call sites via
        // [`retry_backoff`]; this method keeps its own outer loop only because
        // the reasoning callback is borrowed mutably and cannot escape a
        // `FnMut` closure into a returned future (the lending-callback
        // constraint). Each attempt's async block borrows `on_reasoning`
        // locally and is fully awaited before the next attempt.
        let mut attempt = 0;
        loop {
            attempt += 1;
            let outcome = async {
                let mut output = String::new();
                with_agent!(self, agent, {
                    let mut stream = agent.stream_prompt(&prompt).await;
                    while let Some(item) = stream.next().await {
                        match item {
                            Ok(MultiTurnStreamItem::StreamAssistantItem(
                                StreamedAssistantContent::Text(Text { text }),
                            )) => output.push_str(&text),
                            Ok(MultiTurnStreamItem::StreamAssistantItem(
                                StreamedAssistantContent::ReasoningDelta { reasoning, .. },
                            )) => on_reasoning(&reasoning),
                            Ok(MultiTurnStreamItem::StreamAssistantItem(
                                StreamedAssistantContent::Reasoning(r),
                            )) => {
                                let text = r.display_text();
                                if !text.is_empty() {
                                    on_reasoning(&text);
                                }
                            }
                            Ok(_) => {}
                            Err(e) => anyhow::bail!("Stream error: {e}"),
                        }
                    }
                });
                // A stream that produced only reasoning and no text is the
                // streaming analogue of `EmptyResponse`: retry like `call`.
                if output.trim().is_empty() {
                    return Err(anyhow::Error::new(StructuredOutputError::EmptyResponse));
                }
                Ok(output)
            }
            .await;
            match outcome {
                Ok(v) => return Ok(v),
                Err(e) => match retry_backoff(attempt, &e) {
                    Some(backoff) => {
                        tokio::time::sleep(backoff + jitter()).await;
                        continue;
                    }
                    None => return Err(e),
                },
            }
        }
    }

    pub async fn schema<T>(&self, prompt: &str) -> Result<T>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        let prompt = prompt.to_string();
        with_retry(|| {
            let prompt = prompt.clone();
            async move { with_agent!(self, agent, Ok(agent.prompt_typed::<T>(&prompt).await?)) }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn ollama_has_no_env_key_but_cloud_providers_do() {
        assert_eq!(Provider::Ollama.env_key(), None);
        assert_eq!(Provider::OpenAiCompatible.env_key(), None);
        assert!(Provider::OpenAI.env_key().is_some());
        assert!(Provider::Xai.env_key().is_some());
        assert_eq!(Provider::Xai.env_key(), Some("XAI_API_KEY"));
    }

    /// Build an anyhow error carrying a typed rig HTTP status, mirroring what
    /// rig emits for a non-2xx response (`InvalidStatusCodeWithMessage`). Used
    /// to exercise the typed classification path without a live network call.
    fn http_status_err(code: http::StatusCode, msg: &str) -> anyhow::Error {
        anyhow::Error::new(rig::http_client::Error::InvalidStatusCodeWithMessage(
            code,
            msg.to_string(),
        ))
    }

    #[test]
    fn is_transient_classifies_typed_4xx_as_permanent() {
        // Real auth / request-shape errors carry a typed HTTP status → fail
        // fast. These are no longer matched by sniffing the message string.
        for code in [400, 401, 403, 404, 422] {
            assert!(
                !is_transient(&http_status_err(
                    http::StatusCode::from_u16(code).unwrap(),
                    "err"
                )),
                "{code} must be permanent"
            );
        }
    }

    #[test]
    fn is_transient_retries_typed_5xx_and_transient_4xx() {
        // 408 (Request Timeout) and 429 (Too Many Requests) are the two 4xx
        // codes that are transient; all 5xx are transient.
        for code in [408, 429, 500, 502, 503, 504] {
            assert!(
                is_transient(&http_status_err(
                    http::StatusCode::from_u16(code).unwrap(),
                    "err"
                )),
                "{code} must be transient"
            );
        }
    }

    #[test]
    fn is_transient_defaults_unknown_to_transient() {
        // No typed status anywhere in the chain → default transient (retry),
        // never aborting a whole multi-batch Run on an unrecognized failure.
        assert!(is_transient(&anyhow::anyhow!("connection reset by peer")));
        assert!(is_transient(&anyhow::anyhow!("timeout")));
        assert!(is_transient(&anyhow::anyhow!("EOF while reading header")));
        // A bare message containing "401" but carrying NO typed status is still
        // transient — classification is driven by the typed status, so an
        // incidental token (a port, a path) can't flip it to permanent.
        assert!(is_transient(&anyhow::anyhow!("redirected to port 401")));
    }

    #[test]
    fn is_transient_finds_status_through_wrapped_chain() {
        // rig wraps the HTTP status under PromptError / CompletionError layers;
        // anyhow's `.context` mirrors that wrapping, and the chain walk in
        // [`http_status`] must still reach the typed status.
        let err =
            http_status_err(http::StatusCode::UNAUTHORIZED, "bad key").context("prompt failed");
        assert_eq!(http_status(&err), Some(401));
        assert!(
            !is_transient(&err),
            "status must be found through the wrapper"
        );
    }

    #[test]
    fn is_empty_response_targets_unusable_content() {
        assert!(is_empty_response(&anyhow::Error::new(
            StructuredOutputError::EmptyResponse
        )));
        let json_err = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("must be a parse error");
        assert!(is_empty_response(&anyhow::Error::new(
            StructuredOutputError::DeserializationError(json_err)
        )));
        assert!(
            !is_empty_response(&anyhow::anyhow!("network / auth / etc.")),
            "non-content errors must not match the empty-response predicate"
        );
    }

    #[test]
    fn should_retry_covers_both_failure_modes() {
        // Budget-exhausted model output is retryable.
        assert!(should_retry(&anyhow::Error::new(
            StructuredOutputError::EmptyResponse
        )));
        // A transient network error (no typed status) is retryable.
        assert!(should_retry(&anyhow::anyhow!("connection reset by peer")));
        assert!(should_retry(&http_status_err(
            http::StatusCode::INTERNAL_SERVER_ERROR,
            "5xx"
        )));
        // A permanent 4xx error fails fast.
        assert!(!should_retry(&http_status_err(
            http::StatusCode::UNAUTHORIZED,
            "no key"
        )));
        assert!(!should_retry(&http_status_err(
            http::StatusCode::BAD_REQUEST,
            "bad req"
        )));
    }

    /// A retryable failure (empty model output) is retried until it succeeds —
    /// the run recovers instead of aborting on a single unlucky call. Pins the
    /// attempt count so an accidental infinite loop, or a missing retry, is
    /// caught.
    #[tokio::test]
    async fn with_retry_retries_empty_response_then_succeeds() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let result: anyhow::Result<&str> = with_retry(move || {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    Err(anyhow::Error::new(StructuredOutputError::EmptyResponse))
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(result.expect("must succeed after a retry"), "ok");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            2,
            "an empty response must be retried"
        );
    }

    /// Persistent retryable failures exhaust the budget and surface the
    /// original error — the run aborts with the actionable re-run message; it
    /// does not hang or loop forever.
    #[tokio::test]
    async fn with_retry_gives_up_after_budget() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let result: anyhow::Result<()> = with_retry(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::Error::new(StructuredOutputError::EmptyResponse))
            }
        })
        .await;
        let err = result.expect_err("must give up after the retry budget");
        assert!(
            should_retry(&err),
            "the surfaced error must still be the empty-response error: {err:#}"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            LLM_MAX_ATTEMPTS as usize,
            "must stop after the configured budget"
        );
    }

    /// A permanent error (auth / bad request) is surfaced immediately, not
    /// masked by retries.
    #[tokio::test]
    async fn with_retry_fails_fast_on_permanent_error() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let result: anyhow::Result<()> = with_retry(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(http_status_err(
                    http::StatusCode::UNAUTHORIZED,
                    "Unauthorized",
                ))
            }
        })
        .await;
        let err = result.expect_err("must surface the error");
        assert_eq!(
            http_status(&err),
            Some(401),
            "the surfaced error must still carry the 401 status: {err:#}"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "permanent errors must not be retried"
        );
    }
}
