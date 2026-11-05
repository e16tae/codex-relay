//! `POST /v1/images/generations` on top of the Codex image backend.

use std::time::Instant;

use axum::Extension;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, State};
use futures_util::future::try_join_all;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use super::error::ApiError;
use crate::codex::credentials::now_unix;
use crate::server::{AppState, ClientName};

/// Upper bound for `n`; each image is a separate backend call against the subscription quota.
pub const MAX_IMAGES: u32 = 4;

#[derive(Deserialize)]
pub struct ImageRequest {
    pub prompt: String,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub n: Option<u32>,
    #[serde(default)]
    pub size: Option<String>,
    #[serde(default)]
    pub quality: Option<String>,
    #[serde(default)]
    pub background: Option<String>,
    #[serde(default)]
    pub response_format: Option<String>,
}

/// Builds the backend body. Any requested model name is replaced by the configured
/// image model, so clients defaulting to `dall-e-3` or `gpt-image-1` still work.
fn backend_body(req: &ImageRequest, image_model: &str) -> Result<Value, ApiError> {
    if req.prompt.trim().is_empty() {
        return Err(ApiError::invalid_request("prompt is required"));
    }
    if let Some(format) = req.response_format.as_deref() {
        if format != "b64_json" {
            return Err(ApiError::invalid_request(
                "only response_format \"b64_json\" is supported",
            ));
        }
    }
    let mut body = json!({ "model": image_model, "prompt": req.prompt });
    if let Some(size) = req.size.as_deref().filter(|s| valid_size(s)) {
        body["size"] = size.into();
    }
    // DALL·E 3 style values map onto the gpt-image scale.
    match req.quality.as_deref() {
        Some(q @ ("low" | "medium" | "high" | "auto")) => body["quality"] = q.into(),
        Some("hd") => body["quality"] = "high".into(),
        _ => {}
    }
    if let Some(bg @ ("transparent" | "opaque" | "auto")) = req.background.as_deref() {
        body["background"] = bg.into();
    }
    Ok(body)
}

fn valid_size(size: &str) -> bool {
    size == "auto"
        || size
            .split_once('x')
            .is_some_and(|(w, h)| w.parse::<u32>().is_ok() && h.parse::<u32>().is_ok())
}

/// Merges per-image backend responses into one OpenAI Images response.
fn merge(responses: Vec<Value>) -> Value {
    let mut data = Vec::new();
    let mut usage = Map::new();
    for response in &responses {
        for item in response["data"].as_array().into_iter().flatten() {
            if let Some(b64) = item.get("b64_json") {
                data.push(json!({ "b64_json": b64 }));
            }
        }
        for key in ["input_tokens", "output_tokens", "total_tokens"] {
            let add = response
                .pointer(&format!("/usage/{key}"))
                .and_then(Value::as_u64);
            if let Some(add) = add {
                let sum = usage.get(key).and_then(Value::as_u64).unwrap_or(0) + add;
                usage.insert(key.to_owned(), sum.into());
            }
        }
    }
    let first = responses.first().cloned().unwrap_or_default();
    let mut out = json!({ "created": now_unix(), "data": data });
    for key in ["size", "quality", "background", "output_format"] {
        if let Some(value) = first.get(key).filter(|v| !v.is_null()) {
            out[key] = value.clone();
        }
    }
    if !usage.is_empty() {
        out["usage"] = usage.into();
    }
    out
}

pub async fn generations(
    State(state): State<AppState>,
    Extension(client): Extension<ClientName>,
    request: Result<Json<ImageRequest>, JsonRejection>,
) -> Result<Json<Value>, ApiError> {
    let Json(req) = request.map_err(|err| ApiError::invalid_request(err.body_text()))?;
    let n = req.n.unwrap_or(1);
    if !(1..=MAX_IMAGES).contains(&n) {
        return Err(ApiError::invalid_request(format!(
            "n must be between 1 and {MAX_IMAGES}"
        )));
    }
    let body = backend_body(&req, &state.image_model)?;
    let started = Instant::now();

    // The backend returns one image per call, so `n` images means `n` parallel calls.
    let result = try_join_all((0..n).map(|_| state.backend.generate_image(&body))).await;
    let ms = started.elapsed().as_millis() as u64;
    let requested_model = req.model.as_deref().unwrap_or("-");
    match result {
        Ok(responses) => {
            let merged = merge(responses);
            let size = merged["size"].as_str().unwrap_or("?").to_owned();
            let output_tokens = merged
                .pointer("/usage/output_tokens")
                .and_then(|v| v.as_u64());
            tracing::info!(
                client = %client.0, requested_model, n, ms, size, output_tokens,
                "image generation"
            );
            Ok(Json(merged))
        }
        Err(err) => {
            let err = ApiError::from(err);
            tracing::warn!(
                client = %client.0, requested_model, n, ms,
                code = %err.code, error = %err.message,
                "image generation failed"
            );
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> ImageRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn builds_backend_body() {
        let body = backend_body(
            &request(json!({
                "prompt": "a fox", "model": "dall-e-3", "size": "1024x1024",
                "quality": "hd", "background": "transparent", "n": 2
            })),
            "gpt-image-2",
        )
        .unwrap();
        assert_eq!(body["model"], "gpt-image-2");
        assert_eq!(body["size"], "1024x1024");
        assert_eq!(body["quality"], "high");
        assert_eq!(body["background"], "transparent");
        assert!(body.get("n").is_none());

        let body = backend_body(
            &request(json!({ "prompt": "x", "size": "huge", "quality": "standard" })),
            "gpt-image-2",
        )
        .unwrap();
        assert!(body.get("size").is_none());
        assert!(body.get("quality").is_none());
    }

    #[test]
    fn rejects_bad_requests() {
        assert!(backend_body(&request(json!({ "prompt": " " })), "m").is_err());
        assert!(
            backend_body(
                &request(json!({ "prompt": "x", "response_format": "url" })),
                "m"
            )
            .is_err()
        );
    }

    #[test]
    fn merges_responses() {
        let one = |b64: &str| {
            json!({
                "created": 1, "size": "1254x1254", "quality": "low", "background": "opaque",
                "output_format": "png", "safety_identifier": null,
                "data": [{ "b64_json": b64, "generation_id": "g" }],
                "usage": { "input_tokens": 10, "output_tokens": 500, "total_tokens": 510 }
            })
        };
        let merged = merge(vec![one("AAA"), one("BBB")]);
        assert_eq!(merged["data"].as_array().unwrap().len(), 2);
        assert_eq!(merged["data"][1], json!({ "b64_json": "BBB" }));
        assert_eq!(merged["size"], "1254x1254");
        assert_eq!(merged["usage"]["output_tokens"], 1000);
        assert!(merged.get("safety_identifier").is_none());
    }
}
