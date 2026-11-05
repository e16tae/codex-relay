//! `POST /v1/chat/completions` on top of the Codex Responses backend.
//!
//! The backend only streams, so non-streaming requests are collected here.

mod request;
mod translate;

use std::convert::Infallible;
use std::time::Instant;

use axum::Extension;
use axum::body::Body;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Json, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use self::request::{ChatRequest, to_responses_body};
use self::translate::{Output, Translator, connection_lost, delta_for, ended_early};
use super::error::ApiError;
use crate::server::{AppState, ClientName};
use crate::sse::SseParser;

struct RequestLog {
    client: String,
    model: String,
    stream: bool,
    started: Instant,
}

impl RequestLog {
    fn finish(&self, translator: Option<&Translator>, error: Option<&ApiError>) {
        let ms = self.started.elapsed().as_millis() as u64;
        let usage = translator.and_then(|t| t.usage.as_ref());
        let tokens = |k: &str| usage.and_then(|u| u.get(k)).and_then(Value::as_u64);
        match error {
            None => tracing::info!(
                client = %self.client, model = %self.model, stream = self.stream, ms,
                prompt_tokens = tokens("prompt_tokens"),
                completion_tokens = tokens("completion_tokens"),
                tool_calls = translator.map_or(0, Translator::tool_call_count),
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
                    outcome = Err(connection_lost(err));
                    break;
                }
            };
            for event in parser.push(&chunk) {
                for output in translator.on_data(&event.data) {
                    match output {
                        Output::Done => {
                            outcome = Ok(());
                            break 'read;
                        }
                        Output::Error(err) => {
                            outcome = Err(err);
                            break 'read;
                        }
                        output => {
                            let Some(mut delta) = delta_for(&output) else {
                                continue;
                            };
                            if !sent_role {
                                delta["role"] = "assistant".into();
                                sent_role = true;
                            }
                            if tx.send(sse(&translator.chunk(delta, None))).await.is_err() {
                                tracing::info!(client = %log.client, "client disconnected mid-stream");
                                return;
                            }
                        }
                    }
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
                log.finish(Some(&translator), None);
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
    let result = 'read: loop {
        let Some(chunk) = body.next().await else {
            break Err(ended_early());
        };
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(err) => break Err(connection_lost(err)),
        };
        for event in parser.push(&chunk) {
            for output in translator.on_data(&event.data) {
                match output {
                    Output::Done => break 'read Ok(()),
                    Output::Error(err) => break 'read Err(err),
                    _ => {}
                }
            }
        }
    };
    match result {
        Ok(()) => {
            log.finish(Some(&translator), None);
            Ok(Json(translator.completion()).into_response())
        }
        Err(err) => {
            log.finish(None, Some(&err));
            Err(err)
        }
    }
}
