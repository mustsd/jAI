//! OpenAI-compatible provider.
//!
//! One implementation covers every provider that speaks the OpenAI
//! `/chat/completions` + `/models` contract (DeepSeek, OpenAI, OpenRouter,
//! Groq, Ollama, LM Studio, self-hosted gateways, ...).
//!
//! Responsibilities are deliberately narrow: build the request, stream the
//! response, map failures onto [`jai_core::Error`]. Configuration, policy,
//! storage and presentation live elsewhere.

pub mod error;
pub mod sse;
pub mod wire;

pub use error::{extract_message, map_status, map_transport};

use futures_util::StreamExt;
use jai_core::{BoxFuture, CancelFlag, ChatEvent, ChatRequest, Error, ModelInfo, Provider, Result};
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use sse::{SseDecoder, SseEvent};
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::mpsc;

const USER_AGENT: &str = concat!("jAI/", env!("CARGO_PKG_VERSION"));
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
const MODELS_TIMEOUT: Duration = Duration::from_secs(30);

/// A provider speaking the OpenAI-compatible HTTP API.
pub struct OpenAiCompatProvider {
    name: String,
    base_url: String,
    api_key: Option<String>,
    extra_headers: HeaderMap,
    client: reqwest::Client,
}

/// Never print the API key, not even in debug logs.
impl std::fmt::Debug for OpenAiCompatProvider {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatProvider")
            .field("name", &self.name)
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "<set>"))
            .finish()
    }
}

impl OpenAiCompatProvider {
    /// Create a provider for `base_url` (e.g. `https://api.deepseek.com`).
    pub fn new(
        name: impl Into<String>,
        base_url: impl Into<String>,
        api_key: Option<String>,
    ) -> Result<Self> {
        let name = name.into();
        let base_url = normalize_base_url(&base_url.into());
        if base_url.is_empty() {
            return Err(Error::config(format!(
                "provider `{name}` needs a base URL (set base_url in config.toml or pass --base-url)"
            )));
        }
        let client = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|err| Error::internal(format!("could not build HTTP client: {err}")))?;
        Ok(Self {
            name,
            base_url,
            api_key: api_key.filter(|key| !key.trim().is_empty()),
            extra_headers: HeaderMap::new(),
            client,
        })
    }

    /// Add static headers (from `config.toml`).
    pub fn with_headers(mut self, headers: &[(String, String)]) -> Result<Self> {
        for (name, value) in headers {
            let header_name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::config(format!("invalid header name `{name}`")))?;
            let header_value = HeaderValue::from_str(value)
                .map_err(|_| Error::config(format!("invalid value for header `{name}`")))?;
            self.extra_headers.insert(header_name, header_value);
        }
        Ok(self)
    }

    /// Inject a pre-built client (tests, proxies).
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    pub fn has_api_key(&self) -> bool {
        self.api_key.is_some()
    }

    /// Full URL for an API path such as `chat/completions`.
    pub fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn request_headers(&self) -> Result<HeaderMap> {
        let mut headers = self.extra_headers.clone();
        headers.insert(
            reqwest::header::ACCEPT,
            HeaderValue::from_static("text/event-stream"),
        );
        // Never let a proxy buffer or compress the event stream.
        headers.insert(
            reqwest::header::ACCEPT_ENCODING,
            HeaderValue::from_static("identity"),
        );
        if let Some(key) = &self.api_key {
            let value = HeaderValue::from_str(&format!("Bearer {key}")).map_err(|_| {
                Error::config(format!(
                    "the API key for `{}` contains characters that are not valid in an HTTP header",
                    self.name
                ))
            })?;
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
        Ok(headers)
    }
}

/// Trim a trailing slash and an accidentally pasted endpoint path.
fn normalize_base_url(input: &str) -> String {
    let mut url = input.trim().trim_end_matches('/').to_string();
    for suffix in ["/chat/completions", "/completions"] {
        if let Some(stripped) = url.strip_suffix(suffix) {
            url = stripped.trim_end_matches('/').to_string();
        }
    }
    url
}

impl Provider for OpenAiCompatProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn base_url(&self) -> &str {
        &self.base_url
    }

    fn chat_stream(
        &self,
        request: ChatRequest,
    ) -> BoxFuture<'_, Result<mpsc::Receiver<Result<ChatEvent>>>> {
        let url = self.endpoint("chat/completions");
        let headers = match self.request_headers() {
            Ok(headers) => headers,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let body = wire::build_chat_body(&request);
        let client = self.client.clone();
        let name = self.name.clone();
        let cancel = request.cancel.clone();

        Box::pin(async move {
            let response = client
                .post(&url)
                .headers(headers)
                .json(&body)
                .send()
                .await
                .map_err(|err| map_transport(&name, err))?;

            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default();
                return Err(map_status(&name, status, &body));
            }

            let (tx, rx) = mpsc::channel(64);
            tokio::spawn(async move {
                pump_stream(name, response, tx, cancel).await;
            });
            Ok(rx)
        })
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>>> {
        let url = self.endpoint("models");
        let headers = match self.request_headers() {
            Ok(headers) => headers,
            Err(err) => return Box::pin(async move { Err(err) }),
        };
        let client = self.client.clone();
        let name = self.name.clone();

        Box::pin(async move {
            let response = client
                .get(&url)
                .headers(headers)
                .timeout(MODELS_TIMEOUT)
                .send()
                .await
                .map_err(|err| map_transport(&name, err))?;
            let status = response.status();
            let body = response
                .text()
                .await
                .map_err(|err| map_transport(&name, err))?;
            if !status.is_success() {
                return Err(map_status(&name, status, &body));
            }
            let models: wire::ModelsResponse = serde_json::from_str(&body).map_err(|err| {
                Error::api(&name, 0, format!("could not parse /models response: {err}"))
            })?;
            Ok(models
                .data
                .into_iter()
                .map(|entry| ModelInfo {
                    id: entry.id,
                    owned_by: entry.owned_by,
                })
                .collect())
        })
    }
}

/// Read the response body, decode SSE frames, and forward chat events.
async fn pump_stream(
    name: String,
    response: reqwest::Response,
    tx: mpsc::Sender<Result<ChatEvent>>,
    cancel: Option<CancelFlag>,
) {
    let mut decoder = SseDecoder::new();
    let mut body = response.bytes_stream();
    let mut finished = false;

    while let Some(chunk) = body.next().await {
        if is_cancelled(&cancel) {
            let _ = tx.send(Err(Error::Cancelled)).await;
            return;
        }
        match chunk {
            Ok(bytes) => {
                for event in decoder.push(&bytes) {
                    if event.is_done() {
                        finished = true;
                        break;
                    }
                    if forward(&name, &event, &tx).await.is_err() {
                        return;
                    }
                }
            }
            Err(err) => {
                let _ = tx.send(Err(map_transport(&name, err))).await;
                return;
            }
        }
        if finished {
            break;
        }
    }

    // The server may end the stream without a `[DONE]` sentinel.
    if !finished {
        if let Some(event) = decoder.finish() {
            if !event.is_done() {
                let _ = forward(&name, &event, &tx).await;
            }
        }
    }
    let _ = tx.send(Ok(ChatEvent::Finished)).await;
}

/// Returns `Err(())` when the consumer went away.
async fn forward(
    name: &str,
    event: &SseEvent,
    tx: &mpsc::Sender<Result<ChatEvent>>,
) -> std::result::Result<(), ()> {
    let data = event.data.trim();
    if data.is_empty() {
        return Ok(());
    }
    match serde_json::from_str::<wire::StreamChunk>(data) {
        Ok(chunk) => match chunk.into_events() {
            Ok(events) => {
                for chat_event in events {
                    if tx.send(Ok(chat_event)).await.is_err() {
                        return Err(());
                    }
                }
                Ok(())
            }
            Err(message) => {
                let _ = tx.send(Err(Error::api(name, 0, message))).await;
                Err(())
            }
        },
        Err(err) => {
            let error = Error::api(name, 0, format!("could not parse stream frame: {err}"));
            if tx.send(Err(error)).await.is_err() {
                Err(())
            } else {
                Ok(())
            }
        }
    }
}

fn is_cancelled(cancel: &Option<CancelFlag>) -> bool {
    cancel
        .as_ref()
        .map(|flag| flag.load(Ordering::Relaxed))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::StatusCode;

    #[test]
    fn base_urls_are_normalized() {
        assert_eq!(
            normalize_base_url("https://api.deepseek.com/"),
            "https://api.deepseek.com"
        );
        assert_eq!(
            normalize_base_url("https://api.openai.com/v1/chat/completions"),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            normalize_base_url("http://127.0.0.1:11434/v1/"),
            "http://127.0.0.1:11434/v1"
        );
        assert_eq!(
            normalize_base_url("  https://x.test/v1  "),
            "https://x.test/v1"
        );
    }

    #[test]
    fn endpoints_are_appended_once() {
        let provider =
            OpenAiCompatProvider::new("deepseek", "https://api.deepseek.com", None).unwrap();
        assert_eq!(
            provider.endpoint("chat/completions"),
            "https://api.deepseek.com/chat/completions"
        );
        assert_eq!(
            provider.endpoint("/models"),
            "https://api.deepseek.com/models"
        );
    }

    #[test]
    fn empty_base_url_is_a_configuration_error() {
        let error = OpenAiCompatProvider::new("custom", "   ", None).unwrap_err();
        assert_eq!(error.exit_code(), 2);
        assert!(error.to_string().contains("base URL"));
    }

    #[test]
    fn the_api_key_is_sent_only_when_present() {
        let with_key =
            OpenAiCompatProvider::new("deepseek", "https://x.test", Some("secret".into()))
                .unwrap()
                .request_headers()
                .unwrap();
        assert_eq!(with_key["authorization"], "Bearer secret");
        assert_eq!(with_key["accept"], "text/event-stream");
        assert_eq!(with_key["accept-encoding"], "identity");

        let without_key =
            OpenAiCompatProvider::new("ollama", "http://127.0.0.1:11434/v1", None).unwrap();
        assert!(!without_key.has_api_key());
        assert!(
            without_key
                .request_headers()
                .unwrap()
                .get("authorization")
                .is_none()
        );
    }

    #[test]
    fn blank_api_keys_are_ignored() {
        let provider =
            OpenAiCompatProvider::new("ollama", "http://127.0.0.1:11434/v1", Some("   ".into()))
                .unwrap();
        assert!(!provider.has_api_key());
    }

    #[test]
    fn custom_headers_are_validated() {
        let provider =
            OpenAiCompatProvider::new("openrouter", "https://openrouter.ai/api/v1", None)
                .unwrap()
                .with_headers(&[("x-title".to_string(), "jAI".to_string())])
                .unwrap();
        assert_eq!(provider.request_headers().unwrap()["x-title"], "jAI");

        let error = OpenAiCompatProvider::new("x", "https://x.test", None)
            .unwrap()
            .with_headers(&[("bad header".to_string(), "v".to_string())])
            .unwrap_err();
        assert_eq!(error.exit_code(), 2);
    }

    #[test]
    fn debug_output_hides_the_api_key() {
        let provider =
            OpenAiCompatProvider::new("deepseek", "https://x.test", Some("sk-secret".into()))
                .unwrap();
        let debug = format!("{provider:?}");
        assert!(!debug.contains("sk-secret"), "{debug}");
        assert!(debug.contains("<set>"), "{debug}");
    }

    #[test]
    fn status_mapping_is_exposed_for_other_crates() {
        assert_eq!(
            map_status("deepseek", StatusCode::UNAUTHORIZED, "{}").exit_code(),
            3
        );
    }
}
