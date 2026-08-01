use anyhow::Result;
use futures::StreamExt;
use rig::agent::{MultiTurnStreamItem, Text};
use rig::client::CompletionClient;
use rig::completion::{Prompt, StructuredOutputError, TypedPrompt};
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use std::time::Duration;

pub const DEFAULT_PROVIDER: &str = "openai";

/// Default endpoint for a locally-run Ollama server.
pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// Extra attempts a model call gets after it returns no usable content.
///
/// Reasoning models (DeepSeek's `deepseek-v4-flash` included) intermittently
/// spend their entire output budget on `reasoning_content` and come back with
/// an empty `content`, or with content truncated mid-generation. rig surfaces
/// those as [`StructuredOutputError::EmptyResponse`] and
/// [`StructuredOutputError::DeserializationError`] respectively; either one
/// previously aborted a whole multi-batch `aic` run on a single unlucky call.
/// Retrying usually succeeds because the model re-rolls its reasoning path
/// each attempt (verified against the DeepSeek API: 3 of 4 budget-starved
/// calls recovered within 3 attempts). Non-content errors are never retried —
/// see [`is_retryable`].
const RETRY_BUDGET: usize = 2;

/// Base step (ms) of the linear backoff between retries. See [`retry_backoff`].
const RETRY_BACKOFF_BASE_MS: u64 = 300;

/// Whether `err` is a "model returned no usable content" failure worth
/// retrying: rig's [`StructuredOutputError::EmptyResponse`] (no content at all)
/// or [`StructuredOutputError::DeserializationError`] (content truncated
/// mid-generation so it won't parse). Both are the common signature of a
/// reasoning model blowing its output budget on `reasoning_content`. Any other
/// error — a wrapped rig completion failure (auth, rate limit, network) or an
/// unrelated error — is left alone.
fn is_retryable(err: &anyhow::Error) -> bool {
    matches!(
        err.downcast_ref::<StructuredOutputError>(),
        Some(StructuredOutputError::EmptyResponse | StructuredOutputError::DeserializationError(_))
    )
}

/// Linear backoff between retries, so a budget-starved call gets a moment to
/// recover before the next attempt: `RETRY_BACKOFF_BASE_MS`, then `2×`, …
fn retry_backoff(attempt: usize) -> Duration {
    Duration::from_millis(RETRY_BACKOFF_BASE_MS * attempt as u64)
}

/// Run `op`, retrying it up to [`RETRY_BUDGET`] extra times when the model
/// returns no usable content (see [`is_retryable`]), with a short linear
/// backoff. Any other error propagates immediately.
async fn retry_model_call<T, F, Fut>(mut op: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let mut attempts = 0usize;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            Err(err) if is_retryable(&err) && attempts < RETRY_BUDGET => {
                attempts += 1;
                tokio::time::sleep(retry_backoff(attempts)).await;
            }
            Err(err) => return Err(err),
        }
    }
}

/// Strip a surrounding ```…``` code fence if the model ignored the "no fences"
/// instruction. Only touches a fence that wraps the entire output; partial
/// fences (e.g. a fenced block legitimately inside the file) are left alone.
pub(crate) fn strip_code_fence(mut s: &str) -> &str {
    s = s.strip_suffix('\n').unwrap_or(s).trim();
    if !s.starts_with("```") {
        return s;
    }
    // Drop the opening fence line (``` or ```lang).
    let Some(nl) = s.find('\n') else {
        return s;
    };
    s = &s[nl + 1..];
    // Drop a trailing closing fence.
    let trimmed_end = s.trim_end();
    if let Some(idx) = trimmed_end.rfind("```")
        && trimmed_end[idx..].trim() == "```"
    {
        return trimmed_end[..idx].trim();
    }
    s.trim()
}

/// Parse a JSON-structured LLM response, tolerating the stray prose or code
/// fence models occasionally emit around the payload. Strips a wrapping
/// ```` ``` ````-fence, jumps to the first value start (skipping any leading
/// prose), and lets serde_json's streaming deserializer parse exactly one value
/// — so trailing commentary is ignored without us hand-rolling brace matching.
///
/// A parse failure is reported as
/// [`StructuredOutputError::DeserializationError`] — the same classification
/// rig uses for a truncated `prompt_typed` response — so the shared retry
/// policy ([`is_retryable`]) treats tolerant-parse failures exactly like
/// typed-path truncation. The raw text rides in an anyhow context (the
/// downcast in `is_retryable` still finds the underlying error).
fn parse_json_response<T: serde::de::DeserializeOwned>(raw: &str) -> Result<T> {
    let body = strip_code_fence(raw);
    let start = body.find(['{', '[']).unwrap_or(0);
    let mut stream =
        serde_json::Deserializer::from_str(body[start..].trim_start()).into_iter::<T>();
    match stream.next() {
        Some(Ok(value)) => Ok(value),
        Some(Err(e)) => Err(
            anyhow::Error::new(StructuredOutputError::DeserializationError(e)).context(format!(
                "failed to parse LLM JSON response\n--- raw ---\n{raw}"
            )),
        ),
        None => Err(
            anyhow::Error::new(StructuredOutputError::DeserializationError(
                serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "LLM response contained no JSON value",
                )),
            ))
            .context(format!("--- raw ---\n{raw}")),
        ),
    }
}

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

#[derive(Clone)]
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

impl LLMAgent {
    /// Untyped completion, routed through [`retry_model_call`]: rig's `prompt`
    /// returns the raw assistant text, so an empty completion would surface
    /// as `Ok("")` rather than an error. Without this guard that would
    /// silently propagate (e.g. an empty file written as a conflict
    /// resolution). Empty output is classified as
    /// [`StructuredOutputError::EmptyResponse`] so the shared retry policy
    /// treats it like any other budget-starved response.
    pub async fn call(&self, prompt: &str) -> Result<String> {
        let this = self.clone();
        let prompt = prompt.to_string();
        retry_model_call(move || {
            let this = this.clone();
            let prompt = prompt.clone();
            async move {
                match with_agent!(this, agent, Ok(agent.prompt(&prompt).await?)) {
                    Ok(text) if !text.trim().is_empty() => Ok(text),
                    Ok(_) => Err(anyhow::Error::new(StructuredOutputError::EmptyResponse)),
                    Err(err) => Err(err),
                }
            }
        })
        .await
    }

    /// One streaming attempt: routes the model's "thinking"/reasoning deltas
    /// to `on_reasoning` and returns the accumulated assistant text (possibly
    /// empty). No retry here — retries live in
    /// [`Self::stream_typed_with_reasoning`], which reborrows `on_reasoning`
    /// across attempts. Providers that emit no reasoning (e.g. plain
    /// completions, Ollama) simply produce text and never call `on_reasoning`.
    async fn stream_once_with_reasoning(
        &self,
        prompt: &str,
        on_reasoning: &mut impl FnMut(&str),
    ) -> Result<String> {
        let mut output = String::new();
        with_agent!(self, agent, {
            let mut stream = agent.stream_prompt(prompt).await;
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
        Ok(output)
    }

    /// Stream a typed completion with live reasoning — the batch-plan path's
    /// analogue of [`Self::schema`].
    ///
    /// We stream the raw completion (rather than `prompt_typed`) so reasoning
    /// tokens are surfaced live, then tolerant-parse the accumulated text
    /// ourselves. A budget-starved model can still produce truncated JSON —
    /// the streaming analogue of rig's
    /// [`StructuredOutputError::DeserializationError`] — so the parse runs
    /// INSIDE the retry loop: empty output and parse failure both count as
    /// "no usable content" and get the same budget and backoff as
    /// [`Self::schema`] (see [`is_retryable`]). A real stream error (auth,
    /// rate limit, network) propagates immediately, never retried.
    ///
    /// The loop is inline rather than [`retry_model_call`]: the reasoning
    /// callback is a borrowed `FnMut`, which an escaping async closure could
    /// not reborrow across attempts — the same constraint the old
    /// `stream_with_reasoning` documented. The policy (budget, backoff,
    /// classification) is still the shared one.
    pub async fn stream_typed_with_reasoning<T>(
        &self,
        prompt: &str,
        mut on_reasoning: impl FnMut(&str),
    ) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        let mut attempts = 0usize;
        loop {
            let raw = self
                .stream_once_with_reasoning(prompt, &mut on_reasoning)
                .await?;
            let parsed = if raw.trim().is_empty() {
                Err(anyhow::Error::new(StructuredOutputError::EmptyResponse))
            } else {
                parse_json_response::<T>(&raw)
            };
            match parsed {
                Ok(value) => return Ok(value),
                Err(err) if is_retryable(&err) && attempts < RETRY_BUDGET => {
                    attempts += 1;
                    tokio::time::sleep(retry_backoff(attempts)).await;
                }
                Err(err) => return Err(err),
            }
        }
    }

    pub async fn schema<T>(&self, prompt: &str) -> Result<T>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        let this = self.clone();
        let prompt = prompt.to_string();
        retry_model_call(move || {
            let this = this.clone();
            let prompt = prompt.clone();
            async move { with_agent!(this, agent, Ok(agent.prompt_typed(&prompt).await?)) }
        })
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

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

    /// The heart of the regression fix: a model call that returns an empty
    /// response (DeepSeek reasoning models blowing their output budget) is
    /// retried and the run succeeds — instead of aborting the whole multi-batch
    /// commit run. Pins the retry count too, so a future change that, say,
    /// makes retries infinite is caught.
    #[tokio::test]
    async fn retry_model_call_retries_then_succeeds() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let result: anyhow::Result<&str> = retry_model_call(move || {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n < 2 {
                    Err(anyhow::Error::new(StructuredOutputError::EmptyResponse))
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(result.expect("must succeed after retries"), "ok");
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            RETRY_BUDGET + 1,
            "an empty response must be retried up to the configured budget"
        );
    }

    /// Persistent empty responses exhaust the retry budget and surface the
    /// original error — the run aborts with the actionable re-run message, it
    /// does not hang or loop forever.
    #[tokio::test]
    async fn retry_model_call_gives_up_after_budget() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let result: anyhow::Result<()> = retry_model_call(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::Error::new(StructuredOutputError::EmptyResponse))
            }
        })
        .await;
        let err = result.expect_err("must give up after the retry budget");
        assert!(
            is_retryable(&err),
            "the surfaced error must still be the empty-response error: {err:#}"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            RETRY_BUDGET + 1,
            "must stop after the configured budget"
        );
    }

    /// Non-empty errors are not retried — a genuine failure (auth, rate limit,
    /// context overflow) should surface immediately, not be masked by retries.
    #[tokio::test]
    async fn retry_model_call_does_not_retry_other_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let result: anyhow::Result<()> = retry_model_call(move || {
            let counter = counter.clone();
            async move {
                counter.fetch_add(1, Ordering::SeqCst);
                Err(anyhow::anyhow!("boom"))
            }
        })
        .await;
        assert_eq!(
            format!("{:#}", result.expect_err("must surface the error")),
            "boom"
        );
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            1,
            "non-empty errors must not be retried"
        );
    }

    /// Truncated content (`DeserializationError`) is retried too — a reasoning
    /// model blowing its budget mid-generation produces either an empty or a
    /// truncated response, and both should recover on retry. Locks in the
    /// broadening of `is_retryable` beyond `EmptyResponse`.
    #[tokio::test]
    async fn retry_model_call_retries_deserialization_errors() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let counter = attempts.clone();
        let result: anyhow::Result<&str> = retry_model_call(move || {
            let counter = counter.clone();
            async move {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    let err = serde_json::from_str::<serde_json::Value>("not json")
                        .expect_err("must be a parse error");
                    Err(anyhow::Error::new(
                        StructuredOutputError::DeserializationError(err),
                    ))
                } else {
                    Ok("ok")
                }
            }
        })
        .await;
        assert_eq!(result.expect("must succeed after one retry"), "ok");
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    /// `is_retryable` classifies the two unusable-content shapes as retriable
    /// and leaves everything else alone — the contract every retry path relies on.
    #[test]
    fn is_retryable_targets_unusable_content() {
        assert!(is_retryable(&anyhow::Error::new(
            StructuredOutputError::EmptyResponse
        )));
        let json_err = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("must be a parse error");
        assert!(is_retryable(&anyhow::Error::new(
            StructuredOutputError::DeserializationError(json_err)
        )));
        assert!(
            !is_retryable(&anyhow::anyhow!("network / auth / etc.")),
            "non-content errors must not be retried"
        );
    }

    // --- Tolerant output parsing (moved from generator.rs with the seam) ---

    #[test]
    fn parse_json_response_ignores_leading_prose_and_trailing_junk() {
        // Fence sits inside leading prose (so strip_code_fence can't help);
        // jump-to-`{` + serde_json's streaming parser handle both ends.
        let raw = "Here is the plan:\n```json\n{\"batches\": []}\n```\ndone";
        let out: crate::generator::BatchPlanOutput = parse_json_response(raw).unwrap();
        assert!(out.batches.is_empty());
    }

    #[test]
    fn parse_json_response_handles_escaped_quotes() {
        let raw =
            r#"{"batches": [{"changes": [{"file": "a\"b.rs", "hunks": []}], "reason": "x"}]}"#;
        let out: crate::generator::BatchPlanOutput = parse_json_response(raw).unwrap();
        assert_eq!(out.batches[0].changes[0].file, "a\"b.rs");
    }

    #[test]
    fn parse_json_response_returns_err_when_no_json() {
        let res: anyhow::Result<crate::generator::BatchPlanOutput> =
            parse_json_response("no json here at all");
        assert!(res.is_err());
    }

    /// The contract that makes batch-plan truncation retryable: a
    /// tolerant-parse failure must surface as
    /// [`StructuredOutputError::DeserializationError`], the same class rig's
    /// `prompt_typed` produces for truncated content — so `is_retryable`
    /// retries it with the same policy as the typed path.
    #[test]
    fn parse_failure_is_classified_as_deserialization_error() {
        let err = parse_json_response::<crate::generator::BatchPlanOutput>("no json here")
            .expect_err("must fail");
        assert!(
            is_retryable(&err),
            "parse failures must be retried like typed-path truncation"
        );
        assert!(matches!(
            err.downcast_ref::<StructuredOutputError>(),
            Some(StructuredOutputError::DeserializationError(_))
        ));
    }

    #[test]
    fn strip_fence_removes_wrapping_fence() {
        assert_eq!(strip_code_fence("```\nfn main() {}\n```"), "fn main() {}");
    }

    #[test]
    fn strip_fence_removes_language_tag() {
        assert_eq!(strip_code_fence("```rust\nlet x = 1;\n```"), "let x = 1;");
    }

    #[test]
    fn strip_fence_leaves_plain_content_alone() {
        assert_eq!(strip_code_fence("fn main() {}"), "fn main() {}");
    }

    #[test]
    fn strip_fence_leaves_inner_fences_alone() {
        // A fenced block that is legitimately part of the file is not stripped —
        // only a fence wrapping the *entire* output is.
        let inner = "text before\n\n```rs\ncode\n```\n\ntext after";
        assert_eq!(strip_code_fence(inner), inner);
    }
}
