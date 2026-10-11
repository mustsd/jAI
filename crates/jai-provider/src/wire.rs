//! Wire format for OpenAI-compatible `/chat/completions` and `/models`.
//!
//! Only fields jAI understands are decoded; everything else is ignored so that
//! provider extensions do not break parsing.

use jai_core::{
    ChatEvent, ChatRequest, Message, Role, TokenUsage, ToolCall, ToolSpec, normalize_tool_name,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// Build the JSON body for a streaming chat completion.
pub fn build_chat_body(request: &ChatRequest) -> Value {
    let mut body = Map::new();
    body.insert("model".to_string(), Value::String(request.model.clone()));
    body.insert(
        "messages".to_string(),
        Value::Array(request.messages.iter().map(message_json).collect()),
    );
    body.insert("stream".to_string(), Value::Bool(true));
    if !request.tools.is_empty() {
        body.insert(
            "tools".to_string(),
            Value::Array(request.tools.iter().map(tool_spec_json).collect()),
        );
        body.insert("tool_choice".to_string(), Value::String("auto".to_string()));
    }
    if let Some(temperature) = request.temperature {
        body.insert("temperature".to_string(), temperature_json(temperature));
    }
    if let Some(max_tokens) = request.max_tokens {
        body.insert("max_tokens".to_string(), json!(max_tokens));
    }
    // `--extra-json` / config overrides win over everything above, which is the
    // documented escape hatch for provider-specific fields.
    for (key, value) in &request.extra {
        body.insert(key.clone(), value.clone());
    }
    Value::Object(body)
}

/// One message in OpenAI's shape.
pub fn message_json(message: &Message) -> Value {
    let mut object = Map::new();
    object.insert(
        "role".to_string(),
        Value::String(message.role.as_str().to_string()),
    );
    let content = if message.content.is_empty() && !message.tool_calls.is_empty() {
        // Assistant turns that only request tools send `content: null`.
        Value::Null
    } else {
        Value::String(message.content.clone())
    };
    object.insert("content".to_string(), content);
    if !message.tool_calls.is_empty() {
        object.insert(
            "tool_calls".to_string(),
            Value::Array(message.tool_calls.iter().map(tool_call_json).collect()),
        );
    }
    if let Some(id) = &message.tool_call_id {
        object.insert("tool_call_id".to_string(), Value::String(id.clone()));
    }
    if message.role == Role::Tool {
        if let Some(name) = &message.name {
            object.insert("name".to_string(), Value::String(normalize_tool_name(name)));
        }
    }
    Value::Object(object)
}

/// `f32` to JSON without binary-floating-point noise (`0.2`, not
/// `0.20000000298023224`).
fn temperature_json(value: f32) -> Value {
    let cleaned = value
        .to_string()
        .parse::<f64>()
        .unwrap_or_else(|_| f64::from(value));
    serde_json::Number::from_f64(cleaned)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

fn tool_call_json(call: &ToolCall) -> Value {
    json!({
        "id": call.id,
        "type": "function",
        "function": {
            "name": normalize_tool_name(&call.name),
            "arguments": call.arguments,
        }
    })
}

fn tool_spec_json(spec: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": normalize_tool_name(&spec.name),
            "description": spec.description,
            "parameters": spec.parameters,
        }
    })
}

/// One `data:` frame of a streaming completion.
#[derive(Debug, Deserialize)]
pub struct StreamChunk {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    #[serde(default)]
    pub usage: Option<UsagePayload>,
    #[serde(default)]
    pub error: Option<ErrorPayload>,
}

#[derive(Debug, Deserialize)]
pub struct StreamChoice {
    #[serde(default)]
    pub delta: Option<DeltaPayload>,
}

#[derive(Debug, Deserialize)]
pub struct DeltaPayload {
    #[serde(default)]
    pub content: Option<String>,
    /// DeepSeek's dialect for streamed chain-of-thought.
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// OpenRouter and a few others use the shorter key.
    #[serde(default)]
    pub reasoning: Option<String>,
    #[serde(default)]
    pub tool_calls: Option<Vec<ToolCallFragment>>,
}

#[derive(Debug, Deserialize)]
pub struct ToolCallFragment {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<FunctionPayload>,
}

#[derive(Debug, Deserialize)]
pub struct FunctionPayload {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct UsagePayload {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

/// A provider-side error delivered inside the stream.
#[derive(Debug, Deserialize)]
pub struct ErrorPayload {
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub type_field: Option<String>,
    #[serde(default)]
    pub code: Option<Value>,
}

impl ErrorPayload {
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(message) = &self.message {
            parts.push(message.clone());
        }
        if let Some(code) = self.code.as_ref().and_then(Value::as_str) {
            parts.push(format!("code={code}"));
        }
        if let Some(kind) = &self.type_field {
            parts.push(format!("type={kind}"));
        }
        if parts.is_empty() {
            "provider reported an unspecified streaming error".to_string()
        } else {
            parts.join(" ")
        }
    }
}

impl StreamChunk {
    /// Convert a chunk into provider-neutral events.
    pub fn into_events(self) -> Result<Vec<ChatEvent>, String> {
        if let Some(error) = self.error {
            return Err(error.describe());
        }
        let mut events = Vec::new();
        for choice in self.choices {
            let Some(delta) = choice.delta else {
                continue;
            };
            if let Some(content) = delta.content {
                if !content.is_empty() {
                    events.push(ChatEvent::Delta(content));
                }
            }
            // Reasoning is reported before the answer; the provider decides the
            // key, so both spellings are accepted.
            for reasoning in [delta.reasoning_content, delta.reasoning].into_iter().flatten() {
                if !reasoning.is_empty() {
                    events.push(ChatEvent::ReasoningDelta(reasoning));
                }
            }
            for fragment in delta.tool_calls.into_iter().flatten() {
                let (name, arguments) = match fragment.function {
                    Some(function) => (function.name, function.arguments),
                    None => (None, None),
                };
                events.push(ChatEvent::ToolCallDelta {
                    index: fragment.index.unwrap_or(0),
                    id: fragment.id,
                    name,
                    arguments_delta: arguments,
                });
            }
        }
        if let Some(usage) = self.usage {
            events.push(ChatEvent::Usage(TokenUsage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
            }));
        }
        Ok(events)
    }
}

/// `GET /models` response.
#[derive(Debug, Deserialize)]
pub struct ModelsResponse {
    #[serde(default)]
    pub data: Vec<ModelEntry>,
}

#[derive(Debug, Deserialize)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub owned_by: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> ChatRequest {
        ChatRequest::new("deepseek-chat", vec![Message::user("hi")])
    }

    #[test]
    fn body_defaults_to_a_streaming_completion() {
        let body = build_chat_body(&request());
        assert_eq!(body["model"], "deepseek-chat");
        assert_eq!(body["stream"], true);
        assert_eq!(body["messages"][0]["role"], "user");
        assert!(body.get("tools").is_none());
        assert!(body.get("temperature").is_none());
    }

    #[test]
    fn tools_and_sampling_options_are_included() {
        let mut request = request();
        request.tools.push(ToolSpec {
            name: "filesystem_read".to_string(),
            description: "read a file".to_string(),
            parameters: json!({"type": "object", "properties": {"path": {"type": "string"}}}),
        });
        request.temperature = Some(0.2);
        request.max_tokens = Some(256);
        let body = build_chat_body(&request);
        assert_eq!(body["tools"][0]["function"]["name"], "filesystem_read");
        assert_eq!(body["tool_choice"], "auto");
        assert_eq!(body["temperature"], 0.2);
        assert_eq!(body["max_tokens"], 256);
    }

    #[test]
    fn tool_names_are_sanitised_before_they_reach_the_provider() {
        // A name outside `^[a-zA-Z0-9_-]+$` makes DeepSeek answer
        // `400 Invalid 'tools[0].function.name'`, so the wire layer rewrites it.
        let mut request = request();
        request.tools.push(ToolSpec {
            name: "filesystem.read".to_string(),
            description: "read a file".to_string(),
            parameters: json!({"type": "object"}),
        });
        let body = build_chat_body(&request);
        assert_eq!(body["tools"][0]["function"]["name"], "filesystem_read");

        // Sessions recorded before the rename replay assistant/tool messages
        // that still carry the dotted spelling.
        let legacy =
            Message::assistant_with_tools("", vec![ToolCall::new("call_1", "shell.execute", "{}")]);
        let json = message_json(&legacy);
        assert_eq!(json["tool_calls"][0]["function"]["name"], "shell_execute");
        assert_eq!(json["tool_calls"][0]["function"]["arguments"], "{}");
        let result = message_json(&Message::tool("call_1", "shell.execute", "ok"));
        assert_eq!(result["name"], "shell_execute");
    }

    #[test]
    fn temperature_is_sent_without_float_noise() {
        let mut request = request();
        request.temperature = Some(0.2);
        let body = build_chat_body(&request);
        assert_eq!(body["temperature"].to_string(), "0.2");
        request.temperature = Some(0.0);
        assert_eq!(build_chat_body(&request)["temperature"].to_string(), "0.0");
    }

    #[test]
    fn extra_json_overrides_built_in_fields() {
        let mut request = request();
        request.max_tokens = Some(256);
        request.extra.insert("max_tokens".to_string(), json!(1024));
        request
            .extra
            .insert("thinking".to_string(), json!({"type": "enabled"}));
        let body = build_chat_body(&request);
        assert_eq!(body["max_tokens"], 1024);
        assert_eq!(body["thinking"]["type"], "enabled");
    }

    #[test]
    fn assistant_tool_calls_use_the_wire_shape() {
        let message = Message::assistant_with_tools(
            "",
            vec![ToolCall::new(
                "call_1",
                "shell_execute",
                "{\"command\":\"ls\"}",
            )],
        );
        let json = message_json(&message);
        assert_eq!(json["content"], Value::Null);
        assert_eq!(json["tool_calls"][0]["id"], "call_1");
        assert_eq!(json["tool_calls"][0]["type"], "function");
        assert_eq!(json["tool_calls"][0]["function"]["name"], "shell_execute");
    }

    #[test]
    fn tool_results_carry_the_call_id() {
        let json = message_json(&Message::tool("call_1", "shell_execute", "ok"));
        assert_eq!(json["role"], "tool");
        assert_eq!(json["tool_call_id"], "call_1");
        assert_eq!(json["name"], "shell_execute");
    }

    #[test]
    fn stream_chunks_decode_text_tool_calls_and_usage() {
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"choices":[{"delta":{"content":"Hi","tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"fs.read","arguments":"{"}}]},"finish_reason":null}],"usage":{"prompt_tokens":3,"completion_tokens":4,"total_tokens":7}}"#,
        )
        .unwrap();
        let events = chunk.into_events().unwrap();
        assert!(matches!(&events[0], ChatEvent::Delta(text) if text == "Hi"));
        assert!(matches!(
            &events[1],
            ChatEvent::ToolCallDelta { index: 0, id: Some(id), name: Some(name), arguments_delta: Some(args) }
                if id == "call_1" && name == "fs.read" && args == "{"
        ));
        assert!(matches!(events[2], ChatEvent::Usage(usage) if usage.total_tokens == 7));
    }

    #[test]
    fn role_only_chunks_decode_to_nothing() {
        let chunk: StreamChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#).unwrap();
        assert!(chunk.into_events().unwrap().is_empty());
    }

    #[test]
    fn empty_content_frames_are_dropped() {
        let chunk: StreamChunk =
            serde_json::from_str(r#"{"choices":[{"delta":{"content":""}}]}"#).unwrap();
        assert!(chunk.into_events().unwrap().is_empty());
    }

    #[test]
    fn in_stream_errors_are_surfaced() {
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"error":{"message":"rate limited","type":"rate_limit_error","code":"429"}}"#,
        )
        .unwrap();
        let error = chunk.into_events().unwrap_err();
        assert!(error.contains("rate limited"), "{error}");
        assert!(error.contains("429"), "{error}");
    }

    #[test]
    fn unknown_fields_do_not_break_decoding() {
        let chunk: StreamChunk = serde_json::from_str(
            r#"{"id":"x","object":"chat.completion.chunk","created":1,"model":"m","choices":[{"index":0,"logprobs":null,"delta":{"content":"ok"}}],"system_fingerprint":"fp"}"#,
        )
        .unwrap();
        assert_eq!(chunk.into_events().unwrap().len(), 1);
    }

    #[test]
    fn models_response_decodes() {
        let response: ModelsResponse = serde_json::from_str(
            r#"{"object":"list","data":[{"id":"deepseek-chat","owned_by":"deepseek"}]}"#,
        )
        .unwrap();
        assert_eq!(response.data[0].id, "deepseek-chat");
        assert_eq!(response.data[0].owned_by.as_deref(), Some("deepseek"));
    }
}
