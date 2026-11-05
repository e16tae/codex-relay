use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::codex::backend::UpstreamError;
use crate::codex::tokens::TokenError;

/// An error in the shape OpenAI clients expect.
#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub kind: &'static str,
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(
        status: StatusCode,
        kind: &'static str,
        code: &str,
        message: impl Into<String>,
    ) -> Self {
        Self {
            status,
            kind,
            code: code.to_owned(),
            message: message.into(),
        }
    }

    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self::new(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "invalid_request",
            message,
        )
    }

    pub fn body(&self) -> Value {
        json!({ "error": { "message": self.message, "type": self.kind, "code": self.code } })
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body())).into_response()
    }
}

impl From<UpstreamError> for ApiError {
    fn from(err: UpstreamError) -> Self {
        match err {
            UpstreamError::Token(err @ (TokenError::SignedOut | TokenError::Dead(_))) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "relay_sign_in_required",
                err.to_string(),
            ),
            UpstreamError::Token(err @ TokenError::Unavailable(_)) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_error",
                "relay_token_unavailable",
                err.to_string(),
            ),
            UpstreamError::Http {
                status,
                message,
                code,
            } => {
                let code = code.unwrap_or_default();
                match status {
                    StatusCode::TOO_MANY_REQUESTS => Self::new(
                        status,
                        "rate_limit_error",
                        if code.is_empty() {
                            "rate_limit_exceeded"
                        } else {
                            &code
                        },
                        message,
                    ),
                    StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Self::new(
                        StatusCode::BAD_GATEWAY,
                        "server_error",
                        "upstream_auth_failed",
                        format!("ChatGPT rejected the relay's credentials ({status}): {message}"),
                    ),
                    s if s.is_client_error() => Self::new(
                        s,
                        "invalid_request_error",
                        if code.is_empty() {
                            "upstream_rejected"
                        } else {
                            &code
                        },
                        message,
                    ),
                    _ => Self::new(
                        StatusCode::BAD_GATEWAY,
                        "server_error",
                        "upstream_error",
                        format!("ChatGPT backend error ({status}): {message}"),
                    ),
                }
            }
            UpstreamError::Network(message) => Self::new(
                StatusCode::BAD_GATEWAY,
                "server_error",
                "upstream_unreachable",
                format!("could not reach the ChatGPT backend: {message}"),
            ),
        }
    }
}
