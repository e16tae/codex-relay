//! `POST /v1/chat/completions` on top of the Codex Responses backend.
//!
//! The backend only streams, so non-streaming requests are collected here.

use std::convert::Infallible;
use std::time::Instant;

use axum::Extension;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use reqwest::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use super::error::ApiError;
use crate::codex::credentials::now_unix;
use crate::server::{AppState, ClientName};
use crate::sse::SseParser;

// ----------------------------------------------------------------------------
// Request
// ----------------------------------------------------------------------------

/// The subset of the Chat Completions request that maps onto the backend.
/// Sampling knobs the backend rejects (`temperature`, `top_p`, `stop`, penalties, ...)
/// are accepted and dropped.
#[derive(Deserialize)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub stream_options: Option<StreamOptions>,
    #[serde(default)]
    pub max_tokens: Option<u64>,
    #[serde(default)]
    pub max_completion_tokens: Option<u64>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub response_format: Option<Value>,
    #[serde(default)]
    pub tools: Option<Vec<Value>>,
    #[serde(default)]
    pub n: Option<u32>,
}

#[derive(Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

/// Unknown fields (and `null`s such as `"name": null`) are ignored.
#[derive(Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<MessageContent>,
}

#[derive(Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    Text(String),
    Parts(Vec<ContentPart>),
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    ImageUrl {
        image_url: ImageUrl,
    },
    #[serde(other)]
    Unsupported,
}

#[derive(Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(default)]
    pub detail: Option<String>,
}

/// Builds the backend `/responses` body. The backend requires `stream: true` and
/// `store: false`, and does not allow `system` messages (they become `developer`).
pub fn to_responses_body(
    req: &ChatRequest,
    default_effort: Option<&str>,
) -> Result<Value, ApiError> {
    if req.n.unwrap_or(1) != 1 {
        return Err(ApiError::invalid_request("n other than 1 is not supported"));
    }
    if req.tools.as_ref().is_some_and(|t| !t.is_empty()) {
        return Err(ApiError::invalid_request(
            "tool calls are not supported yet",
        ));
    }
    if req.model.trim().is_empty() {
        return Err(ApiError::invalid_request("model is required"));
    }
    let mut input = Vec::new();
    for (i, message) in req.messages.iter().enumerate() {
        let item = convert_message(message)
            .map_err(|err| ApiError::invalid_request(format!("messages[{i}]: {err}")))?;
        input.extend(item);
    }
    if input.is_empty() {
        return Err(ApiError::invalid_request(
            "messages must contain some content",
        ));
    }

    let mut body = json!({
        "model": req.model,
        "input": input,
        "stream": true,
        "store": false,
        "include": [],
    });
    if let Some(effort) = req.reasoning_effort.as_deref().or(default_effort) {
        body["reasoning"] = json!({ "effort": effort });
    }
    if let Some(max) = req.max_completion_tokens.or(req.max_tokens) {
        body["max_output_tokens"] = max.into();
    }
    if let Some(format) = req.response_format.as_ref() {
        if let Some(format) = convert_response_format(format)? {
            body["text"] = json!({ "format": format });
        }
    }
    Ok(body)
}

fn convert_message(message: &ChatMessage) -> Result<Option<Value>, String> {
    let (role, text_type) = match message.role.as_str() {
        "system" | "developer" => ("developer", "input_text"),
        "user" => ("user", "input_text"),
        "assistant" => ("assistant", "output_text"),
        "tool" | "function" => return Err("tool messages are not supported yet".into()),
        other => return Err(format!("unsupported role `{other}`")),
    };
    let mut content = Vec::new();
    match &message.content {
        None => {}
        Some(MessageContent::Text(text)) => {
            if !text.is_empty() {
                content.push(json!({ "type": text_type, "text": text }));
            }
        }
        Some(MessageContent::Parts(parts)) => {
            for part in parts {
                match part {
                    ContentPart::Text { text } => {
                        content.push(json!({ "type": text_type, "text": text }));
                    }
                    ContentPart::ImageUrl { image_url } if role == "user" => {
                        let mut image =
                            json!({ "type": "input_image", "image_url": image_url.url });
                        if let Some(detail) = &image_url.detail {
                            image["detail"] = detail.clone().into();
                        }
                        content.push(image);
                    }
                    ContentPart::ImageUrl { .. } => {
                        return Err("images are only supported in user messages".into());
                    }
                    ContentPart::Unsupported => return Err("unsupported content part type".into()),
                }
            }
        }
    }
    if content.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        json!({ "type": "message", "role": role, "content": content }),
    ))
}

fn convert_response_format(format: &Value) -> Result<Option<Value>, ApiError> {
    match format.get("type").and_then(Value::as_str) {
        None | Some("text") => Ok(None),
        Some("json_object") => Ok(Some(json!({ "type": "json_object" }))),
        Some("json_schema") => {
            let spec = format.get("json_schema").ok_or_else(|| {
                ApiError::invalid_request("response_format.json_schema is required")
            })?;
            let mut out = json!({
                "type": "json_schema",
                "name": spec.get("name").cloned().unwrap_or_else(|| "response".into()),
                "schema": spec.get("schema").cloned().unwrap_or_else(|| json!({})),
            });
            for key in ["strict", "description"] {
                if let Some(value) = spec.get(key) {
                    out[key] = value.clone();
                }
            }
            Ok(Some(out))
        }
        Some(other) => Err(ApiError::invalid_request(format!(
            "unsupported response_format type `{other}`"
        ))),
    }
}

// ----------------------------------------------------------------------------
// Response translation
// ----------------------------------------------------------------------------

pub enum Output {
    Text(String),
    Done,
    Error(ApiError),
}

/// Turns backend Responses events into Chat Completions pieces.
pub struct Translator {
    id: String,
    model: String,
    created: i64,
    finish_reason: Option<&'static str>,
    usage: Option<Value>,
}

impl Translator {
    pub fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", now_unix()),
            model: model.to_owned(),
            created: now_unix(),
            finish_reason: None,
            usage: None,
        }
    }

    pub fn on_data(&mut self, data: &str) -> Option<Output> {
        let event: Value = serde_json::from_str(data).ok()?;
        match event.get("type").and_then(Value::as_str)? {
            "response.created" => {
                if let Some(id) = event.pointer("/response/id").and_then(Value::as_str) {
                    self.id = format!("chatcmpl-{}", id.trim_start_matches("resp_"));
                }
                None
            }
            "response.output_text.delta" => event
                .get("delta")
                .and_then(Value::as_str)
                .filter(|d| !d.is_empty())
                .map(|d| Output::Text(d.to_owned())),
            "response.completed" => {
                self.finish(&event, "stop");
                Some(Output::Done)
            }
            "response.incomplete" => {
                let reason = event
                    .pointer("/response/incomplete_details/reason")
                    .and_then(Value::as_str);
                let finish = if reason == Some("content_filter") {
                    "content_filter"
                } else {
                    "length"
                };
                self.finish(&event, finish);
                Some(Output::Done)
            }
            "response.failed" => Some(Output::Error(stream_error(
                event.pointer("/response/error"),
            ))),
            "error" => Some(Output::Error(stream_error(Some(
                event.get("error").unwrap_or(&event),
            )))),
            _ => None,
        }
    }

    fn finish(&mut self, event: &Value, reason: &'static str) {
        self.finish_reason = Some(reason);
        self.usage = event.pointer("/response/usage").map(chat_usage);
    }

    fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
        })
    }

    fn usage_chunk(&self, usage: &Value) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": usage,
        })
    }

    fn completion(&self, text: String) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": text, "refusal": null },
                "finish_reason": self.finish_reason.unwrap_or("stop"),
                "logprobs": null,
            }],
            "usage": self.usage,
        })
    }
}

fn stream_error(error: Option<&Value>) -> ApiError {
    let field = |k: &str| error.and_then(|e| e.get(k)).and_then(Value::as_str);
    let message = field("message").unwrap_or("the ChatGPT backend reported an error");
    let code = field("code").or(field("type")).unwrap_or("upstream_error");
    if code.contains("rate_limit") || code.contains("usage_limit") {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit_error",
            code,
            message,
        )
    } else {
        ApiError::new(StatusCode::BAD_GATEWAY, "server_error", code, message)
    }
}

fn ended_early() -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "server_error",
        "upstream_incomplete",
        "the ChatGPT backend closed the stream before the response completed",
    )
}

fn chat_usage(usage: &Value) -> Value {
    let num = |p: &str| usage.pointer(p).and_then(Value::as_u64).unwrap_or(0);
    let (input, output) = (num("/input_tokens"), num("/output_tokens"));
    json!({
        "prompt_tokens": input,
        "completion_tokens": output,
        "total_tokens": usage.get("total_tokens").and_then(Value::as_u64).unwrap_or(input + output),
        "prompt_tokens_details": { "cached_tokens": num("/input_tokens_details/cached_tokens") },
        "completion_tokens_details": { "reasoning_tokens": num("/output_tokens_details/reasoning_tokens") },
    })
}

// ----------------------------------------------------------------------------
// Handler
// ----------------------------------------------------------------------------

struct RequestLog {
    client: String,
    model: String,
    stream: bool,
    started: Instant,
}

impl RequestLog {
    fn finish(&self, usage: Option<&Value>, error: Option<&ApiError>) {
        let ms = self.started.elapsed().as_millis() as u64;
        let tokens = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64);
        match error {
            None => tracing::info!(
                client = %self.client, model = %self.model, stream = self.stream, ms,
                prompt_tokens = tokens("prompt_tokens"),
                completion_tokens = tokens("completion_tokens"),
                "chat completion"
            ),
            Some(err) => tracing::warn!(
                client = %self.client, model = %self.model, stream = self.stream, ms,
                code = %err.code, error = %err.message,
                "chat completion failed"
            ),
        }
    }
}

pub async fn completions(
    State(state): State<AppState>,
    Extension(client): Extension<ClientName>,
    request: Result<Json<ChatRequest>, JsonRejection>,
) -> Result<Response, ApiError> {
    let Json(req) = request.map_err(|err| ApiError::invalid_request(err.body_text()))?;
    let body = to_responses_body(&req, state.default_effort.as_deref())?;
    let log = RequestLog {
        client: client.0,
        model: req.model.clone(),
        stream: req.stream,
        started: Instant::now(),
    };
    let upstream = match state.backend.responses(&body).await {
        Ok(upstream) => upstream,
        Err(err) => {
            let err = ApiError::from(err);
            log.finish(None, Some(&err));
            return Err(err);
        }
    };
    let translator = Translator::new(&req.model);
    if req.stream {
        let include_usage = req.stream_options.is_some_and(|o| o.include_usage);
        Ok(stream_response(upstream, translator, include_usage, log))
    } else {
        collect_response(upstream, translator, log).await
    }
}

fn sse(value: &Value) -> Result<String, Infallible> {
    Ok(format!("data: {value}\n\n"))
}

fn stream_response(
    upstream: reqwest::Response,
    mut translator: Translator,
    include_usage: bool,
    log: RequestLog,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<String, Infallible>>(64);
    tokio::spawn(async move {
        // The role goes into the first delta, sent once the backend's response ID is known.
        let mut sent_role = false;
        let mut parser = SseParser::default();
        let mut body = upstream.bytes_stream();
        let mut outcome = Err(ended_early());
        'read: while let Some(chunk) = body.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(err) => {
                    outcome = Err(ApiError::new(
                        StatusCode::BAD_GATEWAY,
                        "server_error",
                        "upstream_unreachable",
                        format!("lost connection to the ChatGPT backend: {err}"),
                    ));
                    break;
                }
            };
            for event in parser.push(&chunk) {
                match translator.on_data(&event.data) {
                    Some(Output::Text(text)) => {
                        let delta = if sent_role {
                            json!({ "content": text })
                        } else {
                            json!({ "role": "assistant", "content": text })
                        };
                        sent_role = true;
                        let piece = translator.chunk(delta, None);
                        if tx.send(sse(&piece)).await.is_err() {
                            tracing::info!(client = %log.client, "client disconnected mid-stream");
                            return;
                        }
                    }
                    Some(Output::Done) => {
                        outcome = Ok(());
                        break 'read;
                    }
                    Some(Output::Error(err)) => {
                        outcome = Err(err);
                        break 'read;
                    }
                    None => {}
                }
            }
        }
        match outcome {
            Ok(()) => {
                let delta = if sent_role {
                    json!({})
                } else {
                    json!({ "role": "assistant", "content": "" })
                };
                let last = translator.chunk(delta, translator.finish_reason);
                let _ = tx.send(sse(&last)).await;
                if include_usage {
                    if let Some(usage) = &translator.usage {
                        let _ = tx.send(sse(&translator.usage_chunk(usage))).await;
                    }
                }
                log.finish(translator.usage.as_ref(), None);
            }
            Err(err) => {
                let _ = tx.send(sse(&err.body())).await;
                log.finish(None, Some(&err));
            }
        }
        let _ = tx.send(Ok("data: [DONE]\n\n".to_owned())).await;
    });

    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-cache"),
            (header::HeaderName::from_static("x-accel-buffering"), "no"),
        ],
        Body::from_stream(ReceiverStream::new(rx)),
    )
        .into_response()
}

async fn collect_response(
    upstream: reqwest::Response,
    mut translator: Translator,
    log: RequestLog,
) -> Result<Response, ApiError> {
    let mut parser = SseParser::default();
    let mut body = upstream.bytes_stream();
    let mut text = String::new();
    let result = 'read: loop {
        let Some(chunk) = body.next().await else {
            break Err(ended_early());
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) => {
                break Err(ApiError::new(
                    StatusCode::BAD_GATEWAY,
                    "server_error",
                    "upstream_unreachable",
                    format!("lost connection to the ChatGPT backend: {err}"),
                ));
            }
        };
        for event in parser.push(&chunk) {
            match translator.on_data(&event.data) {
                Some(Output::Text(piece)) => text.push_str(&piece),
                Some(Output::Done) => break 'read Ok(()),
                Some(Output::Error(err)) => break 'read Err(err),
                None => {}
            }
        }
    };
    match result {
        Ok(()) => {
            log.finish(translator.usage.as_ref(), None);
            Ok(Json(translator.completion(text)).into_response())
        }
        Err(err) => {
            log.finish(None, Some(&err));
            Err(err)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(value: Value) -> ChatRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn converts_messages_and_options() {
        let req = request(json!({
            "model": "gpt-6-luna",
            "temperature": 0.3,
            "max_tokens": 100,
            "response_format": { "type": "json_object" },
            "messages": [
                { "role": "system", "content": "Be brief.", "name": null },
                { "role": "user", "content": [
                    { "type": "text", "text": "What is this?" },
                    { "type": "image_url", "image_url": { "url": "data:image/png;base64,AAAA", "detail": "low" } }
                ]},
                { "role": "assistant", "content": "A cat." },
                { "role": "assistant", "content": null },
                { "role": "user", "content": "Thanks" }
            ]
        }));
        let body = to_responses_body(&req, Some("low")).unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert!(body.get("temperature").is_none());
        assert_eq!(body["max_output_tokens"], 100);
        assert_eq!(body["reasoning"]["effort"], "low");
        assert_eq!(body["text"]["format"]["type"], "json_object");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4, "empty assistant message is dropped");
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["content"][1]["type"], "input_image");
        assert_eq!(input[1]["content"][1]["detail"], "low");
        assert_eq!(input[2]["content"][0]["type"], "output_text");
    }

    #[test]
    fn request_effort_overrides_default() {
        let req = request(json!({
            "model": "m", "reasoning_effort": "high",
            "messages": [{ "role": "user", "content": "hi" }]
        }));
        assert_eq!(
            to_responses_body(&req, Some("low")).unwrap()["reasoning"]["effort"],
            "high"
        );
    }

    #[test]
    fn converts_json_schema_format() {
        let req = request(json!({
            "model": "m",
            "messages": [{ "role": "user", "content": "hi" }],
            "response_format": { "type": "json_schema", "json_schema": {
                "name": "answer", "strict": true, "schema": { "type": "object" } } }
        }));
        let format = &to_responses_body(&req, None).unwrap()["text"]["format"];
        assert_eq!(format["type"], "json_schema");
        assert_eq!(format["name"], "answer");
        assert_eq!(format["strict"], true);
        assert_eq!(format["schema"]["type"], "object");
    }

    #[test]
    fn rejects_unsupported_requests() {
        for value in [
            json!({ "model": "m", "n": 2, "messages": [{ "role": "user", "content": "hi" }] }),
            json!({ "model": "m", "messages": [] }),
            json!({ "model": "m", "messages": [{ "role": "tool", "content": "x" }] }),
            json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }],
                    "tools": [{ "type": "function" }] }),
        ] {
            assert!(to_responses_body(&request(value), None).is_err());
        }
    }

    #[test]
    fn translates_backend_events() {
        let mut t = Translator::new("gpt-6-luna");
        assert!(
            t.on_data(r#"{"type":"response.created","response":{"id":"resp_abc"}}"#)
                .is_none()
        );
        assert!(matches!(
            t.on_data(r#"{"type":"response.output_text.delta","delta":"안녕"}"#),
            Some(Output::Text(s)) if s == "안녕"
        ));
        assert!(t.on_data(r#"{"type":"response.metadata"}"#).is_none());
        assert!(matches!(
            t.on_data(r#"{"type":"response.completed","response":{"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":15}}}"#),
            Some(Output::Done)
        ));
        let done = t.completion("안녕".into());
        assert_eq!(done["id"], "chatcmpl-abc");
        assert_eq!(done["choices"][0]["finish_reason"], "stop");
        assert_eq!(done["usage"]["prompt_tokens"], 10);
        assert_eq!(done["usage"]["prompt_tokens_details"]["cached_tokens"], 4);
        assert_eq!(
            done["usage"]["completion_tokens_details"]["reasoning_tokens"],
            2
        );
    }

    #[test]
    fn maps_incomplete_and_failures() {
        let mut t = Translator::new("m");
        t.on_data(r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#);
        assert_eq!(t.finish_reason, Some("length"));
        match t.on_data(r#"{"type":"response.failed","response":{"error":{"code":"usage_limit_reached","message":"limit"}}}"#) {
            Some(Output::Error(err)) => assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS),
            _ => panic!("expected an error"),
        }
    }
}
