//! Live model listing for the setup wizard (ADR 0019): ask the provider what
//! models exist instead of curating them in the registry. rig's
//! [`ModelListingClient`] serves the eight providers that expose a lister;
//! Groq, xAI, Perplexity and Together declare `ModelListing = Nothing` in rig
//! and are fetched directly over the OpenAI-compatible `GET {base}/models`
//! convention. Every failure mode — network, auth, timeout, unknown endpoint —
//! surfaces as `Err`, and the wizard's single caller degrades to required
//! free-text input without showing the error (ADR 0019 §4).

use anyhow::{Context, Result};
use std::time::Duration;

use crate::llm::{OLLAMA_DEFAULT_BASE_URL, Provider};
use rig::client::ModelListingClient;

/// Hard cap for one listing call (ADR 0019): covers rig's internal pagination
/// (Anthropic's cursor, Gemini's page tokens) so a slow provider can never
/// stall the wizard longer than this.
const FETCH_TIMEOUT: Duration = Duration::from_secs(5);

/// List the model ids `provider` offers, authenticated with `api_key` (empty
/// for keyless providers), against `base_url` when the provider takes one.
/// The list is deduplicated and sorted so the wizard's picker and cache hold
/// one canonical shape.
pub(crate) async fn list_models(
    provider: Provider,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Vec<String>> {
    let fetched = tokio::time::timeout(
        FETCH_TIMEOUT,
        list_models_uncapped(provider, api_key, base_url),
    )
    .await
    .context("model listing timed out")??;
    let mut models = fetched;
    models.sort();
    models.dedup();
    Ok(models)
}

async fn list_models_uncapped(
    provider: Provider,
    api_key: &str,
    base_url: Option<&str>,
) -> Result<Vec<String>> {
    // The same clients `with_agent!` builds, so listing and Run traffic hit
    // the same endpoint (custom base URLs honored where a provider takes one).
    match provider {
        Provider::OpenAI => {
            let client = rig::providers::openai::Client::new(api_key)?;
            Ok(client
                .list_models()
                .await?
                .data
                .into_iter()
                .map(|m| m.id)
                .collect())
        }
        Provider::Anthropic => {
            let client = rig::providers::anthropic::Client::new(api_key)?;
            Ok(client
                .list_models()
                .await?
                .data
                .into_iter()
                .map(|m| m.id)
                .collect())
        }
        Provider::Gemini => {
            let client = rig::providers::gemini::Client::new(api_key)?;
            Ok(client
                .list_models()
                .await?
                .data
                .into_iter()
                .map(|m| m.id)
                .collect())
        }
        Provider::DeepSeek => {
            let client = rig::providers::deepseek::Client::new(api_key)?;
            Ok(client
                .list_models()
                .await?
                .data
                .into_iter()
                .map(|m| m.id)
                .collect())
        }
        Provider::Mistral => {
            let client = rig::providers::mistral::Client::new(api_key)?;
            Ok(client
                .list_models()
                .await?
                .data
                .into_iter()
                .map(|m| m.id)
                .collect())
        }
        Provider::OpenRouter => {
            let client = rig::providers::openrouter::Client::new(api_key)?;
            Ok(client
                .list_models()
                .await?
                .data
                .into_iter()
                .map(|m| m.id)
                .collect())
        }
        Provider::Ollama => {
            let url = base_url.unwrap_or(OLLAMA_DEFAULT_BASE_URL);
            let key = if api_key.is_empty() {
                rig::providers::ollama::OllamaApiKey::default()
            } else {
                rig::providers::ollama::OllamaApiKey::from(api_key.to_string())
            };
            let client = rig::providers::ollama::Client::builder()
                .api_key(key)
                .base_url(url)
                .build()?;
            Ok(client
                .list_models()
                .await?
                .data
                .into_iter()
                .map(|m| m.id)
                .collect())
        }
        Provider::OpenAiCompatible => {
            let base = base_url.context("the openai-compatible provider requires a base URL")?;
            // Keyless local servers: a placeholder satisfies rig's builder,
            // exactly as the Run path in `with_agent!`.
            let key = if api_key.is_empty() {
                String::from("no-key")
            } else {
                api_key.to_string()
            };
            let client = rig::providers::openai::Client::builder()
                .api_key(&key)
                .base_url(base)
                .build()?;
            Ok(client
                .list_models()
                .await?
                .data
                .into_iter()
                .map(|m| m.id)
                .collect())
        }
        // rig declares no listing capability for these four; their catalogs
        // follow the OpenAI `GET {base}/models` shape (ADR 0019 §2). A
        // provider without a real endpoint 404s here and the wizard degrades.
        Provider::Groq => direct_models("https://api.groq.com/openai/v1/models", api_key).await,
        Provider::Xai => direct_models("https://api.x.ai/v1/models", api_key).await,
        Provider::Perplexity => direct_models("https://api.perplexity.ai/models", api_key).await,
        Provider::Together => direct_models("https://api.together.xyz/v1/models", api_key).await,
    }
}

/// One OpenAI-shaped `GET {url}` listing: `{"data":[{"id":…},…]}`, Bearer
/// auth when a key exists. Kept struct-local — only the direct arms parse
/// this shape; rig arms get typed `ModelList`s.
async fn direct_models(url: &str, api_key: &str) -> Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Listing {
        #[serde(default)]
        data: Vec<Id>,
    }
    #[derive(serde::Deserialize)]
    struct Id {
        id: String,
    }
    let client = reqwest::Client::new();
    let mut request = client.get(url);
    if !api_key.is_empty() {
        request = request.bearer_auth(api_key);
    }
    let listing: Listing = request
        .send()
        .await
        .with_context(|| format!("GET {url} failed"))?
        .error_for_status()
        .with_context(|| format!("GET {url} returned an error status"))?
        .json()
        .await
        .with_context(|| format!("GET {url} returned an unparseable body"))?;
    Ok(listing.data.into_iter().map(|m| m.id).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A server that accepts the TCP connection but never responds must not
    /// stall `list_models` past the 5 s cap — the wizard's Busy screen rides
    /// on this bound. Regression for the fetch-hang report: the timer has to
    /// fire even while the inner reqwest future parks on the socket read.
    #[test]
    fn list_models_caps_a_never_responding_server() {
        use std::io::Read;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for conn in listener.incoming() {
                // hold the connection open, never write a byte
                let mut conn = conn.unwrap();
                let mut buf = [0u8; 512];
                let _ = conn.read(&mut buf);
                std::thread::park();
            }
        });
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let start = std::time::Instant::now();
        let url = format!("http://{addr}/v1");
        let out = rt.block_on(list_models(
            Provider::OpenAiCompatible,
            "sk-test",
            Some(&url),
        ));
        assert!(out.is_err(), "hang must surface as Err, got {out:?}");
        assert!(
            start.elapsed() < std::time::Duration::from_secs(8),
            "5 s cap must bound the call, took {:?}",
            start.elapsed()
        );
    }
}
