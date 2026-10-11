//! Conversation and provider-agnostic request/response types.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Cooperative cancellation flag shared with providers.
pub type CancelFlag = Arc<AtomicBool>;

/// Conversation roles, matching the OpenAI chat schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "system" => Some(Role::System),
            "user" => Some(Role::User),
            "assistant" => Some(Role::Assistant),
            "tool" => Some(Role::Tool),
            _ => None,
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A tool invocation requested by the model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned id; used to match the tool result message.
    pub id: String,
    /// Function name, e.g. `filesystem_read`. OpenAI-compatible APIs only
    /// accept names matching `^[a-zA-Z0-9_-]+$`; see [`normalize_tool_name`].
    pub name: String,
    /// Raw JSON arguments exactly as produced by the model.
    pub arguments: String,
}

impl ToolCall {
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// Copy with a wire-safe function name (see [`normalize_tool_name`]).
    pub fn normalized(&self) -> Self {
        let name = normalize_tool_name(&self.name);
        if name == self.name {
            return self.clone();
        }
        Self {
            name,
            ..self.clone()
        }
    }

    /// Parsed arguments; empty/blank arguments parse to JSON `null`.
    pub fn arguments_value(&self) -> Result<Value, serde_json::Error> {
        let trimmed = self.arguments.trim();
        if trimmed.is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(trimmed)
    }
}

/// A single conversation message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    #[serde(default)]
    pub content: String,
    /// Reasoning/thinking text the model produced alongside `content`
    /// (`reasoning_content` in DeepSeek's dialect).
    ///
    /// This is *never* sent back to the provider — it is kept so the user can
    /// read what the model was thinking and so `jai session --export` keeps the
    /// full record.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl Message {
    pub fn system(content: impl Into<String>) -> Self {
        Self::plain(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::plain(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::plain(Role::Assistant, content)
    }

    pub fn assistant_with_tools(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            reasoning: None,
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    /// Attach reasoning text to an assistant message.
    pub fn with_reasoning(mut self, reasoning: Option<String>) -> Self {
        self.reasoning = reasoning.filter(|text| !text.trim().is_empty());
        self
    }

    /// Reasoning text, when the model produced any.
    pub fn reasoning(&self) -> Option<&str> {
        self.reasoning.as_deref().filter(|text| !text.trim().is_empty())
    }

    /// Result of a tool call, fed back to the model.
    pub fn tool(
        tool_call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
            name: Some(name.into()),
        }
    }

    fn plain(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            reasoning: None,
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }
}

/// JSON-schema description of a tool offered to the model.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Token accounting reported by the provider.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl TokenUsage {
    pub fn add(&mut self, other: &TokenUsage) {
        self.prompt_tokens += other.prompt_tokens;
        self.completion_tokens += other.completion_tokens;
        self.total_tokens += other.total_tokens;
    }

    pub fn is_empty(&self) -> bool {
        self.prompt_tokens == 0 && self.completion_tokens == 0 && self.total_tokens == 0
    }
}

/// A model advertised by a provider's `GET /models`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owned_by: Option<String>,
}

/// One provider request.
#[derive(Debug, Clone)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Extra JSON merged into the request body (`--extra-json` / config).
    pub extra: Map<String, Value>,
    /// Cooperative cancellation observed while streaming.
    pub cancel: Option<CancelFlag>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            temperature: None,
            max_tokens: None,
            extra: Map::new(),
            cancel: None,
        }
    }

    pub fn cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .map(|flag| flag.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(false)
    }
}

/// Streaming events emitted by a provider.
#[derive(Debug, Clone, PartialEq)]
pub enum ChatEvent {
    /// Visible assistant text.
    Delta(String),
    /// Reasoning/thinking text, when the provider exposes it.
    ReasoningDelta(String),
    /// Incremental tool call; `index` groups fragments of one call.
    ToolCallDelta {
        index: usize,
        id: Option<String>,
        name: Option<String>,
        arguments_delta: Option<String>,
    },
    Usage(TokenUsage),
    /// The provider signalled end of stream.
    Finished,
}

/// How much damage a tool call could do. Drives the policy engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    ReadOnly,
    Write,
    Execute,
    Destructive,
    Network,
}

impl RiskLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            RiskLevel::ReadOnly => "read-only",
            RiskLevel::Write => "write",
            RiskLevel::Execute => "execute",
            RiskLevel::Destructive => "destructive",
            RiskLevel::Network => "network",
        }
    }
}

impl std::fmt::Display for RiskLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Tool execution policy: safe (confirm) or yolo (no prompts).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    /// Ask the user before every tool call.
    #[default]
    Safe,
    /// Never ask, but still pass every call through the policy engine.
    Yolo,
}

impl PermissionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionMode::Safe => "safe",
            PermissionMode::Yolo => "yolo",
        }
    }
}

impl std::fmt::Display for PermissionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for PermissionMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "safe" => Ok(PermissionMode::Safe),
            "yolo" => Ok(PermissionMode::Yolo),
            other => Err(format!(
                "unknown permission mode `{other}` (expected safe|yolo)"
            )),
        }
    }
}

/// Longest function name OpenAI-compatible APIs accept.
pub const MAX_TOOL_NAME_LEN: usize = 64;

/// Map a function name onto the `^[a-zA-Z0-9_-]+$` pattern that
/// OpenAI-compatible APIs require, replacing every other character with `_`.
///
/// Names are the one part of the request no provider forgives: DeepSeek answers
/// `400 Invalid 'tools[0].function.name'` for anything outside that pattern, so
/// the wire layer sanitises instead of trusting callers.
///
/// The result is always non-empty ASCII and at most [`MAX_TOOL_NAME_LEN`] bytes
/// (an all-invalid name becomes `tool`).
pub fn normalize_tool_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len().min(MAX_TOOL_NAME_LEN));
    for ch in name.chars() {
        if out.len() == MAX_TOOL_NAME_LEN {
            break;
        }
        out.push(if is_tool_name_char(ch) { ch } else { '_' });
    }
    if out.is_empty() {
        out.push_str("tool");
    }
    out
}

/// Whether `name` already matches the pattern providers enforce.
pub fn is_valid_tool_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= MAX_TOOL_NAME_LEN && name.chars().all(is_tool_name_char)
}

fn is_tool_name_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_round_trip() {
        for role in [Role::System, Role::User, Role::Assistant, Role::Tool] {
            assert_eq!(Role::parse(role.as_str()), Some(role));
        }
        assert_eq!(Role::parse("developer"), None);
    }

    #[test]
    fn tool_arguments_parse_and_default_to_null() {
        let call = ToolCall::new("call_1", "filesystem_read", r#"{"path":"Cargo.toml"}"#);
        let value = call.arguments_value().unwrap();
        assert_eq!(value["path"], "Cargo.toml");
        assert!(
            ToolCall::new("c", "t", "")
                .arguments_value()
                .unwrap()
                .is_null()
        );
        assert!(
            ToolCall::new("c", "t", "{not json")
                .arguments_value()
                .is_err()
        );
    }

    #[test]
    fn message_helpers_keep_the_wire_shape() {
        let message = Message::tool("call_1", "shell_execute", "ok");
        assert_eq!(message.role, Role::Tool);
        assert_eq!(message.tool_call_id.as_deref(), Some("call_1"));
        let json = serde_json::to_string(&message).unwrap();
        assert!(
            !json.contains("tool_calls"),
            "empty tool_calls must be omitted: {json}"
        );
        assert!(json.contains("\"role\":\"tool\""));
    }

    #[test]
    fn usage_accumulates() {
        let mut total = TokenUsage::default();
        total.add(&TokenUsage {
            prompt_tokens: 10,
            completion_tokens: 5,
            total_tokens: 15,
        });
        total.add(&TokenUsage {
            prompt_tokens: 1,
            completion_tokens: 2,
            total_tokens: 3,
        });
        assert_eq!(total.total_tokens, 18);
        assert!(!total.is_empty());
    }

    #[test]
    fn permission_mode_parsing_is_lenient_about_case() {
        assert_eq!(
            "SAFE".parse::<PermissionMode>().unwrap(),
            PermissionMode::Safe
        );
        assert_eq!(
            "yolo".parse::<PermissionMode>().unwrap(),
            PermissionMode::Yolo
        );
        assert!("maybe".parse::<PermissionMode>().is_err());
        assert_eq!(PermissionMode::default(), PermissionMode::Safe);
    }

    #[test]
    fn risk_levels_are_ordered_from_read_only_to_network() {
        assert!(RiskLevel::ReadOnly < RiskLevel::Write);
        assert!(RiskLevel::Write < RiskLevel::Execute);
        assert!(RiskLevel::Execute < RiskLevel::Destructive);
        assert!(RiskLevel::Destructive < RiskLevel::Network);
    }

    #[test]
    fn tool_names_are_normalized_for_the_wire() {
        assert_eq!(normalize_tool_name("filesystem.read"), "filesystem_read");
        assert_eq!(normalize_tool_name("shell execute"), "shell_execute");
        assert_eq!(normalize_tool_name("网络.请求"), "_____");
        assert_eq!(normalize_tool_name(""), "tool");
        assert_eq!(normalize_tool_name("/etc/passwd"), "_etc_passwd");
        // Providers reject anything longer than 64 bytes.
        assert_eq!(
            normalize_tool_name(&"x".repeat(100)).len(),
            MAX_TOOL_NAME_LEN
        );
        // Whatever the input, the result passes the pattern check.
        for name in ["filesystem_read", "a.b", "", "网络.请求", "fs/read"] {
            let normalized = normalize_tool_name(name);
            assert!(is_valid_tool_name(&normalized), "{name} -> {normalized}");
        }
        assert!(is_valid_tool_name("read-file_1"));
        assert!(!is_valid_tool_name("filesystem.read"));
        assert!(!is_valid_tool_name(""));
        assert!(!is_valid_tool_name(&"x".repeat(MAX_TOOL_NAME_LEN + 1)));
    }

    #[test]
    fn normalized_calls_only_rewrite_the_name() {
        let call = ToolCall::new("call_1", "filesystem.read", r#"{"path":"a"}"#);
        let normalized = call.normalized();
        assert_eq!(normalized.name, "filesystem_read");
        assert_eq!(normalized.id, "call_1");
        assert_eq!(normalized.arguments, call.arguments);
        // Already-valid names are left alone, and normalizing is idempotent.
        assert_eq!(normalized.normalized(), normalized);
        assert_eq!(
            ToolCall::new("c", "shell_execute", "{}").normalized().name,
            "shell_execute"
        );
    }
}
