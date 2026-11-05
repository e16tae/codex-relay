//! HTTP client for the ChatGPT Codex backend (`chatgpt.com/backend-api/codex`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::tokens::{TokenError, TokenManager};

const MODELS_TTL: Duration = Duration::from_secs(60 * 60);

pub struct CodexBackend {
    http: reqwest::Client,
    base_url: String,
    client_version: String,
    tokens: Arc<TokenManager>,
    models: Mutex<Option<(Instant, Arc<Vec<UpstreamModel>>)>>,
}

#[derive(Clone, Deserialize)]
pub struct UpstreamModel {
    pub slug: String,
    #[serde(default)]
    pub visibility: Option<String>,
    #[serde(default)]
    pub context_window: Option<u64>,
}

#[derive(Debug)]
pub enum UpstreamError {
    Token(TokenError),
    Http {
        status: StatusCode,
        message: String,
        code: Option<String>,
    },
    Network(String),
}

impl From<TokenError> for UpstreamError {
    fn from(err: TokenError) -> Self {
        Self::Token(err)
    }
}

impl CodexBackend {
    pub fn new(
        base_url: String,
        client_version: String,
        tokens: Arc<TokenManager>,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(concat!("codex-relay/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            // Reasoning can pause the stream for a while; no overall timeout.
            .read_timeout(Duration::from_secs(300))
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_owned(),
            client_version,
            tokens,
            models: Mutex::new(None),
        })
    }

    async fn request(&self, method: Method, path: &str) -> Result<RequestBuilder, UpstreamError> {
        let access = self.tokens.access().await?;
        Ok(self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .bearer_auth(access.access_token)
            .header("chatgpt-account-id", access.account_id)
            .header("OpenAI-Beta", "responses=experimental")
            .header("originator", "codex_cli_rs"))
    }

    /// Models available to this account, cached for an hour.
    pub async fn models(&self) -> Result<Arc<Vec<UpstreamModel>>, UpstreamError> {
        let mut cache = self.models.lock().await;
        if let Some((at, models)) = cache.as_ref() {
            if at.elapsed() < MODELS_TTL {
                return Ok(Arc::clone(models));
            }
        }
        #[derive(Deserialize)]
        struct ModelsResponse {
            models: Vec<UpstreamModel>,
        }
        let path = format!("/models?client_version={}", self.client_version);
        let response = send(self.request(Method::GET, &path).await?).await?;
        let parsed: ModelsResponse = response
            .json()
            .await
            .map_err(|err| UpstreamError::Network(format!("invalid models response: {err}")))?;
        let models = Arc::new(parsed.models);
        *cache = Some((Instant::now(), Arc::clone(&models)));
        Ok(models)
    }

    /// Starts a streaming `/responses` call. The body must already follow the backend's rules.
    pub async fn responses(&self, body: &Value) -> Result<reqwest::Response, UpstreamError> {
        let request = self
            .request(Method::POST, "/responses")
            .await?
            .header(reqwest::header::ACCEPT, "text/event-stream")
            .json(body);
        send(request).await
    }
}

async fn send(request: RequestBuilder) -> Result<reqwest::Response, UpstreamError> {
    let response = request
        .send()
        .await
        .map_err(|err| UpstreamError::Network(err.to_string()))?;
    if response.status().is_success() {
        return Ok(response);
    }
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let (message, code) = parse_error_body(&text);
    Err(UpstreamError::Http {
        status,
        message,
        code,
    })
}

/// Understands `{"detail": "..."}` and `{"error": {"message", "code"|"type"}}`.
fn parse_error_body(text: &str) -> (String, Option<String>) {
    let fallback = || text.chars().take(500).collect::<String>();
    let Ok(body) = serde_json::from_str::<Value>(text) else {
        return (fallback(), None);
    };
    if let Some(detail) = body.get("detail") {
        let message = detail
            .as_str()
            .map(str::to_owned)
            .unwrap_or_else(|| detail.to_string());
        return (message, None);
    }
    if let Some(error) = body.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(fallback);
        let code = ["code", "type"]
            .iter()
            .find_map(|k| error.get(*k).and_then(Value::as_str))
            .map(str::to_owned);
        return (message, code);
    }
    (fallback(), None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_backend_errors() {
        assert_eq!(
            parse_error_body(r#"{"detail":"Stream must be set to true"}"#),
            ("Stream must be set to true".into(), None)
        );
        assert_eq!(
            parse_error_body(r#"{"error":{"message":"slow down","type":"usage_limit_reached"}}"#),
            ("slow down".into(), Some("usage_limit_reached".into()))
        );
        assert_eq!(parse_error_body("oops"), ("oops".into(), None));
    }
}
