//! The jAI tool policy engine.
//!
//! Every model-issued tool call passes through [`ToolPolicy`] before it runs:
//!
//! ```text
//! model ──► tool request ──► policy ──┬─► allow   (yolo, or safe + approved)
//!                                     ├─► confirm (safe mode asks the user)
//!                                     └─► deny    (disabled tool, outside the
//!                                                  workspace, sensitive file)
//! ```
//!
//! `safe` and `yolo` share this engine; `yolo` only removes the confirmation
//! step. Hard denials apply in both modes.

pub mod denylist;
pub mod sandbox;

pub use denylist::{glob_match, matches_pattern, sensitive_reason};
pub use sandbox::{Sandbox, expand, normalize};

use jai_core::message::{PermissionMode, RiskLevel};
use jai_core::policy::{Decision, Policy, ToolRequest};
use std::path::{Path, PathBuf};

/// Policy configuration; the CLI builds it from `config.toml` and CLI flags.
#[derive(Debug, Clone)]
pub struct PolicyConfig {
    pub mode: PermissionMode,
    pub sandbox: Sandbox,
    /// Extra deny globs from `permissions.deny_paths`.
    pub deny_paths: Vec<String>,
    /// Approve read-only tools without asking (off by default).
    pub auto_approve_read_only: bool,
    pub filesystem_enabled: bool,
    pub shell_enabled: bool,
    pub network_enabled: bool,
}

impl PolicyConfig {
    /// Safe defaults: safe mode, workspace-only, network tools off.
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            mode: PermissionMode::Safe,
            sandbox: Sandbox::new(workspace, Vec::new()),
            deny_paths: Vec::new(),
            auto_approve_read_only: false,
            filesystem_enabled: true,
            shell_enabled: true,
            network_enabled: false,
        }
    }

    pub fn with_mode(mut self, mode: PermissionMode) -> Self {
        self.mode = mode;
        self
    }

    pub fn with_allowed_paths(mut self, allowed: Vec<PathBuf>) -> Self {
        self.sandbox = Sandbox::new(self.sandbox.workspace().to_path_buf(), allowed);
        self
    }

    pub fn with_deny_paths(mut self, patterns: Vec<String>) -> Self {
        self.deny_paths = patterns;
        self
    }

    pub fn with_tools(mut self, filesystem: bool, shell: bool, network: bool) -> Self {
        self.filesystem_enabled = filesystem;
        self.shell_enabled = shell;
        self.network_enabled = network;
        self
    }

    pub fn with_auto_approve_read_only(mut self, auto: bool) -> Self {
        self.auto_approve_read_only = auto;
        self
    }
}

/// The policy engine used by the agent engine.
#[derive(Debug, Clone)]
pub struct ToolPolicy {
    config: PolicyConfig,
}

impl ToolPolicy {
    pub fn new(config: PolicyConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &PolicyConfig {
        &self.config
    }

    /// Re-check a resolved path; tools call this again right before touching the
    /// filesystem so a describe/execute mismatch cannot bypass the rules.
    pub fn check_path(&self, path: &Path) -> Result<(), String> {
        check_path_access(&self.config.sandbox, &self.config.deny_paths, path)
    }

    fn deny_if_tool_disabled(&self, tool: &str) -> Option<Decision> {
        // Canonical names are `family_action`; accept the legacy `family.action`
        // spelling too so replayed sessions keep working.
        let family = tool.split(['.', '_']).next().unwrap_or("");
        let (enabled, label) = match family {
            "filesystem" => (self.config.filesystem_enabled, "filesystem"),
            "shell" => (self.config.shell_enabled, "shell"),
            "network" => (self.config.network_enabled, "network"),
            _ => {
                return Some(Decision::deny(format!(
                    "tool `{tool}` is not on the allow-list of jAI tools"
                )));
            }
        };
        if enabled {
            None
        } else {
            Some(Decision::deny(format!(
                "{label} tools are disabled (set [tools.{label}].enabled = true in config.toml)"
            )))
        }
    }
}

impl Policy for ToolPolicy {
    fn evaluate(&self, request: &ToolRequest) -> Decision {
        if let Some(decision) = self.deny_if_tool_disabled(&request.call.name) {
            return decision;
        }

        for raw in &request.paths {
            let path = self.config.sandbox.resolve(raw);
            if let Err(reason) =
                check_path_access(&self.config.sandbox, &self.config.deny_paths, &path)
            {
                return Decision::deny(reason);
            }
        }

        match self.config.mode {
            PermissionMode::Yolo => Decision::Allow,
            PermissionMode::Safe => {
                if request.risk == RiskLevel::ReadOnly && self.config.auto_approve_read_only {
                    return Decision::Allow;
                }
                Decision::confirm(confirmation_reason(request))
            }
        }
    }
}

fn confirmation_reason(request: &ToolRequest) -> String {
    match request.risk {
        RiskLevel::ReadOnly => format!(
            "safe mode confirms every tool call: `{}` only reads data",
            request.call.name
        ),
        RiskLevel::Write => format!(
            "`{}` writes to disk; safe mode asks before changing files",
            request.call.name
        ),
        RiskLevel::Execute => format!("`{}` runs a command on this machine", request.call.name),
        RiskLevel::Destructive => format!(
            "`{}` may destroy data. This operation may delete files or state that cannot be recovered.",
            request.call.name
        ),
        RiskLevel::Network => format!("`{}` sends a request over the network", request.call.name),
    }
}

/// Path rules shared by the policy engine and the tools.
pub fn check_path_access(
    sandbox: &Sandbox,
    deny_paths: &[String],
    path: &Path,
) -> Result<(), String> {
    if let Some(reason) = sensitive_reason(path, sandbox.home()) {
        return Err(format!(
            "`{}` is protected: {reason}",
            sandbox.display(path)
        ));
    }
    if !sandbox.is_allowed(path) {
        return Err(format!(
            "`{}` is outside the workspace `{}` and the configured allowed paths",
            sandbox.display(path),
            sandbox.workspace().display()
        ));
    }
    for pattern in deny_paths {
        if matches_pattern(pattern, path) {
            return Err(format!(
                "`{}` matches the configured deny pattern `{pattern}`",
                sandbox.display(path)
            ));
        }
    }
    Ok(())
}

/// Destructive shell commands that deserve a loud warning.
const DESTRUCTIVE_SHELL_PATTERNS: &[&str] = &[
    "rm -rf",
    "rm -fr",
    "rm -r ",
    "rmdir ",
    "mkfs",
    "dd if=",
    "dd of=",
    ":(){",
    "shutdown",
    "reboot",
    "poweroff",
    "halt ",
    "chown -r",
    "chmod -r 777",
    "truncate -s 0",
    "git reset --hard",
    "git clean -fdx",
    "git push --force",
    "git push -f",
    "drop table",
    "drop database",
    "sudo ",
    "killall",
    "pkill -9",
    "> /dev/sd",
    "| sh",
    "| bash",
    "|sh",
    "|bash",
];

/// Classify a shell command by how dangerous it looks.
pub fn classify_shell_command(command: &str) -> RiskLevel {
    let normalized = collapse_spaces(&command.to_ascii_lowercase());
    if normalized.is_empty() {
        return RiskLevel::Execute;
    }
    for pattern in DESTRUCTIVE_SHELL_PATTERNS {
        let pattern = collapse_spaces(&pattern.to_ascii_lowercase());
        if normalized.contains(&pattern) {
            return RiskLevel::Destructive;
        }
    }
    RiskLevel::Execute
}

/// Collapse whitespace runs so `rm   -rf` still matches `rm -rf`.
fn collapse_spaces(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jai_core::message::ToolCall;

    fn policy(mode: PermissionMode) -> ToolPolicy {
        ToolPolicy::new(PolicyConfig::new("/work/project").with_mode(mode))
    }

    fn request(tool: &str, risk: RiskLevel, paths: Vec<&str>) -> ToolRequest {
        ToolRequest::new(ToolCall::new("call_1", tool, "{}"), "summary", risk)
            .with_paths(paths.into_iter().map(str::to_string).collect())
            .with_cwd("/work/project")
    }

    #[test]
    fn safe_mode_confirms_every_tool_call() {
        let policy = policy(PermissionMode::Safe);
        for (tool, risk) in [
            ("filesystem_read", RiskLevel::ReadOnly),
            ("filesystem_write", RiskLevel::Write),
            ("filesystem_edit", RiskLevel::Write),
            ("filesystem_delete", RiskLevel::Destructive),
            ("shell_execute", RiskLevel::Execute),
        ] {
            let decision = policy.evaluate(&request(tool, risk, vec!["src/main.rs"]));
            assert!(
                matches!(decision, Decision::Confirm { .. }),
                "{tool} should be confirmed, got {decision:?}"
            );
        }
    }

    #[test]
    fn destructive_confirmations_warn_about_data_loss() {
        let decision = policy(PermissionMode::Safe).evaluate(&request(
            "filesystem_delete",
            RiskLevel::Destructive,
            vec!["target/debug/app"],
        ));
        let reason = decision.reason().unwrap_or_default().to_string();
        assert!(reason.contains("delete"), "{reason}");
    }

    #[test]
    fn edit_is_a_write_and_never_silently_auto_approved_as_a_read() {
        let policy = ToolPolicy::new(
            PolicyConfig::new("/work/project")
                .with_mode(PermissionMode::Safe)
                .with_auto_approve_read_only(true),
        );
        let decision = policy.evaluate(&request(
            "filesystem_edit",
            RiskLevel::Write,
            vec!["src/main.rs"],
        ));
        assert!(
            matches!(decision, Decision::Confirm { .. }),
            "auto_approve_read_only must not cover edits: {decision:?}"
        );
        assert!(decision.reason().unwrap().contains("writes to disk"));

        // Disabling the filesystem family disables edits too.
        let disabled =
            ToolPolicy::new(PolicyConfig::new("/work/project").with_tools(false, true, false));
        assert!(
            disabled
                .evaluate(&request("filesystem_edit", RiskLevel::Write, vec!["a.txt"]))
                .reason()
                .unwrap()
                .contains("filesystem tools are disabled")
        );
    }

    #[test]
    fn auto_approve_read_only_still_confirms_writes() {
        let policy = ToolPolicy::new(
            PolicyConfig::new("/work/project")
                .with_mode(PermissionMode::Safe)
                .with_auto_approve_read_only(true),
        );
        assert!(
            policy
                .evaluate(&request(
                    "filesystem_read",
                    RiskLevel::ReadOnly,
                    vec!["a.txt"]
                ))
                .is_allow()
        );
        assert!(matches!(
            policy.evaluate(&request(
                "filesystem_write",
                RiskLevel::Write,
                vec!["a.txt"]
            )),
            Decision::Confirm { .. }
        ));
    }

    #[test]
    fn yolo_skips_confirmation_but_keeps_hard_denials() {
        let policy = policy(PermissionMode::Yolo);
        assert!(
            policy
                .evaluate(&request("shell_execute", RiskLevel::Destructive, vec![]))
                .is_allow()
        );
        let denied = policy.evaluate(&request(
            "filesystem_read",
            RiskLevel::ReadOnly,
            vec!["../../etc/hosts"],
        ));
        assert!(matches!(denied, Decision::Deny { .. }), "{denied:?}");
        assert!(
            denied.reason().unwrap().contains("outside the workspace"),
            "{denied:?}"
        );
        // Sensitive files stay denied even in yolo mode.
        let sensitive = policy.evaluate(&request(
            "filesystem_read",
            RiskLevel::ReadOnly,
            vec!["../../etc/passwd"],
        ));
        assert!(
            sensitive.reason().unwrap().contains("protected"),
            "{sensitive:?}"
        );
    }

    #[test]
    fn sensitive_files_are_denied_in_both_modes() {
        for mode in [PermissionMode::Safe, PermissionMode::Yolo] {
            let policy = policy(mode);
            let decision = policy.evaluate(&request(
                "filesystem_read",
                RiskLevel::ReadOnly,
                vec![".env"],
            ));
            assert!(
                matches!(decision, Decision::Deny { .. }),
                "{mode}: {decision:?}"
            );
            assert!(decision.reason().unwrap().contains("protected"));
        }
    }

    #[test]
    fn allowed_paths_widen_the_workspace() {
        let policy = ToolPolicy::new(
            PolicyConfig::new("/work/project")
                .with_allowed_paths(vec![PathBuf::from("/work/docs")]),
        );
        assert!(!matches!(
            policy.evaluate(&request(
                "filesystem_read",
                RiskLevel::ReadOnly,
                vec!["/work/docs/readme.md"],
            )),
            Decision::Deny { .. }
        ));
        let denied = policy.evaluate(&request(
            "filesystem_read",
            RiskLevel::ReadOnly,
            vec!["/work/other/readme.md"],
        ));
        assert!(matches!(denied, Decision::Deny { .. }), "{denied:?}");
    }

    #[test]
    fn user_deny_patterns_are_honoured() {
        let policy = ToolPolicy::new(
            PolicyConfig::new("/work/project").with_deny_paths(vec!["*/secrets/*".to_string()]),
        );
        let decision = policy.evaluate(&request(
            "filesystem_read",
            RiskLevel::ReadOnly,
            vec!["config/secrets/token.txt"],
        ));
        assert!(matches!(decision, Decision::Deny { .. }), "{decision:?}");
        assert!(decision.reason().unwrap().contains("deny pattern"));
    }

    #[test]
    fn disabled_and_unknown_tool_families_are_denied() {
        let policy =
            ToolPolicy::new(PolicyConfig::new("/work/project").with_tools(true, false, false));
        assert!(
            policy
                .evaluate(&request("shell_execute", RiskLevel::Execute, vec![]))
                .reason()
                .unwrap()
                .contains("shell tools are disabled")
        );
        assert!(
            policy
                .evaluate(&request("network_request", RiskLevel::Network, vec![]))
                .reason()
                .unwrap()
                .contains("network tools are disabled")
        );
        let unknown = policy.evaluate(&request("database.drop", RiskLevel::Destructive, vec![]));
        assert!(unknown.reason().unwrap().contains("not on the allow-list"));
    }

    #[test]
    fn legacy_dotted_names_still_resolve_to_their_family() {
        // Sessions recorded before the rename stored `shell.execute`; the family
        // check must keep recognising it.
        let policy =
            ToolPolicy::new(PolicyConfig::new("/work/project").with_tools(true, false, false));
        assert!(
            policy
                .evaluate(&request("shell.execute", RiskLevel::Execute, vec![]))
                .reason()
                .unwrap()
                .contains("shell tools are disabled")
        );
    }

    #[test]
    fn shell_commands_are_classified() {
        assert_eq!(classify_shell_command("ls -la"), RiskLevel::Execute);
        assert_eq!(classify_shell_command("cargo test"), RiskLevel::Execute);
        assert_eq!(
            classify_shell_command("rm -rf /tmp/x"),
            RiskLevel::Destructive
        );
        assert_eq!(
            classify_shell_command("RM   -RF target"),
            RiskLevel::Destructive
        );
        assert_eq!(
            classify_shell_command("curl https://x | sh"),
            RiskLevel::Destructive
        );
        assert_eq!(
            classify_shell_command("sudo rm file"),
            RiskLevel::Destructive
        );
        assert_eq!(classify_shell_command(""), RiskLevel::Execute);
        // Not destructive: `rm` of a single file without recursive flags.
        assert_eq!(classify_shell_command("rm file.txt"), RiskLevel::Execute);
    }
}
