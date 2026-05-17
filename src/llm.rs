use anyhow::Result;
use futures::StreamExt;
use rig::agent::{MultiTurnStreamItem, Text};
use rig::client::CompletionClient;
use rig::completion::{Prompt, TypedPrompt};
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use std::io::Write;

pub const DEFAULT_PROVIDER: &str = "openai";

#[derive(Debug, Clone, PartialEq)]
pub enum Provider {
    OpenAI,
    Anthropic,
    Gemini,
    DeepSeek,
    Groq,
    Ollama,
}

impl Provider {
    pub fn from_name(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "anthropic" | "claude" => Self::Anthropic,
            "gemini" | "google" => Self::Gemini,
            "deepseek" => Self::DeepSeek,
            "groq" => Self::Groq,
            "ollama" => Self::Ollama,
            _ => Self::OpenAI,
        }
    }

    pub fn default_model(&self) -> &'static str {
        match self {
            Self::OpenAI => "gpt-4o-mini",
            Self::Anthropic => "claude-sonnet-4-20250514",
            Self::Gemini => "gemini-2.0-flash",
            Self::DeepSeek => "deepseek-chat",
            Self::Groq => "llama-3.3-70b-versatile",
            Self::Ollama => "llama3.2",
        }
    }

    pub fn env_key(&self) -> Option<&'static str> {
        match self {
            Self::OpenAI => Some("OPENAI_API_KEY"),
            Self::Anthropic => Some("ANTHROPIC_API_KEY"),
            Self::Gemini => Some("GEMINI_API_KEY"),
            Self::DeepSeek => Some("DEEPSEEK_API_KEY"),
            Self::Groq => Some("GROQ_API_KEY"),
            Self::Ollama => None,
        }
    }
}

#[derive(Clone)]
pub struct LLM {
    pub provider: Provider,
    pub model: String,
    pub api_key: String,
}

impl Default for LLM {
    fn default() -> Self {
        Self::from_env()
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
            Provider::Ollama => {
                let client =
                    rig::providers::ollama::Client::new("http://localhost:11434".to_string())?;
                let $agent = client
                    .agent(&$self.llm.model)
                    .preamble(&$self.system_prompt)
                    .build();
                $body
            }
        }
    };
}

impl LLM {
    pub fn from_env() -> Self {
        let config = crate::config::Config::load().ok().flatten();
        let resolved = crate::config::ResolvedConfig::resolve(config.as_ref());
        Self {
            provider: Provider::from_name(&resolved.backend),
            model: resolved.model,
            api_key: resolved.api_key,
        }
    }

    pub fn agent(&self, system_prompt: impl Into<String>) -> LLMAgent {
        LLMAgent {
            llm: self.clone(),
            system_prompt: system_prompt.into(),
        }
    }
}

impl LLMAgent {
    pub async fn call(&self, prompt: &str) -> Result<String> {
        with_agent!(self, agent, Ok(agent.prompt(prompt).await?))
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

    pub async fn schema<T>(&self, prompt: &str) -> Result<T>
    where
        T: schemars::JsonSchema + serde::de::DeserializeOwned + Send + 'static,
    {
        with_agent!(self, agent, Ok(agent.prompt_typed(prompt).await?))
    }
}
