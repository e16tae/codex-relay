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
/// Same policy as the Codex CLI: up to 4 retries, 200 ms doubling each time.
const MAX_RETRIES: u32 = 4;
const RETRY_BASE_DELAY: Duration = Duration::from_millis(200);

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
        let response = send_with_retries(request, MAX_RETRIES, RETRY_BASE_DELAY)
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

/// Server errors and transport failures are retried, like the Codex CLI does.
/// They arrive before any output, so a retry can't duplicate a streamed answer.
/// `429` is not retried: it means a usage limit, which a retry won't lift.
fn retryable(outcome: &Result<reqwest::Response, reqwest::Error>) -> bool {
    match outcome {
        Ok(response) => response.status().is_server_error(),
        Err(err) => err.is_connect() || err.is_timeout() || err.is_request(),
    }
}

/// `base` doubled for each earlier retry, with ±10 % jitter so parallel
/// requests don't retry in lockstep.
fn backoff(base: Duration, retry: u32) -> Duration {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.subsec_nanos());
    let jitter = 0.9 + f64::from(nanos % 1000) / 5000.0;
    base.saturating_mul(1 << retry.min(16)).mul_f64(jitter)
}

/// Sends a request, retrying failures up to `max_retries` times.
/// Returns the last outcome, which can still carry an error status.
async fn send_with_retries(
    request: RequestBuilder,
    max_retries: u32,
    base_delay: Duration,
) -> Result<reqwest::Response, reqwest::Error> {
    for retry in 0.. {
        // JSON bodies can always be cloned; anything else is sent once.
        let Some(attempt) = request.try_clone() else {
            return request.send().await;
        };
        let outcome = attempt.send().await;
        if retry >= max_retries || !retryable(&outcome) {
            return outcome;
        }
        let failure = match &outcome {
            Ok(response) => response.status().to_string(),
            Err(err) => err.to_string(),
        };
        let delay = backoff(base_delay, retry);
        tracing::warn!(%failure, "upstream request failed; retry {} of {max_retries} in {delay:?}", retry + 1);
        tokio::time::sleep(delay).await;
    }
    unreachable!("the loop returns once retries run out")
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
        send_with_retries(request, MAX_RETRIES, Duration::ZERO)
            .await
            .unwrap()
            .status()
    }

    #[tokio::test]
    async fn retries_gateway_errors() {
        let (url, hits) = fake_upstream(&[502, 504, 500, 503, 200]).await;
        assert_eq!(send_to(&url).await, StatusCode::OK);
        assert_eq!(hits.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn gives_up_after_the_last_retry() {
        let (url, hits) = fake_upstream(&[503]).await;
        assert_eq!(send_to(&url).await, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(hits.load(Ordering::SeqCst), 5);
    }

    #[test]
    fn backs_off_exponentially() {
        let base = Duration::from_millis(200);
        for (retry, ms) in [(0, 200), (1, 400), (2, 800), (3, 1600)] {
            let delay = backoff(base, retry).as_millis();
            assert!(delay >= ms * 9 / 10 && delay <= ms * 11 / 10, "{delay}");
        }
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
