use axum::Json;
use axum::extract::State;
use serde_json::{Value, json};

use super::error::ApiError;
use crate::server::AppState;

/// `GET /v1/models`: the models the signed-in account can use.
pub async fn list(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    let models = state.backend.models().await?;
    let data: Vec<Value> = models
        .iter()
        .filter(|m| m.visibility.as_deref() != Some("hide"))
        .map(|m| {
            json!({
                "id": m.slug,
                "object": "model",
                "created": 0,
                "owned_by": "openai",
                "context_window": m.context_window,
            })
        })
        .collect();
    Ok(Json(json!({ "object": "list", "data": data })))
}
