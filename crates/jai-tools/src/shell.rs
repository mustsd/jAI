//! [`SHELL_EXECUTE`]: run one command inside the sandbox.
//!
//! Two protections that matter more than the confirmation prompt:
//! * the child process gets a **scrubbed environment** — API keys from the jAI
//!   process are never inherited, so `env | curl …` cannot leak them;
//! * output is truncated and (in the engine) redacted before it is stored or
//!   sent to a provider.

use crate::SHELL_EXECUTE;
use crate::paths::PathGuard;
use jai_core::error::{Error, Result};
use jai_core::text::truncate_bytes;
use jai_core::tool::ToolOutcome;
use serde::Deserialize;
use std::process::Stdio;
use std::time::Duration;
use tokio::process::Command;

/// Environment variables that are safe to pass to a child process.
const SAFE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TMPDIR",
    "TMP",
    "TEMP",
    "PWD",
    "SystemRoot",
    "ComSpec",
    "PATHEXT",
    "WINDIR",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "ProgramData",
    "ProgramFiles",
    "NUMBER_OF_PROCESSORS",
    "OS",
];

/// Arguments of [`SHELL_EXECUTE`].
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellArgs {
    pub command: String,
    /// Working directory, relative to the workspace.
    #[serde(default)]
    pub cwd: Option<String>,
    /// Requested timeout in seconds (clamped to the configured maximum).
    #[serde(default)]
    pub timeout_seconds: Option<u64>,
}

/// Limits applied to shell execution.
#[derive(Debug, Clone, Copy)]
pub struct ShellLimits {
    pub timeout: Duration,
    pub max_timeout: Duration,
    pub max_output_bytes: usize,
}

/// Run `args.command` and return its combined output.
pub async fn run(guard: &PathGuard, args: &ShellArgs, limits: &ShellLimits) -> Result<ToolOutcome> {
    if args.command.trim().is_empty() {
        return Err(Error::tool(SHELL_EXECUTE, "command must not be empty"));
    }
    let cwd = match args.cwd.as_deref() {
        Some(raw) if !raw.trim().is_empty() => guard.resolve_existing(raw).map_err(|err| {
            Error::tool(SHELL_EXECUTE, format!("invalid working directory: {err}"))
        })?,
        _ => guard.sandbox().workspace().to_path_buf(),
    };
    let timeout = Duration::from_secs(
        args.timeout_seconds
            .map(|seconds| seconds.clamp(1, limits.max_timeout.as_secs().max(1)))
            .unwrap_or_else(|| limits.timeout.as_secs().max(1)),
    );

    let mut command = shell_command(&args.command);
    command
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    apply_safe_environment(&mut command, guard.sandbox().workspace(), |name| {
        std::env::var(name).ok()
    });

    let child = command
        .spawn()
        .map_err(|err| Error::tool(SHELL_EXECUTE, format!("could not start command: {err}")))?;

    let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(result) => {
            result.map_err(|err| Error::tool(SHELL_EXECUTE, format!("command failed: {err}")))?
        }
        Err(_) => {
            return Ok(ToolOutcome::failure(format!(
                "command timed out after {}s (killed): {}",
                timeout.as_secs(),
                args.command
            )));
        }
    };

    let status = output.status.code();
    let mut combined = String::new();
    combined.push_str(&format!(
        "exit: {}\n",
        match status {
            Some(code) => code.to_string(),
            None => format!("terminated by signal ({:?})", signal_of(&output.status)),
        }
    ));
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        combined.push_str("\n--- stdout ---\n");
        combined.push_str(&stdout);
    }
    if !stderr.trim().is_empty() {
        combined.push_str("\n--- stderr ---\n");
        combined.push_str(&stderr);
    }
    if stdout.trim().is_empty() && stderr.trim().is_empty() {
        combined.push_str("(no output)\n");
    }

    let (content, truncated) = truncate_bytes(&combined, limits.max_output_bytes);
    Ok(ToolOutcome {
        ok: output.status.success(),
        content,
        truncated,
    })
}

fn signal_of(status: &std::process::ExitStatus) -> Option<i32> {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        status.signal()
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// Give the child a scrubbed environment: only [`SAFE_ENV`] names are copied,
/// so credentials in the jAI process are never inherited.
fn apply_safe_environment(
    command: &mut Command,
    workspace: &std::path::Path,
    lookup: impl Fn(&str) -> Option<String>,
) {
    command.env_clear();
    for name in SAFE_ENV {
        if let Some(value) = lookup(name) {
            command.env(name, value);
        }
    }
    command.env("JAI_WORKSPACE", workspace);
}

#[cfg(unix)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("sh");
    process.arg("-c").arg(command);
    process
}

#[cfg(windows)]
fn shell_command(command: &str) -> Command {
    let mut process = Command::new("cmd");
    process.arg("/C").arg(command);
    process
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::{Path, PathBuf};

    fn workspace(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("jai-sh-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn guard(dir: &Path) -> PathGuard {
        PathGuard::new(dir.to_path_buf(), Vec::new(), Vec::new())
    }

    fn limits() -> ShellLimits {
        ShellLimits {
            timeout: Duration::from_secs(10),
            max_timeout: Duration::from_secs(60),
            max_output_bytes: 4096,
        }
    }

    #[tokio::test]
    async fn runs_a_command_and_reports_success() {
        let dir = workspace("ok");
        let guard = guard(&dir);
        let outcome = run(
            &guard,
            &ShellArgs {
                command: "echo hello".into(),
                cwd: None,
                timeout_seconds: None,
            },
            &limits(),
        )
        .await
        .unwrap();
        assert!(outcome.ok, "{}", outcome.content);
        assert!(outcome.content.contains("exit: 0"), "{}", outcome.content);
        assert!(outcome.content.contains("hello"), "{}", outcome.content);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn failure_exit_codes_are_reported_as_failures() {
        let dir = workspace("fail");
        let outcome = run(
            &guard(&dir),
            &ShellArgs {
                command: "exit 3".into(),
                cwd: None,
                timeout_seconds: None,
            },
            &limits(),
        )
        .await
        .unwrap();
        assert!(!outcome.ok);
        assert!(outcome.content.contains("exit: 3"), "{}", outcome.content);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn the_working_directory_is_respected_and_confined() {
        let dir = workspace("cwd");
        fs::create_dir_all(dir.join("sub")).unwrap();
        let guard = guard(&dir);
        let outcome = run(
            &guard,
            &ShellArgs {
                command: "pwd".into(),
                cwd: Some("sub".into()),
                timeout_seconds: None,
            },
            &limits(),
        )
        .await
        .unwrap();
        assert!(outcome.ok, "{}", outcome.content);
        assert!(outcome.content.contains("sub"), "{}", outcome.content);

        let escape = run(
            &guard,
            &ShellArgs {
                command: "pwd".into(),
                cwd: Some("../../".into()),
                timeout_seconds: None,
            },
            &limits(),
        )
        .await
        .unwrap_err();
        assert_eq!(escape.exit_code(), 6);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn api_keys_are_not_visible_to_the_child_process() {
        // A hostile environment: everything is "set", including credentials.
        let mut command = shell_command("env");
        apply_safe_environment(&mut command, Path::new("/work"), |name| {
            Some(format!("value-of-{name}"))
        });
        let names: Vec<String> = command
            .as_std()
            .get_envs()
            .map(|(name, _)| name.to_string_lossy().to_string())
            .collect();
        assert!(
            !names.iter().any(|name| name.contains("API_KEY")),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|name| name.contains("TOKEN")),
            "{names:?}"
        );
        assert!(
            !names.iter().any(|name| name == "AWS_SECRET_ACCESS_KEY"),
            "{names:?}"
        );
        assert!(
            names.iter().any(|name| name == "PATH"),
            "PATH must be inherited: {names:?}"
        );
        assert!(
            names.iter().any(|name| name == "JAI_WORKSPACE"),
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn the_child_sees_only_the_safe_environment() {
        let dir = workspace("env");
        let outcome = run(
            &guard(&dir),
            &ShellArgs {
                command: "env".into(),
                cwd: None,
                timeout_seconds: None,
            },
            &limits(),
        )
        .await
        .unwrap();
        assert!(
            outcome.content.contains("JAI_WORKSPACE="),
            "{}",
            outcome.content
        );
        assert!(
            !outcome.content.contains("API_KEY="),
            "child environment leaked: {}",
            outcome.content
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn long_running_commands_are_killed() {
        let dir = workspace("timeout");
        let guard = guard(&dir);
        let start = std::time::Instant::now();
        let outcome = run(
            &guard,
            &ShellArgs {
                command: "sleep 30".into(),
                cwd: None,
                timeout_seconds: Some(1),
            },
            &limits(),
        )
        .await
        .unwrap();
        assert!(!outcome.ok);
        assert!(outcome.content.contains("timed out"), "{}", outcome.content);
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "returned promptly"
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn output_is_truncated_to_the_configured_cap() {
        let dir = workspace("truncate");
        let limits = ShellLimits {
            max_output_bytes: 64,
            ..limits()
        };
        let outcome = run(
            &guard(&dir),
            &ShellArgs {
                command: "printf 'x%.0s' $(seq 1 500)".into(),
                cwd: None,
                timeout_seconds: None,
            },
            &limits,
        )
        .await
        .unwrap();
        assert!(outcome.truncated);
        assert!(outcome.content.ends_with("[truncated]"));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn empty_commands_are_rejected() {
        let dir = workspace("empty");
        let error = run(
            &guard(&dir),
            &ShellArgs {
                command: "   ".into(),
                cwd: None,
                timeout_seconds: None,
            },
            &limits(),
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("must not be empty"), "{error}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
