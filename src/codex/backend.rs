//! HTTP client for the ChatGPT Codex backend (`chatgpt.com/backend-api/codex`).

use std::sync::Arc;
use std::time::{Duration, Instant};

use reqwest::{Method, RequestBuilder, StatusCode};
use serde::Deserialize;
use serde_json::Value;
use tokio::sync::Mutex;

use super::quota::{QuotaTracker, reset_hint};
use super::tokens::{TokenError, TokenManager};

const MODELS_TTL: Duration = Duration::from_secs(60 * 60);
/// Pauses before each retry of a gateway error; its length is the number of retries.
const RETRY_DELAYS: [Duration; 2] = [Duration::from_millis(500), Duration::from_millis(1500)];

pub struct CodexBackend {
    http: reqwest::Client,
    base_url: String,
    client_version: String,
    tokens: Arc<TokenManager>,
    models: Mutex<Option<(Instant, Arc<Vec<UpstreamModel>>)>>,
    pub quota: QuotaTracker,
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
            quota: QuotaTracker::default(),
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
        let response = self.send(self.request(Method::GET, &path).await?).await?;
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
        self.send(request).await
    }

    /// Generates one image. The backend ignores `n`, `size` and `quality` and picks
    /// the size itself; it honours `background`.
    pub async fn generate_image(&self, body: &Value) -> Result<Value, UpstreamError> {
        let request = self
            .request(Method::POST, "/images/generations")
            .await?
            .json(body);
        self.send(request)
            .await?
            .json()
            .await
            .map_err(|err| UpstreamError::Network(format!("invalid image response: {err}")))
    }

    /// Sends a request, records the usage-limit headers, and turns failures into errors.
    async fn send(&self, request: RequestBuilder) -> Result<reqwest::Response, UpstreamError> {
        let response = send_with_retries(request, &RETRY_DELAYS)
            .await
            .map_err(|err| UpstreamError::Network(err.to_string()))?;
        self.quota.observe(response.headers());
        if response.status().is_success() {
            return Ok(response);
        }
        let status = response.status();
        let hint = (status == StatusCode::TOO_MANY_REQUESTS)
            .then(|| reset_hint(response.headers()))
            .flatten();
        let text = response.text().await.unwrap_or_default();
        let (mut message, code) = parse_error_body(&text);
        if let Some(hint) = hint {
            message = format!("{message} ({hint})");
        }
        Err(UpstreamError::Http {
            status,
            message,
            code,
        })
    }
}

/// Gateway errors in front of the backend. They arrive before any output, so a
/// retry can't duplicate or cut off a streamed answer.
fn retryable(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::BAD_GATEWAY | StatusCode::SERVICE_UNAVAILABLE | StatusCode::GATEWAY_TIMEOUT
    )
}

/// Sends a request, retrying gateway errors and failed connections once per delay.
/// Returns the last response, which can still carry an error status.
async fn send_with_retries(
    request: RequestBuilder,
    delays: &[Duration],
) -> Result<reqwest::Response, reqwest::Error> {
    let mut delays = delays.iter();
    loop {
        // JSON bodies can always be cloned; anything else is sent once.
        let Some(attempt) = request.try_clone() else {
            return request.send().await;
        };
        let outcome = attempt.send().await;
        let failure = match &outcome {
            Ok(response) if retryable(response.status()) => response.status().to_string(),
            Err(err) if err.is_connect() => err.to_string(),
            _ => return outcome,
        };
        let Some(delay) = delays.next() else {
            return outcome;
        };
        tracing::warn!(%failure, "upstream request failed; retrying in {delay:?}");
        tokio::time::sleep(*delay).await;
    }
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
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Serves the given statuses in order (the last one repeats) and counts hits.
    async fn fake_upstream(statuses: &'static [u16]) -> (String, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        let app = axum::Router::new().route(
            "/",
            axum::routing::post(move || {
                let n = counter.fetch_add(1, Ordering::SeqCst);
                let code = statuses[n.min(statuses.len() - 1)];
                async move { axum::http::StatusCode::from_u16(code).unwrap() }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (url, hits)
    }

    async fn send_to(url: &str) -> StatusCode {
        let request = reqwest::Client::new()
            .post(url)
            .json(&serde_json::json!({}));
        send_with_retries(request, &[Duration::ZERO; 2])
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn retries_gateway_errors() {
        let (url, hits) = fake_upstream(&[502, 504, 200]).await;
        assert_eq!(send_to(&url).await, StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn gives_up_after_the_last_retry() {
        let (url, hits) = fake_upstream(&[503]).await;
        assert_eq!(send_to(&url).await, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn does_not_retry_other_errors() {
        let (url, hits) = fake_upstream(&[429]).await;
        assert_eq!(send_to(&url).await, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

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
