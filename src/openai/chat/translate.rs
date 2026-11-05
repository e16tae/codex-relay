//! Codex Responses stream events → Chat Completions pieces.

use reqwest::StatusCode;
use serde_json::{Value, json};

use crate::codex::credentials::now_unix;
use crate::openai::error::ApiError;

pub enum Output {
    Text(String),
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    ToolCallArgs {
        index: usize,
        delta: String,
    },
    Done,
    Error(ApiError),
}

struct ToolCall {
    /// Backend output item ID (`fc_...`), used to match argument deltas.
    item_id: String,
    /// Call ID (`call_...`) the client echoes back in the `tool` message.
    id: String,
    name: String,
    arguments: String,
}

pub struct Translator {
    id: String,
    model: String,
    created: i64,
    pub finish_reason: Option<&'static str>,
    pub usage: Option<Value>,
    text: String,
    tool_calls: Vec<ToolCall>,
}

impl Translator {
    pub fn new(model: &str) -> Self {
        Self {
            id: format!("chatcmpl-{}", now_unix()),
            model: model.to_owned(),
            created: now_unix(),
            finish_reason: None,
            usage: None,
            text: String::new(),
            tool_calls: Vec::new(),
        }
    }

    pub fn tool_call_count(&self) -> usize {
        self.tool_calls.len()
    }

    pub fn on_data(&mut self, data: &str) -> Vec<Output> {
        let Ok(event) = serde_json::from_str::<Value>(data) else {
            return Vec::new();
        };
        let str_at = |p: &str| event.pointer(p).and_then(Value::as_str);
        match str_at("/type").unwrap_or_default() {
            "response.created" => {
                if let Some(id) = str_at("/response/id") {
                    self.id = format!("chatcmpl-{}", id.trim_start_matches("resp_"));
                }
                Vec::new()
            }
            "response.output_text.delta" => match str_at("/delta").filter(|d| !d.is_empty()) {
                Some(delta) => {
                    self.text.push_str(delta);
                    vec![Output::Text(delta.to_owned())]
                }
                None => Vec::new(),
            },
            "response.output_item.added" => match function_call_item(&event) {
                Some(item) => vec![self.start_tool_call(item)],
                None => Vec::new(),
            },
            "response.function_call_arguments.delta" => {
                let index = self.find_tool_call(str_at("/item_id"));
                match (index, str_at("/delta").filter(|d| !d.is_empty())) {
                    (Some(index), Some(delta)) => {
                        self.tool_calls[index].arguments.push_str(delta);
                        vec![Output::ToolCallArgs {
                            index,
                            delta: delta.to_owned(),
                        }]
                    }
                    _ => Vec::new(),
                }
            }
            "response.output_item.done" => match function_call_item(&event) {
                Some(item) => self.finish_tool_call(item),
                None => Vec::new(),
            },
            "response.completed" => {
                let reason = if self.tool_calls.is_empty() {
                    "stop"
                } else {
                    "tool_calls"
                };
                self.finish(&event, reason);
                vec![Output::Done]
            }
            "response.incomplete" => {
                let finish =
                    if str_at("/response/incomplete_details/reason") == Some("content_filter") {
                        "content_filter"
                    } else {
                        "length"
                    };
                self.finish(&event, finish);
                vec![Output::Done]
            }
            "response.failed" => vec![Output::Error(stream_error(
                event.pointer("/response/error"),
            ))],
            "error" => vec![Output::Error(stream_error(Some(
                event.get("error").unwrap_or(&event),
            )))],
            _ => Vec::new(),
        }
    }

    fn start_tool_call(&mut self, item: &Value) -> Output {
        let field = |k: &str| item.get(k).and_then(Value::as_str).unwrap_or_default();
        let item_id = field("id").to_owned();
        let id = match field("call_id") {
            "" => item_id.clone(),
            call_id => call_id.to_owned(),
        };
        let name = field("name").to_owned();
        let index = self.tool_calls.len();
        self.tool_calls.push(ToolCall {
            item_id,
            id: id.clone(),
            name: name.clone(),
            arguments: String::new(),
        });
        Output::ToolCallStart { index, id, name }
    }

    /// Emits the full arguments if they were not streamed as deltas.
    fn finish_tool_call(&mut self, item: &Value) -> Vec<Output> {
        let mut out = Vec::new();
        let index = match self.find_tool_call(item.get("id").and_then(Value::as_str)) {
            Some(index) => index,
            None => {
                out.push(self.start_tool_call(item));
                self.tool_calls.len() - 1
            }
        };
        let full = item
            .get("arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let call = &mut self.tool_calls[index];
        if call.arguments.is_empty() && !full.is_empty() {
            call.arguments = full.to_owned();
            out.push(Output::ToolCallArgs {
                index,
                delta: full.to_owned(),
            });
        }
        out
    }

    fn find_tool_call(&self, item_id: Option<&str>) -> Option<usize> {
        let item_id = item_id.filter(|id| !id.is_empty())?;
        self.tool_calls.iter().position(|c| c.item_id == item_id)
    }

    fn finish(&mut self, event: &Value, reason: &'static str) {
        self.finish_reason = Some(reason);
        self.usage = event.pointer("/response/usage").map(chat_usage);
    }

    pub fn chunk(&self, delta: Value, finish_reason: Option<&str>) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish_reason }],
        })
    }

    pub fn usage_chunk(&self, usage: &Value) -> Value {
        json!({
            "id": self.id,
            "object": "chat.completion.chunk",
            "created": self.created,
            "model": self.model,
            "choices": [],
            "usage": usage,
        })
    }

    /// The full non-streaming response.
    pub fn completion(&self) -> Value {
        let mut message = json!({ "role": "assistant", "content": self.text, "refusal": null });
        if !self.tool_calls.is_empty() {
            if self.text.is_empty() {
                message["content"] = Value::Null;
            }
            message["tool_calls"] = self
                .tool_calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "type": "function",
                        "function": { "name": c.name, "arguments": c.arguments },
                    })
                })
                .collect::<Vec<_>>()
                .into();
        }
        json!({
            "id": self.id,
            "object": "chat.completion",
            "created": self.created,
            "model": self.model,
            "choices": [{
                "index": 0,
                "message": message,
                "finish_reason": self.finish_reason.unwrap_or("stop"),
                "logprobs": null,
            }],
            "usage": self.usage,
        })
    }
}

/// The streaming delta for one piece of output, if it has one.
pub fn delta_for(output: &Output) -> Option<Value> {
    match output {
        Output::Text(text) => Some(json!({ "content": text })),
        Output::ToolCallStart { index, id, name } => Some(json!({
            "tool_calls": [{
                "index": index,
                "id": id,
                "type": "function",
                "function": { "name": name, "arguments": "" },
            }]
        })),
        Output::ToolCallArgs { index, delta } => Some(json!({
            "tool_calls": [{ "index": index, "function": { "arguments": delta } }]
        })),
        Output::Done | Output::Error(_) => None,
    }
}

fn function_call_item(event: &Value) -> Option<&Value> {
    event
        .get("item")
        .filter(|item| item.get("type").and_then(Value::as_str) == Some("function_call"))
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

pub fn ended_early() -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "server_error",
        "upstream_incomplete",
        "the ChatGPT backend closed the stream before the response completed",
    )
}

pub fn connection_lost(err: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        StatusCode::BAD_GATEWAY,
        "server_error",
        "upstream_unreachable",
        format!("lost connection to the ChatGPT backend: {err}"),
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

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(t: &mut Translator, events: &[Value]) -> Vec<Output> {
        events
            .iter()
            .flat_map(|e| t.on_data(&e.to_string()))
            .collect()
    }

    #[test]
    fn translates_text_and_usage() {
        let mut t = Translator::new("gpt-6-luna");
        let out = feed(
            &mut t,
            &[
                json!({"type":"response.created","response":{"id":"resp_abc"}}),
                json!({"type":"response.output_text.delta","delta":"안녕"}),
                json!({"type":"response.metadata"}),
                json!({"type":"response.completed","response":{"usage":{"input_tokens":10,"input_tokens_details":{"cached_tokens":4},"output_tokens":5,"output_tokens_details":{"reasoning_tokens":2},"total_tokens":15}}}),
            ],
        );
        assert!(matches!(&out[0], Output::Text(s) if s == "안녕"));
        assert!(matches!(out[1], Output::Done));
        let done = t.completion();
        assert_eq!(done["id"], "chatcmpl-abc");
        assert_eq!(done["choices"][0]["message"]["content"], "안녕");
        assert_eq!(done["choices"][0]["finish_reason"], "stop");
        assert!(done["choices"][0]["message"].get("tool_calls").is_none());
        assert_eq!(done["usage"]["prompt_tokens"], 10);
        assert_eq!(done["usage"]["prompt_tokens_details"]["cached_tokens"], 4);
        assert_eq!(
            done["usage"]["completion_tokens_details"]["reasoning_tokens"],
            2
        );
    }

    #[test]
    fn translates_parallel_tool_calls_without_deltas() {
        // The shape the backend actually sends: added → done with full arguments.
        let mut t = Translator::new("m");
        let call = |n: u8, city: &str| {
            json!({"type":"function_call","id":format!("fc_{n}"),"call_id":format!("call_{n}"),
                   "name":"get_weather","arguments":format!("{{\"city\":\"{city}\"}}")})
        };
        let added = |n: u8| {
            let mut item = call(n, "");
            item["arguments"] = "".into();
            json!({"type":"response.output_item.added","item":item})
        };
        let events = [
            added(1),
            json!({"type":"response.output_item.done","item":call(1,"Seoul")}),
            added(2),
            json!({"type":"response.output_item.done","item":call(2,"Busan")}),
            json!({"type":"response.completed","response":{}}),
        ];
        let out = feed(&mut t, &events);
        let deltas: Vec<Value> = out.iter().filter_map(delta_for).collect();
        assert_eq!(deltas.len(), 4);
        assert_eq!(deltas[0]["tool_calls"][0]["id"], "call_1");
        assert_eq!(
            deltas[0]["tool_calls"][0]["function"]["name"],
            "get_weather"
        );
        assert_eq!(
            deltas[1]["tool_calls"][0]["function"]["arguments"],
            "{\"city\":\"Seoul\"}"
        );
        assert_eq!(deltas[2]["tool_calls"][0]["index"], 1);
        let done = t.completion();
        let message = &done["choices"][0]["message"];
        assert_eq!(done["choices"][0]["finish_reason"], "tool_calls");
        assert!(message["content"].is_null());
        assert_eq!(message["tool_calls"][1]["id"], "call_2");
        assert_eq!(
            message["tool_calls"][1]["function"]["arguments"],
            "{\"city\":\"Busan\"}"
        );
    }

    #[test]
    fn streams_argument_deltas_once() {
        let mut t = Translator::new("m");
        let out = feed(
            &mut t,
            &[
                json!({"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"f","arguments":""}}),
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"a\":"}),
                json!({"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"1}"}),
                json!({"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"f","arguments":"{\"a\":1}"}}),
            ],
        );
        // start + two deltas; the done event must not repeat the arguments.
        assert_eq!(out.len(), 3);
        assert_eq!(
            t.completion()["choices"][0]["message"]["tool_calls"][0]["function"]["arguments"],
            "{\"a\":1}"
        );
    }

    #[test]
    fn maps_incomplete_and_failures() {
        let mut t = Translator::new("m");
        t.on_data(r#"{"type":"response.incomplete","response":{"incomplete_details":{"reason":"max_output_tokens"}}}"#);
        assert_eq!(t.finish_reason, Some("length"));
        let out = t.on_data(r#"{"type":"response.failed","response":{"error":{"code":"usage_limit_reached","message":"limit"}}}"#);
        match out.as_slice() {
            [Output::Error(err)] => assert_eq!(err.status, StatusCode::TOO_MANY_REQUESTS),
            _ => panic!("expected an error"),
        }
    }
}
