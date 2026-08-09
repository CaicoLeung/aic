//! Retrying "no usable content" responses from reasoning models.
//!
//! Reasoning models (DeepSeek's `deepseek-v4-flash` included) intermittently
//! spend their entire output budget on `reasoning_content` and come back with
//! an empty `content`, or with content truncated mid-generation. rig surfaces
//! those as [`StructuredOutputError::EmptyResponse`] and
//! [`StructuredOutputError::DeserializationError`] respectively; either one
//! previously aborted a whole multi-batch `aic` run on a single unlucky call.
//! Retrying usually succeeds because the model re-rolls its reasoning path
//! each attempt (verified against the DeepSeek API: 3 of 4 budget-starved
//! calls recovered within 3 attempts). All retry seams share the
//! [`crate::retry`] module: [`classify_retry`] is the single rig→reason
//! mapping at this boundary, and every seam uses [`RetryPolicy::transient`]
//! (budget 2, 300 ms linear backoff). Non-content errors are never retried.

use crate::retry::{RetryPolicy, RetryReason, retry, should_retry};
use anyhow::Result;
use futures::StreamExt;
use rig::agent::{MultiTurnStreamItem, Text};
use rig::client::AgentClientExt;
use rig::completion::{Prompt, StructuredOutputError, TypedPrompt};
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};

pub const DEFAULT_PROVIDER: &str = "openai";

/// Default endpoint for a locally-run Ollama server.
pub const OLLAMA_DEFAULT_BASE_URL: &str = "http://localhost:11434";

/// The single rig→[`RetryReason`] mapping at the llm boundary: rig's
/// [`StructuredOutputError::EmptyResponse`] (no content) and
/// [`StructuredOutputError::DeserializationError`] (content truncated
/// mid-generation) are the retryable "no usable content" failures; anything
/// else — a wrapped rig completion failure (auth, rate limit, network) or an
/// unrelated error — is `None`, so the caller propagates the original error
/// unchanged. This is the only place the retry module touches a rig type.
fn classify_retry(err: &anyhow::Error) -> Option<RetryReason> {
    match err.downcast_ref::<StructuredOutputError>() {
        Some(StructuredOutputError::EmptyResponse) => Some(RetryReason::Empty),
        Some(StructuredOutputError::DeserializationError(_)) => Some(RetryReason::Truncated),
        _ => None,
    }
}

/// Consume `err` into a [`RetryReason`] for the [`retry`] closures: the
/// retryable shapes from [`classify_retry`], anything else as
/// [`RetryReason::Fatal`] carrying the error verbatim.
fn classify_or_fatal(err: anyhow::Error) -> RetryReason {
    classify_retry(&err).unwrap_or(RetryReason::Fatal(err))
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
/// policy ([`classify_retry`]) treats tolerant-parse failures exactly like
/// typed-path truncation. The raw text rides in an anyhow context (the
/// downcast in `classify_retry` still finds the underlying error).
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
/// source of truth for a provider's canonical name, aliases, API-key
/// requirement, and base-URL requirement. Adding a provider = one registry row + one
/// `default_model` arm + one `with_agent!` arm. See docs/adr/0003.
struct ProviderMeta {
    provider: Provider,
    name: &'static str,
    display: &'static str,
    aliases: &'static [&'static str],
    requires_key: bool,
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
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Anthropic,
        name: "anthropic",
        display: "Anthropic",
        aliases: &["claude"],
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Gemini,
        name: "gemini",
        display: "Gemini",
        aliases: &["google"],
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::DeepSeek,
        name: "deepseek",
        display: "DeepSeek",
        aliases: &[],
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Groq,
        name: "groq",
        display: "Groq",
        aliases: &[],
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Ollama,
        name: "ollama",
        display: "Ollama",
        aliases: &[],
        requires_key: false,
        base_url: BaseUrlRequirement::Optional(OLLAMA_DEFAULT_BASE_URL),
    },
    ProviderMeta {
        provider: Provider::Xai,
        name: "xai",
        display: "xAI",
        aliases: &["grok"],
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Mistral,
        name: "mistral",
        display: "Mistral",
        aliases: &[],
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::OpenRouter,
        name: "openrouter",
        display: "OpenRouter",
        aliases: &[],
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Perplexity,
        name: "perplexity",
        display: "Perplexity",
        aliases: &[],
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::Together,
        name: "together",
        display: "Together",
        aliases: &["together-ai"],
        requires_key: true,
        base_url: BaseUrlRequirement::None,
    },
    ProviderMeta {
        provider: Provider::OpenAiCompatible,
        name: "openai-compatible",
        display: "OpenAI-compatible",
        aliases: &["custom"],
        requires_key: false,
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

    pub fn requires_key(&self) -> bool {
        self.meta().requires_key
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

    /// Curated, currently-recommended model IDs for the `aic setup` picker.
    /// Empty for providers where a fixed list doesn't fit (OpenRouter exposes
    /// thousands; OpenAI-compatible points at a user's own server). The picker
    /// pre-selects the provider's [`default_model`](Self::default_model) when
    /// present, otherwise the first entry. These are best-effort and may lag
    /// behind each provider's latest releases — the picker always offers a
    /// "custom" escape hatch.
    pub fn models(&self) -> &'static [&'static str] {
        match self {
            Self::OpenAI => &["gpt-5", "gpt-5-mini", "gpt-5-nano"],
            Self::Anthropic => &["claude-sonnet-4-5", "claude-haiku-4-5"],
            Self::Gemini => &[
                "gemini-2.5-pro",
                "gemini-2.5-flash",
                "gemini-2.5-flash-lite",
            ],
            Self::DeepSeek => &["deepseek-v4-flash", "deepseek-v4-pro"],
            Self::Groq => &[
                "llama-3.3-70b-versatile",
                "llama-3.1-8b-instant",
                "openai/gpt-oss-120b",
            ],
            Self::Ollama => &["llama3.3", "qwen2.5", "qwen3", "deepseek-r1"],
            Self::Xai => &["grok-4.5", "grok-4.3"],
            Self::Mistral => &[
                "mistral-large-latest",
                "mistral-small-latest",
                "codestral-latest",
            ],
            Self::OpenRouter => &[],
            Self::Perplexity => &[
                "sonar",
                "sonar-pro",
                "sonar-reasoning-pro",
                "sonar-deep-research",
            ],
            Self::Together => &[
                "meta-llama/Llama-3.3-70B-Instruct-Turbo",
                "meta-llama/Llama-4-Scout-17B-16E-Instruct",
                "deepseek-ai/DeepSeek-V4-Pro",
            ],
            Self::OpenAiCompatible => &[],
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
    /// Load the runtime [`LLM`] from the resolved config file. aic reads only
    /// the config file — no environment variables — so it is the single source
    /// of truth (ADR 0008).
    pub fn load() -> Result<Self> {
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
                        "the openai-compatible provider requires a base URL — set \
                         `base_url` in config (run `aic setup`)"
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
    /// One untyped-completion attempt via rig's `prompt`. Returns
    /// [`anyhow::Result`] so the provider-client `?`s inside [`with_agent!`]
    /// convert to `anyhow::Error`; the retry closure in [`Self::call`] maps
    /// that to a [`RetryReason`] at the boundary via [`classify_or_fatal`].
    async fn prompt_once(&self, prompt: &str) -> anyhow::Result<String> {
        with_agent!(self, agent, Ok(agent.prompt(prompt).await?))
    }

    /// Untyped completion, routed through [`retry`] with
    /// [`RetryPolicy::transient`]: rig's `prompt` returns the raw assistant
    /// text, so an empty completion would surface as `Ok("")` rather than an
    /// error. Without this guard that would silently propagate (e.g. an empty
    /// file written as a conflict resolution). Empty output is classified as
    /// [`RetryReason::Empty`] so the shared retry policy treats it like any
    /// other budget-starved response. A non-content failure maps to
    /// [`RetryReason::Fatal`] and propagates immediately.
    pub async fn call(&self, prompt: &str) -> Result<String> {
        let this = self.clone();
        let prompt = prompt.to_string();
        Ok(retry(
            move || {
                let this = this.clone();
                let prompt = prompt.clone();
                async move {
                    let text = this.prompt_once(&prompt).await.map_err(classify_or_fatal)?;
                    if text.trim().is_empty() {
                        Err(RetryReason::Empty)
                    } else {
                        Ok(text)
                    }
                }
            },
            RetryPolicy::transient(),
        )
        .await?)
    }

    /// One-shot connectivity check: a single minimal completion attempt with
    /// **no retry**. Used by the `aic setup` Verify item (AIC-23) to confirm
    /// the API key + model are usable before the config is saved. Unlike
    /// [`Self::call`], a budget-starved empty response is not retried — Verify
    /// is a user-initiated probe, and the user would rather see the raw
    /// outcome than wait for backoff. Any real failure (auth, rate limit,
    /// network, unknown model) propagates verbatim so the wizard can show it
    /// and the user can act on it. Returns the model's trimmed reply on
    /// success.
    pub async fn verify(&self) -> Result<String> {
        let text = self.prompt_once("Reply with exactly: OK").await?;
        Ok(text.trim().to_string())
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
                        StreamedAssistantContent::Text(Text { text, .. }),
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
    /// [`Self::schema`] via the shared [`crate::retry::should_retry`] +
    /// [`RetryPolicy::transient`] (see [`classify_retry`]). A real stream
    /// error (auth, rate limit, network) propagates immediately, never
    /// retried.
    /// The loop is inline rather than [`crate::retry::retry`]: the reasoning
    /// callback is a borrowed `FnMut`, which an escaping async closure could
    /// not reborrow across attempts — the same constraint the old
    /// `stream_with_reasoning` documented. The budget gate and backoff are
    /// the shared module's, so this seam can't drift from the typed/untyped
    /// paths.
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
                Err(err) => match classify_retry(&err) {
                    Some(reason) => {
                        match should_retry(&reason, &mut attempts, RetryPolicy::transient()) {
                            Some(backoff) => tokio::time::sleep(backoff).await,
                            None => return Err(err),
                        }
                    }
                    None => return Err(err),
                },
            }
        }
    }

    /// One typed-completion attempt via rig's `prompt_typed`. Returns
    /// [`anyhow::Result`] so the provider-client `?`s inside [`with_agent!`]
    /// convert to `anyhow::Error`; the retry closure in [`Self::schema`] maps
    /// that to a [`RetryReason`] at the boundary via [`classify_or_fatal`].
    async fn prompt_typed_once<T>(&self, prompt: &str) -> anyhow::Result<T>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        with_agent!(self, agent, Ok(agent.prompt_typed(prompt).await?))
    }

    /// Typed (JSON-schema) completion — the Drafted-Message (commit-message)
    /// path. Routed through [`retry`] with [`RetryPolicy::transient`]: rig's
    /// `prompt_typed` surfaces a budget-starved response as
    /// [`StructuredOutputError::EmptyResponse`] / [`DeserializationError`],
    /// which [`classify_retry`] maps to the retryable [`RetryReason::Empty`] /
    /// [`RetryReason::Truncated`]. Any other failure propagates immediately.
    pub async fn schema<T>(&self, prompt: &str) -> Result<T>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        let this = self.clone();
        let prompt = prompt.to_string();
        Ok(retry(
            move || {
                let this = this.clone();
                let prompt = prompt.clone();
                async move {
                    this.prompt_typed_once::<T>(&prompt)
                        .await
                        .map_err(classify_or_fatal)
                }
            },
            RetryPolicy::transient(),
        )
        .await?)
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

    /// [`classify_retry`] is the boundary mapping every retry seam relies on:
    /// the two unusable-content shapes become retryable reasons — including
    /// through the anyhow context `parse_json_response` adds — and anything
    /// else is `None`, propagating unchanged.
    #[test]
    fn classify_retry_maps_unusable_content() {
        assert!(matches!(
            classify_retry(&anyhow::Error::new(StructuredOutputError::EmptyResponse)),
            Some(RetryReason::Empty)
        ));
        let json_err = serde_json::from_str::<serde_json::Value>("not json")
            .expect_err("must be a parse error");
        assert!(matches!(
            classify_retry(&anyhow::Error::new(
                StructuredOutputError::DeserializationError(json_err)
            )),
            Some(RetryReason::Truncated)
        ));
        // Context-wrapped, as parse_json_response produces it.
        let wrapped = anyhow::Error::new(StructuredOutputError::DeserializationError(
            serde_json::from_str::<serde_json::Value>("nope").expect_err("must be a parse error"),
        ))
        .context("failed to parse LLM JSON response");
        assert!(matches!(
            classify_retry(&wrapped),
            Some(RetryReason::Truncated)
        ));
        assert!(
            classify_retry(&anyhow::anyhow!("network / auth / etc.")).is_none(),
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
    /// `prompt_typed` produces for truncated content — so [`classify_retry`]
    /// retries it with the same policy as the typed path.
    #[test]
    fn parse_failure_is_classified_as_deserialization_error() {
        let err = parse_json_response::<crate::generator::BatchPlanOutput>("no json here")
            .expect_err("must fail");
        assert!(
            matches!(classify_retry(&err), Some(RetryReason::Truncated)),
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

    #[test]
    fn strip_fence_bare_opening_without_newline_is_left_alone() {
        // A lone opening fence with no content line after it has nothing to
        // strip — returned unchanged rather than producing a dangling slice.
        assert_eq!(strip_code_fence("```"), "```");
        assert_eq!(strip_code_fence("```rust"), "```rust");
    }

    #[test]
    fn strip_fence_opening_with_unclean_closing_keeps_trailing_text() {
        // A closing fence followed by trailing text is not a clean wrapper: the
        // opening fence line is still dropped (so the body is exposed to the
        // tolerant parser), but the trailing "``` text" stays in place — the
        // fallthrough keeps whatever followed the opening fence.
        assert_eq!(
            strip_code_fence("```rust\nlet x = 1;\n``` trailing"),
            "let x = 1;\n``` trailing"
        );
    }
}
