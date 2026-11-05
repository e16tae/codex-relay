//! Chat Completions request → Codex Responses request.

use serde::Deserialize;
use serde_json::{Value, json};

use crate::openai::error::ApiError;

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
    pub tool_choice: Option<Value>,
    #[serde(default)]
    pub parallel_tool_calls: Option<bool>,
    #[serde(default)]
    pub functions: Option<Vec<Value>>,
    #[serde(default)]
    pub n: Option<u32>,
}

#[derive(Deserialize)]
pub struct StreamOptions {
    #[serde(default)]
    pub include_usage: bool,
}

/// Unknown fields and `null`s (such as Nextcloud's `"name": null`) are ignored.
#[derive(Deserialize)]
pub struct ChatMessage {
    pub role: String,
    #[serde(default)]
    pub content: Option<MessageContent>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(default)]
    pub tool_call_id: Option<String>,
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

#[derive(Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub function: FunctionCall,
}

#[derive(Deserialize)]
pub struct FunctionCall {
    pub name: String,
    /// A JSON string per the spec; objects are tolerated and re-serialised.
    #[serde(default)]
    pub arguments: Option<Value>,
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
    if req.functions.as_ref().is_some_and(|f| !f.is_empty()) {
        return Err(ApiError::invalid_request(
            "the legacy `functions` parameter is not supported; use `tools`",
        ));
    }
    if req.model.trim().is_empty() {
        return Err(ApiError::invalid_request("model is required"));
    }
    let mut input = Vec::new();
    for (i, message) in req.messages.iter().enumerate() {
        let items = convert_message(message)
            .map_err(|err| ApiError::invalid_request(format!("messages[{i}]: {err}")))?;
        input.extend(items);
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
    if let Some(tools) = req.tools.as_deref().filter(|t| !t.is_empty()) {
        body["tools"] = convert_tools(tools)?.into();
        body["tool_choice"] = match &req.tool_choice {
            Some(choice) => convert_tool_choice(choice)?,
            None => "auto".into(),
        };
        body["parallel_tool_calls"] = req.parallel_tool_calls.unwrap_or(true).into();
    }
    Ok(body)
}

fn convert_message(message: &ChatMessage) -> Result<Vec<Value>, String> {
    let (role, text_type) = match message.role.as_str() {
        "system" | "developer" => ("developer", "input_text"),
        "user" => ("user", "input_text"),
        "assistant" => ("assistant", "output_text"),
        "tool" => return convert_tool_result(message).map(|item| vec![item]),
        "function" => {
            return Err("legacy `function` messages are not supported; use `tool`".into());
        }
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

    let mut items = Vec::new();
    if !content.is_empty() {
        items.push(json!({ "type": "message", "role": role, "content": content }));
    }
    if role == "assistant" {
        for call in message.tool_calls.iter().flatten() {
            let arguments = match &call.function.arguments {
                None | Some(Value::Null) => "{}".to_owned(),
                Some(Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
            };
            items.push(json!({
                "type": "function_call",
                "call_id": call.id,
                "name": call.function.name,
                "arguments": arguments,
            }));
        }
    }
    Ok(items)
}

fn convert_tool_result(message: &ChatMessage) -> Result<Value, String> {
    let call_id = message
        .tool_call_id
        .as_deref()
        .filter(|id| !id.is_empty())
        .ok_or("tool messages need a `tool_call_id`")?;
    let output = match &message.content {
        None => String::new(),
        Some(MessageContent::Text(text)) => text.clone(),
        Some(MessageContent::Parts(parts)) => {
            let mut texts = Vec::new();
            for part in parts {
                match part {
                    ContentPart::Text { text } => texts.push(text.as_str()),
                    _ => return Err("tool results may only contain text".into()),
                }
            }
            texts.join("\n")
        }
    };
    Ok(json!({ "type": "function_call_output", "call_id": call_id, "output": output }))
}

/// `{"type":"function","function":{name,description,parameters,strict}}` →
/// `{"type":"function",name,description,parameters,strict}`.
fn convert_tools(tools: &[Value]) -> Result<Vec<Value>, ApiError> {
    tools
        .iter()
        .enumerate()
        .map(|(i, tool)| {
            let kind = tool.get("type").and_then(Value::as_str);
            if kind != Some("function") {
                return Err(ApiError::invalid_request(format!(
                    "tools[{i}]: only `function` tools are supported"
                )));
            }
            let function = tool.get("function").ok_or_else(|| {
                ApiError::invalid_request(format!("tools[{i}].function is required"))
            })?;
            let name = function
                .get("name")
                .and_then(Value::as_str)
                .filter(|n| !n.is_empty())
                .ok_or_else(|| {
                    ApiError::invalid_request(format!("tools[{i}].function.name is required"))
                })?;
            let mut out = json!({
                "type": "function",
                "name": name,
                "parameters": function
                    .get("parameters")
                    .cloned()
                    .unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            });
            for key in ["description", "strict"] {
                if let Some(value) = function.get(key).filter(|v| !v.is_null()) {
                    out[key] = value.clone();
                }
            }
            Ok(out)
        })
        .collect()
}

fn convert_tool_choice(choice: &Value) -> Result<Value, ApiError> {
    match choice {
        Value::String(mode) if matches!(mode.as_str(), "auto" | "none" | "required") => {
            Ok(choice.clone())
        }
        Value::Object(_) => {
            let name = choice
                .pointer("/function/name")
                .and_then(Value::as_str)
                .ok_or_else(|| {
                    ApiError::invalid_request("tool_choice.function.name is required")
                })?;
            Ok(json!({ "type": "function", "name": name }))
        }
        _ => Err(ApiError::invalid_request(format!(
            "unsupported tool_choice {choice}"
        ))),
    }
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
        assert!(body.get("tools").is_none());
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
    fn converts_tools_and_tool_history() {
        let req = request(json!({
            "model": "m",
            "tool_choice": { "type": "function", "function": { "name": "get_weather" } },
            "tools": [{ "type": "function", "function": {
                "name": "get_weather", "description": "Weather", "strict": null,
                "parameters": { "type": "object", "properties": { "city": { "type": "string" } } }
            }}],
            "messages": [
                { "role": "user", "content": "Weather in Seoul and Busan?" },
                { "role": "assistant", "content": null, "tool_calls": [
                    { "id": "call_a", "type": "function", "function": { "name": "get_weather", "arguments": "{\"city\":\"Seoul\"}" } },
                    { "id": "call_b", "type": "function", "function": { "name": "get_weather", "arguments": { "city": "Busan" } } }
                ]},
                { "role": "tool", "tool_call_id": "call_a", "name": null, "content": "Sunny" },
                { "role": "tool", "tool_call_id": "call_b", "content": [{ "type": "text", "text": "Rain" }] }
            ]
        }));
        let body = to_responses_body(&req, None).unwrap();
        assert_eq!(body["tools"][0]["name"], "get_weather");
        assert_eq!(body["tools"][0]["description"], "Weather");
        assert!(
            body["tools"][0].get("strict").is_none(),
            "null strict is dropped"
        );
        assert!(body["tools"][0].get("function").is_none());
        assert_eq!(
            body["tool_choice"],
            json!({ "type": "function", "name": "get_weather" })
        );
        assert_eq!(body["parallel_tool_calls"], true);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 5);
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "call_a");
        assert_eq!(input[2]["arguments"], "{\"city\":\"Busan\"}");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["output"], "Sunny");
        assert_eq!(input[4]["output"], "Rain");
    }

    #[test]
    fn empty_tools_are_ignored() {
        let req = request(json!({
            "model": "m", "tools": [], "tool_choice": "auto",
            "messages": [{ "role": "user", "content": "hi" }]
        }));
        let body = to_responses_body(&req, None).unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
    }

    #[test]
    fn rejects_unsupported_requests() {
        for value in [
            json!({ "model": "m", "n": 2, "messages": [{ "role": "user", "content": "hi" }] }),
            json!({ "model": "m", "messages": [] }),
            json!({ "model": "m", "messages": [{ "role": "tool", "content": "x" }] }),
            json!({ "model": "m", "messages": [{ "role": "function", "name": "f", "content": "x" }] }),
            json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }],
                    "tools": [{ "type": "web_search" }] }),
            json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }],
                    "functions": [{ "name": "f" }] }),
            json!({ "model": "m", "messages": [{ "role": "user", "content": "hi" }],
                    "tools": [{ "type": "function", "function": { "name": "f" } }],
                    "tool_choice": "sometimes" }),
        ] {
            assert!(to_responses_body(&request(value), None).is_err());
        }
    }
}
