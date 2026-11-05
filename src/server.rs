use std::sync::Arc;

use anyhow::Context;
use axum::extract::{Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde_json::json;

use crate::api_keys::ApiKeys;
use crate::codex::tokens::TokenManager;
use crate::config::ServeArgs;

#[derive(Clone)]
struct AppState {
    api_keys: Arc<ApiKeys>,
    tokens: Arc<TokenManager>,
}

/// Name of the authenticated client, attached to each `/v1` request.
#[derive(Clone)]
#[allow(dead_code)] // read by handlers once they are implemented
pub struct ClientName(pub String);

pub async fn run(args: ServeArgs) -> anyhow::Result<()> {
    let api_keys = ApiKeys::load(args.api_keys.as_deref(), args.api_keys_file.as_deref())?;
    let tokens = Arc::new(TokenManager::new(&args.data_dir)?);
    tokens.spawn_maintenance();
    let state = AppState {
        api_keys: Arc::new(api_keys),
        tokens,
    };

    let v1 = Router::new()
        .route("/models", get(not_implemented))
        .route("/chat/completions", post(not_implemented))
        .route("/images/generations", post(not_implemented))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));

    let app = Router::new()
        .route("/healthz", get(healthz))
        .nest("/v1", v1)
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(args.listen)
        .await
        .with_context(|| format!("binding {}", args.listen))?;
    tracing::info!(listen = %args.listen, "codex-relay listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

async fn require_api_key(State(state): State<AppState>, mut req: Request, next: Next) -> Response {
    let client = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .and_then(|key| state.api_keys.verify(key.trim()))
        .map(str::to_owned);

    match client {
        Some(name) => {
            req.extensions_mut().insert(ClientName(name));
            next.run(req).await
        }
        None => openai_error(
            StatusCode::UNAUTHORIZED,
            "invalid_api_key",
            "Invalid or missing API key",
        ),
    }
}

/// 200 when the relay can serve requests, 503 when it needs a (new) ChatGPT sign-in.
async fn healthz(State(state): State<AppState>) -> Response {
    let (healthy, token) = state.tokens.status();
    let status = if healthy { "ok" } else { "unavailable" };
    let code = if healthy {
        StatusCode::OK
    } else {
        StatusCode::SERVICE_UNAVAILABLE
    };
    let body = json!({ "status": status, "version": env!("CARGO_PKG_VERSION"), "chatgpt": token });
    (code, Json(body)).into_response()
}

async fn not_implemented() -> Response {
    openai_error(
        StatusCode::NOT_IMPLEMENTED,
        "not_implemented",
        "This endpoint is not implemented yet",
    )
}

/// Error body in the shape OpenAI clients expect.
pub fn openai_error(status: StatusCode, code: &str, message: &str) -> Response {
    let body = json!({
        "error": { "message": message, "type": "invalid_request_error", "code": code }
    });
    (status, Json(body)).into_response()
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {},
        () = terminate => {},
    }
    tracing::info!("shutting down");
}
