//! Tool execution policy: the single place that decides whether a model-issued
//! tool call runs, asks the user, or is refused.
//!
//! `safe` adds a confirmation step on top of the policy, `yolo` skips the
//! confirmation but still passes through the same [`Policy`] engine, so
//! hard denials (outside the workspace, sensitive files, disabled tools) apply
//! in both modes.

use crate::message::{PermissionMode, RiskLevel, ToolCall};
use crate::provider::BoxFuture;
use serde::{Deserialize, Serialize};

/// Everything the policy engine needs to judge one tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolRequest {
    pub call: ToolCall,
    /// What the tool would do, in one line, for the confirmation prompt.
    pub summary: String,
    pub risk: RiskLevel,
    /// Files the tool would touch, relative to the workspace when possible.
    pub paths: Vec<String>,
    /// Working directory the tool would run in.
    pub cwd: String,
}

impl ToolRequest {
    pub fn new(call: ToolCall, summary: impl Into<String>, risk: RiskLevel) -> Self {
        Self {
            call,
            summary: summary.into(),
            risk,
            paths: Vec::new(),
            cwd: String::new(),
        }
    }

    pub fn with_paths(mut self, paths: Vec<String>) -> Self {
        self.paths = paths;
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = cwd.into();
        self
    }
}

/// Outcome of a policy check.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Decision {
    /// Run the tool.
    Allow,
    /// Ask the user first (only meaningful in `safe` mode).
    Confirm { reason: String },
    /// Refuse; the reason is shown to the user and returned to the model.
    Deny { reason: String },
}

impl Decision {
    pub fn confirm(reason: impl Into<String>) -> Self {
        Self::Confirm {
            reason: reason.into(),
        }
    }

    pub fn deny(reason: impl Into<String>) -> Self {
        Self::Deny {
            reason: reason.into(),
        }
    }

    /// Short label used in the audit trail (`allow`, `confirm`, `deny`).
    pub fn label(&self) -> &'static str {
        match self {
            Decision::Allow => "allow",
            Decision::Confirm { .. } => "confirm",
            Decision::Deny { .. } => "deny",
        }
    }

    pub fn reason(&self) -> Option<&str> {
        match self {
            Decision::Allow => None,
            Decision::Confirm { reason } | Decision::Deny { reason } => Some(reason),
        }
    }

    pub fn is_allow(&self) -> bool {
        matches!(self, Decision::Allow)
    }
}

/// Decides what happens to a tool call before it runs.
pub trait Policy: Send + Sync {
    fn evaluate(&self, request: &ToolRequest) -> Decision;
}

/// What the user answered when asked to approve a tool call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Approval {
    Approved,
    Denied,
}

/// Context for a confirmation prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApprovalRequest {
    pub request: ToolRequest,
    /// Why confirmation is being asked.
    pub reason: String,
    pub mode: PermissionMode,
}

/// Asks the user to approve a tool call. CLI prompts on the terminal; web mode
/// waits on a pending-approval channel.
pub trait Approver: Send + Sync {
    fn approve(&self, request: ApprovalRequest) -> BoxFuture<'_, Approval>;
}

/// Approver used in `yolo` mode and in tests.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysApprove;

impl Approver for AlwaysApprove {
    fn approve(&self, _request: ApprovalRequest) -> BoxFuture<'_, Approval> {
        Box::pin(async { Approval::Approved })
    }
}

/// Refuses every confirmation prompt; used when no interactive user exists.
#[derive(Debug, Clone, Copy, Default)]
pub struct AlwaysDeny;

impl Approver for AlwaysDeny {
    fn approve(&self, _request: ApprovalRequest) -> BoxFuture<'_, Approval> {
        Box::pin(async { Approval::Denied })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_labels_match_the_audit_trail() {
        assert_eq!(Decision::Allow.label(), "allow");
        assert_eq!(Decision::confirm("write").label(), "confirm");
        assert_eq!(Decision::deny("outside workspace").label(), "deny");
        assert_eq!(Decision::deny("nope").reason(), Some("nope"));
        assert!(Decision::Allow.is_allow());
    }

    #[tokio::test]
    async fn default_approvers_behave_as_named() {
        let request = ApprovalRequest {
            request: ToolRequest::new(
                ToolCall::new("1", "shell_execute", "{}"),
                "cargo test",
                RiskLevel::Execute,
            ),
            reason: "execute".to_string(),
            mode: PermissionMode::Safe,
        };
        assert_eq!(
            AlwaysApprove.approve(request.clone()).await,
            Approval::Approved
        );
        assert_eq!(AlwaysDeny.approve(request).await, Approval::Denied);
    }
}
