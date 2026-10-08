//! Terminal confirmation prompts for `safe` mode.
//!
//! The prompt shows exactly what jAI wants to do — tool, arguments, risk,
//! working directory, files — and defaults to "no" (just press Enter to refuse).

use jai_core::policy::{AlwaysApprove, Approval, ApprovalRequest, Approver};
use jai_core::provider::BoxFuture;
use std::io::{BufRead, Write};
use std::sync::Arc;

/// Asks on stderr and reads the answer from stdin.
#[derive(Debug, Clone, Copy, Default)]
pub struct TerminalApprover;

/// Approver for a permission mode: terminal prompts in `safe`, automatic in
/// `yolo` (where the policy engine already decided the call is acceptable).
pub fn approver_for(mode: jai_core::PermissionMode) -> Arc<dyn Approver> {
    match mode {
        jai_core::PermissionMode::Safe => Arc::new(TerminalApprover),
        jai_core::PermissionMode::Yolo => Arc::new(AlwaysApprove),
    }
}

impl Approver for TerminalApprover {
    fn approve(&self, request: ApprovalRequest) -> BoxFuture<'_, Approval> {
        Box::pin(async move {
            print_request(&request);
            match tokio::task::spawn_blocking(read_answer).await {
                Ok(true) => Approval::Approved,
                _ => Approval::Denied,
            }
        })
    }
}

/// Render the confirmation block.
pub fn print_request(request: &ApprovalRequest) {
    let stderr = std::io::stderr();
    let mut out = stderr.lock();
    let tool = &request.request;
    let files = if tool.paths.is_empty() {
        "-".to_string()
    } else {
        tool.paths.join(", ")
    };
    let cwd = if tool.cwd.is_empty() {
        ".".to_string()
    } else {
        tool.cwd.clone()
    };
    let _ = writeln!(out, "\njAI wants to execute:\n");
    let _ = writeln!(out, "  tool:       {}", tool.call.name);
    let _ = writeln!(out, "  risk:       {}", tool.risk);
    let _ = writeln!(out, "  summary:    {}", tool.summary);
    let _ = writeln!(out, "  arguments:  {}", shorten(&tool.call.arguments, 400));
    let _ = writeln!(out, "  working dir: {cwd}");
    let _ = writeln!(out, "  files:      {files}");
    let _ = writeln!(out, "  mode:       {}", request.mode);
    let _ = writeln!(out, "\n{}", request.reason);
    let _ = writeln!(out, "\nAllow? [y/N] ");
    let _ = out.flush();
}

fn shorten(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "-".to_string()
    } else if trimmed.chars().count() <= max {
        trimmed.to_string()
    } else {
        let shortened: String = trimmed.chars().take(max).collect();
        format!("{shortened}…")
    }
}

fn read_answer() -> bool {
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jai_core::message::{PermissionMode, RiskLevel, ToolCall};
    use jai_core::policy::ToolRequest;

    fn request() -> ApprovalRequest {
        ApprovalRequest {
            request: ToolRequest::new(
                ToolCall::new("call_1", "shell_execute", r#"{"command":"cargo test"}"#),
                "run `cargo test`",
                RiskLevel::Execute,
            )
            .with_cwd("/work/project"),
            reason: "`shell_execute` runs a command on this machine".to_string(),
            mode: PermissionMode::Safe,
        }
    }

    #[test]
    fn rendering_does_not_panic_and_keeps_the_key_facts() {
        // The block goes to stderr; only the helpers are asserted here.
        print_request(&request());
        assert_eq!(shorten("", 10), "-");
        assert_eq!(shorten("abc", 10), "abc");
        assert_eq!(shorten(&"x".repeat(50), 10).chars().count(), 11);
        assert_eq!(shorten("实现一个 agent", 4), "实现一个…");
    }
}
