//! Tool metadata and execution interface.
//!
//! Tools declare a name, a JSON schema and a risk level; they never decide
//! whether they are allowed to run (that is the policy engine's job).

use crate::error::Result;
use crate::message::ToolCall;
use crate::policy::ToolRequest;
use crate::provider::BoxFuture;
use serde::{Deserialize, Serialize};

/// Result of running a tool.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolOutcome {
    pub ok: bool,
    pub content: String,
    /// True when the output had to be truncated to the configured byte cap.
    pub truncated: bool,
}

impl ToolOutcome {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            ok: true,
            content: content.into(),
            truncated: false,
        }
    }

    pub fn failure(content: impl Into<String>) -> Self {
        Self {
            ok: false,
            content: content.into(),
            truncated: false,
        }
    }
}

/// Registry of the tools offered to the model.
pub trait ToolExecutor: Send + Sync {
    /// Tool definitions sent with every request.
    fn specs(&self) -> Vec<crate::message::ToolSpec>;

    /// Metadata for policy checks and confirmation prompts.
    fn describe(&self, call: &ToolCall) -> ToolRequest;

    /// Run a tool call. The policy engine has already approved it.
    fn execute(&self, call: &ToolCall) -> BoxFuture<'_, Result<ToolOutcome>>;
}
