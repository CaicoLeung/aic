use anyhow::Result;
use futures::StreamExt;
use rig::agent::{MultiTurnStreamItem, Text};
use rig::client::CompletionClient;
use rig::completion::Prompt;
use rig::streaming::{StreamedAssistantContent, StreamingPrompt};
use std::env;
use std::io::Write;

const DEFAULT_PROVIDER: &str = "openai";

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

macro_rules! stream_provider {
    ($lock:expr, $output:expr, $client:expr, $model:expr, $prompt:expr) => {{
        let agent = $client.agent($model).build();
        let mut stream = agent.stream_prompt($prompt).await;
        while let Some(item) = stream.next().await {
            match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(StreamedAssistantContent::Text(
                    Text { text },
                ))) => {
                    write!($lock, "{text}")?;
                    $output.push_str(&text);
                }
                Ok(_) => {}
                Err(e) => anyhow::bail!("Stream error: {e}"),
            }
        }
    }};
}

impl LLM {
    pub fn from_env() -> Self {
        let provider_name =
            env::var("LLM_BACKEND").unwrap_or_else(|_| DEFAULT_PROVIDER.to_string());
        let provider = Provider::from_name(&provider_name);

        let api_key = env::var("LLM_API_KEY")
            .or_else(|_| {
                provider
                    .env_key()
                    .map(env::var)
                    .unwrap_or(Ok(String::new()))
            })
            .unwrap_or_default();

        let model = env::var("LLM_MODEL").unwrap_or_else(|_| provider.default_model().to_string());

        Self {
            provider,
            model,
            api_key,
        }
    }

    pub async fn call(prompt: &str) -> Result<String> {
        let llm = Self::default();
        match llm.provider {
            Provider::OpenAI => {
                let client = rig::providers::openai::Client::new(&llm.api_key)?;
                let agent = client.agent(&llm.model).build();
                Ok(agent.prompt(prompt).await?)
            }
            Provider::Anthropic => {
                let client = rig::providers::anthropic::Client::new(&llm.api_key)?;
                let agent = client.agent(&llm.model).build();
                Ok(agent.prompt(prompt).await?)
            }
            Provider::Gemini => {
                let client = rig::providers::gemini::Client::new(&llm.api_key)?;
                let agent = client.agent(&llm.model).build();
                Ok(agent.prompt(prompt).await?)
            }
            Provider::DeepSeek => {
                let client = rig::providers::deepseek::Client::new(&llm.api_key)?;
                let agent = client.agent(&llm.model).build();
                Ok(agent.prompt(prompt).await?)
            }
            Provider::Groq => {
                let client = rig::providers::groq::Client::new(&llm.api_key)?;
                let agent = client.agent(&llm.model).build();
                Ok(agent.prompt(prompt).await?)
            }
            Provider::Ollama => {
                let client =
                    rig::providers::ollama::Client::new("http://localhost:11434".to_string())?;
                let agent = client.agent(&llm.model).build();
                Ok(agent.prompt(prompt).await?)
            }
        }
    }

    pub async fn stream(prompt: &str) -> Result<String> {
        let llm = Self::default();
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        let mut output = String::new();

        match llm.provider {
            Provider::OpenAI => {
                let client = rig::providers::openai::Client::new(&llm.api_key)?;
                stream_provider!(lock, output, client, &llm.model, prompt);
            }
            Provider::Anthropic => {
                let client = rig::providers::anthropic::Client::new(&llm.api_key)?;
                stream_provider!(lock, output, client, &llm.model, prompt);
            }
            Provider::Gemini => {
                let client = rig::providers::gemini::Client::new(&llm.api_key)?;
                stream_provider!(lock, output, client, &llm.model, prompt);
            }
            Provider::DeepSeek => {
                let client = rig::providers::deepseek::Client::new(&llm.api_key)?;
                stream_provider!(lock, output, client, &llm.model, prompt);
            }
            Provider::Groq => {
                let client = rig::providers::groq::Client::new(&llm.api_key)?;
                stream_provider!(lock, output, client, &llm.model, prompt);
            }
            Provider::Ollama => {
                let client =
                    rig::providers::ollama::Client::new("http://localhost:11434".to_string())?;
                stream_provider!(lock, output, client, &llm.model, prompt);
            }
        }

        writeln!(lock)?;
        lock.flush()?;
        Ok(output)
    }
}
