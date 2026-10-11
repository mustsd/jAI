//! jAI's domain layer: messages, provider/tool/policy traits, the agent engine,
//! and the helpers (redaction, time, text, token accounting) shared by every
//! front end.
//!
//! Layout rule: this crate depends on nothing jAI-specific, so `jai-cli` and
//! `jai-web` can both drive the same engine without depending on each other.

pub mod engine;
pub mod error;
pub mod ids;
pub mod message;
pub mod policy;
pub mod provider;
pub mod redact;
pub mod store;
pub mod stream_accum;
pub mod text;
pub mod time;
pub mod tokens;
pub mod tool;
pub mod usage;

pub use engine::{Engine, EngineEvent, EngineSettings, TurnCanceller, TurnHandle};
pub use error::{Error, Result};
pub use message::{
    CancelFlag, ChatEvent, ChatRequest, MAX_TOOL_NAME_LEN, Message, ModelInfo, PermissionMode,
    RiskLevel, Role, TokenUsage, ToolCall, ToolSpec, is_valid_tool_name, normalize_tool_name,
};
pub use policy::{
    AlwaysApprove, AlwaysDeny, Approval, ApprovalRequest, Approver, Decision, Policy, ToolRequest,
};
pub use provider::{BoxFuture, Provider};
pub use redact::Redactor;
pub use store::{MemoryStore, Session, SessionStore, SessionSummary, ToolAudit};
pub use tool::{ToolExecutor, ToolOutcome};
pub use usage::{ContextUsage, TaskUsage, UsageLine};
