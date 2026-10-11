//! jAI's tools.
//!
//! Tools describe *what* they would do (metadata for the policy engine and the
//! confirmation prompt) and *how* to do it; they never decide whether they are
//! allowed to run. Every filesystem path goes through [`PathGuard`], and the
//! shell tool runs commands with a scrubbed environment.

pub mod fs;
pub mod http;
pub mod paths;
pub mod shell;

pub use jai_policy::Sandbox;
pub use paths::PathGuard;

use jai_core::error::{Error, Result};
use jai_core::message::{RiskLevel, ToolCall, ToolSpec};
use jai_core::policy::ToolRequest;
use jai_core::provider::BoxFuture;
use jai_core::tool::{ToolExecutor, ToolOutcome};
use jai_policy::classify_shell_command;
use serde::de::DeserializeOwned;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// Tool names as they appear in the provider's function-calling schema.
///
/// OpenAI-compatible APIs only accept `^[a-zA-Z0-9_-]+$` for function names
/// (DeepSeek answers `400 Invalid 'tools[0].function.name'` otherwise), so the
/// names use `_` and never a dot.
pub const FS_READ: &str = "filesystem_read";
pub const FS_LIST: &str = "filesystem_list";
pub const FS_WRITE: &str = "filesystem_write";
pub const FS_EDIT: &str = "filesystem_edit";
pub const FS_DELETE: &str = "filesystem_delete";
pub const SHELL_EXECUTE: &str = "shell_execute";
pub const NETWORK_REQUEST: &str = "network_request";

const MAX_LIST_ENTRIES: usize = 500;

/// Tool configuration, built by the CLI from `config.toml` plus flags.
#[derive(Debug, Clone)]
pub struct ToolsConfig {
    pub workspace: PathBuf,
    pub allowed_paths: Vec<PathBuf>,
    pub deny_paths: Vec<String>,
    pub filesystem: bool,
    pub shell: bool,
    pub network: bool,
    pub max_output_bytes: usize,
    /// Default command timeout.
    pub shell_timeout: Duration,
    /// Hard cap on a model-requested timeout.
    pub max_shell_timeout: Duration,
    pub http_timeout: Duration,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }
}

impl ToolsConfig {
    pub fn new(workspace: impl Into<PathBuf>) -> Self {
        Self {
            workspace: workspace.into(),
            allowed_paths: Vec::new(),
            deny_paths: Vec::new(),
            filesystem: true,
            shell: true,
            network: false,
            max_output_bytes: 32 * 1024,
            shell_timeout: Duration::from_secs(120),
            max_shell_timeout: Duration::from_secs(600),
            http_timeout: Duration::from_secs(30),
        }
    }

    pub fn with_allowed_paths(mut self, allowed: Vec<PathBuf>) -> Self {
        self.allowed_paths = allowed;
        self
    }

    pub fn with_deny_paths(mut self, deny: Vec<String>) -> Self {
        self.deny_paths = deny;
        self
    }

    pub fn with_tools(mut self, filesystem: bool, shell: bool, network: bool) -> Self {
        self.filesystem = filesystem;
        self.shell = shell;
        self.network = network;
        self
    }

    pub fn with_max_output_bytes(mut self, bytes: usize) -> Self {
        self.max_output_bytes = bytes.max(1);
        self
    }
}

/// The tool implementations, sharing one HTTP client and one path guard.
pub struct Tools {
    config: ToolsConfig,
    guard: PathGuard,
    client: reqwest::Client,
}

impl Tools {
    pub fn new(config: ToolsConfig) -> Result<Self> {
        let guard = PathGuard::new(
            config.workspace.clone(),
            config.allowed_paths.clone(),
            config.deny_paths.clone(),
        );
        let client = reqwest::Client::builder()
            .user_agent(concat!("jAI/", env!("CARGO_PKG_VERSION")))
            .timeout(config.http_timeout)
            .build()
            .map_err(|err| Error::internal(format!("could not build HTTP client: {err}")))?;
        Ok(Self {
            config,
            guard,
            client,
        })
    }

    pub fn config(&self) -> &ToolsConfig {
        &self.config
    }

    pub fn guard(&self) -> &PathGuard {
        &self.guard
    }

    pub fn sandbox(&self) -> &Sandbox {
        self.guard.sandbox()
    }

    /// Tool names currently offered to the model.
    pub fn enabled_tools(&self) -> Vec<&'static str> {
        let mut names = Vec::new();
        if self.config.filesystem {
            names.extend([FS_READ, FS_LIST, FS_WRITE, FS_EDIT, FS_DELETE]);
        }
        if self.config.shell {
            names.push(SHELL_EXECUTE);
        }
        if self.config.network {
            names.push(NETWORK_REQUEST);
        }
        names
    }

    fn shell_limits(&self) -> shell::ShellLimits {
        shell::ShellLimits {
            timeout: self.config.shell_timeout,
            max_timeout: self.config.max_shell_timeout,
            max_output_bytes: self.config.max_output_bytes,
        }
    }
}

/// [`ToolExecutor`] adapter handed to the agent engine.
pub struct ToolRegistry {
    tools: Arc<Tools>,
}

impl ToolRegistry {
    pub fn new(tools: Arc<Tools>) -> Self {
        Self { tools }
    }

    pub fn tools(&self) -> &Arc<Tools> {
        &self.tools
    }
}

impl ToolExecutor for ToolRegistry {
    fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .enabled_tools()
            .iter()
            .map(|name| match *name {
                FS_READ => ToolSpec {
                    name: FS_READ.to_string(),
                    description: "Read a text file from the workspace.".to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "File path, relative to the workspace"},
                            "max_bytes": {"type": "integer", "description": "Optional cap on returned bytes"}
                        },
                        "required": ["path"]
                    }),
                },
                FS_LIST => ToolSpec {
                    name: FS_LIST.to_string(),
                    description: "List the entries of a directory in the workspace.".to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Directory path (default: workspace root)"}
                        }
                    }),
                },
                FS_WRITE => ToolSpec {
                    name: FS_WRITE.to_string(),
                    description: "Write a text file in the workspace.".to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "File path, relative to the workspace"},
                            "content": {"type": "string", "description": "Full file content"},
                            "create_dirs": {"type": "boolean", "description": "Create missing parent directories"},
                            "overwrite": {"type": "boolean", "description": "Allow replacing an existing file (default true)"}
                        },
                        "required": ["path", "content"]
                    }),
                },
                FS_EDIT => ToolSpec {
                    name: FS_EDIT.to_string(),
                    description: "Replace an exact snippet in an existing text file. Cheaper \
                                  than filesystem_write for small changes: send only the lines \
                                  that change, not the whole file. old_string must match \
                                  exactly `expected_replacements` times (default 1) or the \
                                  edit is refused."
                        .to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "File path, relative to the workspace"},
                            "old_string": {"type": "string", "description": "Exact text to replace, copied from the file (include surrounding lines to make it unique)"},
                            "new_string": {"type": "string", "description": "Replacement text; must differ from old_string"},
                            "expected_replacements": {"type": "integer", "description": "How many occurrences must match (default 1)"}
                        },
                        "required": ["path", "old_string", "new_string"]
                    }),
                },
                FS_DELETE => ToolSpec {
                    name: FS_DELETE.to_string(),
                    description: "Delete a file or directory inside the workspace.".to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string", "description": "Path to delete"},
                            "recursive": {"type": "boolean", "description": "Delete a directory and its contents"}
                        },
                        "required": ["path"]
                    }),
                },
                SHELL_EXECUTE => ToolSpec {
                    name: SHELL_EXECUTE.to_string(),
                    description: "Run a shell command inside the workspace. API keys are not \
                                  available to the command."
                        .to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "command": {"type": "string", "description": "Command line to run"},
                            "cwd": {"type": "string", "description": "Working directory relative to the workspace"},
                            "timeout_seconds": {"type": "integer", "description": "Timeout in seconds"}
                        },
                        "required": ["command"]
                    }),
                },
                NETWORK_REQUEST => ToolSpec {
                    name: NETWORK_REQUEST.to_string(),
                    description: "Perform one HTTP request and return the response body.".to_string(),
                    parameters: json!({
                        "type": "object",
                        "properties": {
                            "url": {"type": "string", "description": "http:// or https:// URL"},
                            "method": {"type": "string", "description": "HTTP method (default GET)"},
                            "headers": {"type": "object", "description": "Request headers"},
                            "body": {"type": "string", "description": "Request body"}
                        },
                        "required": ["url"]
                    }),
                },
                other => ToolSpec {
                    name: other.to_string(),
                    description: String::new(),
                    parameters: json!({"type": "object"}),
                },
            })
            .collect()
    }

    fn describe(&self, call: &ToolCall) -> ToolRequest {
        // Sessions written before the names moved to `_` still hold the legacy
        // `filesystem.read` spelling, and models invent names; normalise once so
        // the policy engine, the audit log and the wire all agree.
        let call = &call.normalized();
        let workspace = self
            .tools
            .guard()
            .sandbox()
            .workspace()
            .display()
            .to_string();
        match call.name.as_str() {
            FS_READ => match parse_args::<fs::ReadArgs>(call) {
                Ok(args) => ToolRequest::new(
                    call.clone(),
                    format!("read `{}`", args.path),
                    RiskLevel::ReadOnly,
                )
                .with_paths(vec![args.path])
                .with_cwd(workspace),
                Err(reason) => unusable(call, reason, workspace),
            },
            FS_LIST => match parse_args::<fs::ListArgs>(call) {
                Ok(args) => {
                    let path = args.path.unwrap_or_else(|| ".".to_string());
                    ToolRequest::new(call.clone(), format!("list `{path}`"), RiskLevel::ReadOnly)
                        .with_paths(vec![path])
                        .with_cwd(workspace)
                }
                Err(reason) => unusable(call, reason, workspace),
            },
            FS_WRITE => match parse_args::<fs::WriteArgs>(call) {
                Ok(args) => ToolRequest::new(
                    call.clone(),
                    format!("write `{}` ({} bytes)", args.path, args.content.len()),
                    RiskLevel::Write,
                )
                .with_paths(vec![args.path])
                .with_cwd(workspace),
                Err(reason) => unusable(call, reason, workspace),
            },
            FS_EDIT => match parse_args::<fs::EditArgs>(call) {
                Ok(args) => ToolRequest::new(
                    call.clone(),
                    format!("edit `{}`", args.path),
                    RiskLevel::Write,
                )
                .with_paths(vec![args.path])
                .with_cwd(workspace),
                Err(reason) => unusable(call, reason, workspace),
            },
            FS_DELETE => match parse_args::<fs::DeleteArgs>(call) {
                Ok(args) => {
                    let what = if args.recursive.unwrap_or(false) {
                        "recursively delete"
                    } else {
                        "delete"
                    };
                    ToolRequest::new(
                        call.clone(),
                        format!("{what} `{}`", args.path),
                        RiskLevel::Destructive,
                    )
                    .with_paths(vec![args.path])
                    .with_cwd(workspace)
                }
                Err(reason) => unusable(call, reason, workspace),
            },
            SHELL_EXECUTE => match parse_args::<shell::ShellArgs>(call) {
                Ok(args) => {
                    let cwd = match args.cwd.as_deref() {
                        Some(raw) if !raw.trim().is_empty() => self
                            .tools
                            .guard()
                            .resolve(raw)
                            .map(|path| self.tools.guard().display(&path))
                            .unwrap_or_else(|_| workspace.clone()),
                        _ => workspace.clone(),
                    };
                    ToolRequest::new(
                        call.clone(),
                        format!("run `{}`", args.command),
                        classify_shell_command(&args.command),
                    )
                    .with_cwd(cwd)
                }
                Err(reason) => unusable(call, reason, workspace),
            },
            NETWORK_REQUEST => match parse_args::<http::HttpArgs>(call) {
                Ok(args) => ToolRequest::new(
                    call.clone(),
                    format!("{} {}", args.method.as_deref().unwrap_or("GET"), args.url),
                    RiskLevel::Network,
                )
                .with_cwd(workspace),
                Err(reason) => unusable(call, reason, workspace),
            },
            _ => ToolRequest::new(
                call.clone(),
                format!("unknown tool `{}`", call.name),
                RiskLevel::Destructive,
            )
            .with_cwd(workspace),
        }
    }

    fn execute(&self, call: &ToolCall) -> BoxFuture<'_, Result<ToolOutcome>> {
        // See `describe`: normalise here too, so an interrupted legacy call
        // replayed from the database still resolves to a real tool.
        let call = call.normalized();
        Box::pin(async move {
            let call = &call;
            match call.name.as_str() {
                FS_READ => {
                    let args: fs::ReadArgs = parse_args(call)?;
                    fs::read(
                        self.tools.guard(),
                        &args,
                        self.tools.config.max_output_bytes,
                    )
                    .await
                }
                FS_LIST => {
                    let args: fs::ListArgs = parse_args(call)?;
                    fs::list(self.tools.guard(), &args, MAX_LIST_ENTRIES).await
                }
                FS_WRITE => {
                    let args: fs::WriteArgs = parse_args(call)?;
                    fs::write(self.tools.guard(), &args).await
                }
                FS_EDIT => {
                    let args: fs::EditArgs = parse_args(call)?;
                    fs::edit(self.tools.guard(), &args).await
                }
                FS_DELETE => {
                    let args: fs::DeleteArgs = parse_args(call)?;
                    fs::delete(self.tools.guard(), &args).await
                }
                SHELL_EXECUTE => {
                    let args: shell::ShellArgs = parse_args(call)?;
                    let limits = self.tools.shell_limits();
                    shell::run(self.tools.guard(), &args, &limits).await
                }
                NETWORK_REQUEST => {
                    let args: http::HttpArgs = parse_args(call)?;
                    http::request(
                        &self.tools.client,
                        &args,
                        self.tools.config.http_timeout,
                        self.tools.config.max_output_bytes,
                    )
                    .await
                }
                other => Err(Error::tool(other, "unknown tool")),
            }
        })
    }
}

/// Parse tool arguments, treating a missing body as "all defaults".
fn parse_args<T: DeserializeOwned + Default>(call: &ToolCall) -> Result<T> {
    match call.arguments_value() {
        Ok(serde_json::Value::Null) => Ok(T::default()),
        Ok(value) => serde_json::from_value(value)
            .map_err(|err| Error::tool(&call.name, format!("invalid arguments: {err}"))),
        Err(err) => Err(Error::tool(
            &call.name,
            format!("arguments are not valid JSON: {err}"),
        )),
    }
}

/// Metadata for calls whose arguments cannot be parsed: treated as destructive
/// so the user sees exactly what was requested before anything runs.
fn unusable(call: &ToolCall, reason: impl std::fmt::Display, cwd: String) -> ToolRequest {
    ToolRequest::new(
        call.clone(),
        format!("unparsable request for `{}`: {reason}", call.name),
        RiskLevel::Destructive,
    )
    .with_cwd(cwd)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tools(config: ToolsConfig) -> ToolRegistry {
        ToolRegistry::new(Arc::new(Tools::new(config).unwrap()))
    }

    fn call(name: &str, arguments: &str) -> ToolCall {
        ToolCall::new("call_1", name, arguments)
    }

    #[test]
    fn specs_follow_the_enabled_tools() {
        let registry = tools(ToolsConfig::new("/tmp").with_tools(true, false, true));
        let names: Vec<String> = registry.specs().into_iter().map(|spec| spec.name).collect();
        assert!(names.contains(&FS_READ.to_string()));
        assert!(!names.contains(&SHELL_EXECUTE.to_string()));
        assert!(names.contains(&NETWORK_REQUEST.to_string()));

        let no_tools = tools(ToolsConfig::new("/tmp").with_tools(false, false, false));
        assert!(no_tools.specs().is_empty());
    }

    #[test]
    fn spec_schemas_are_valid_json_schema_objects() {
        let registry = tools(ToolsConfig::new("/tmp"));
        for spec in registry.specs() {
            assert_eq!(spec.parameters["type"], "object", "{}", spec.name);
            assert!(!spec.description.is_empty(), "{}", spec.name);
        }
        let read = registry
            .specs()
            .into_iter()
            .find(|spec| spec.name == FS_READ)
            .unwrap();
        assert_eq!(read.parameters["required"][0], "path");
    }
    #[test]
    fn every_offered_tool_name_is_wire_safe() {
        // DeepSeek rejects any name outside `^[a-zA-Z0-9_-]+$` with HTTP 400, so
        // a dotted or otherwise exotic tool name must never reach the wire.
        let registry = tools(ToolsConfig::new("/tmp").with_tools(true, true, true));
        let specs = registry.specs();
        assert_eq!(specs.len(), 7);
        for spec in specs {
            assert!(
                jai_core::is_valid_tool_name(&spec.name),
                "`{}` would be rejected by OpenAI-compatible APIs",
                spec.name
            );
        }
    }

    #[test]
    fn describe_reports_risk_and_paths() {
        let registry = tools(ToolsConfig::new("/work"));
        let read = registry.describe(&call(FS_READ, r#"{"path":"src/main.rs"}"#));
        assert_eq!(read.risk, RiskLevel::ReadOnly);
        assert_eq!(read.paths, vec!["src/main.rs"]);
        assert_eq!(read.summary, "read `src/main.rs`");

        let write = registry.describe(&call(FS_WRITE, r#"{"path":"a.txt","content":"hello"}"#));
        assert_eq!(write.risk, RiskLevel::Write);
        assert!(write.summary.contains("5 bytes"), "{}", write.summary);

        let edit = registry.describe(&call(
            FS_EDIT,
            r#"{"path":"a.txt","old_string":"a","new_string":"b"}"#,
        ));
        assert_eq!(edit.risk, RiskLevel::Write);
        assert_eq!(edit.summary, "edit `a.txt`");
        assert_eq!(edit.paths, vec!["a.txt"]);

        let delete = registry.describe(&call(FS_DELETE, r#"{"path":"target"}"#));
        assert_eq!(delete.risk, RiskLevel::Destructive);

        let shell = registry.describe(&call(SHELL_EXECUTE, r#"{"command":"cargo test"}"#));
        assert_eq!(shell.risk, RiskLevel::Execute);
        assert_eq!(shell.summary, "run `cargo test`");

        let destructive = registry.describe(&call(SHELL_EXECUTE, r#"{"command":"rm -rf /tmp/x"}"#));
        assert_eq!(destructive.risk, RiskLevel::Destructive);

        let network = registry.describe(&call(NETWORK_REQUEST, r#"{"url":"https://example.com"}"#));
        assert_eq!(network.risk, RiskLevel::Network);
        assert_eq!(network.summary, "GET https://example.com");
    }

    #[test]
    fn unparsable_arguments_are_treated_as_destructive() {
        let registry = tools(ToolsConfig::new("/work"));
        let described = registry.describe(&call(FS_READ, "{not json"));
        assert_eq!(described.risk, RiskLevel::Destructive);
        assert!(
            described.summary.contains("unparsable"),
            "{}",
            described.summary
        );
    }

    #[tokio::test]
    async fn unknown_tools_fail_at_execution() {
        let registry = tools(ToolsConfig::new("/tmp"));
        let error = registry
            .execute(&call("database.drop", "{}"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown tool"), "{error}");
    }

    #[tokio::test]
    async fn invalid_arguments_fail_before_touching_anything() {
        let registry = tools(ToolsConfig::new("/tmp"));
        let error = registry
            .execute(&call(FS_READ, r#"{"wrong":"field"}"#))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("invalid arguments"), "{error}");
    }

    #[tokio::test]
    async fn missing_arguments_use_defaults() {
        let dir = std::env::temp_dir().join(format!("jai-tools-lib-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        let registry = tools(ToolsConfig::new(&dir));

        let listed = registry
            .execute(&ToolCall::new("call_1", FS_LIST, ""))
            .await
            .unwrap();
        assert!(listed.ok);
        assert!(listed.content.contains("a.txt"), "{}", listed.content);

        let read = registry
            .execute(&call(FS_READ, r#"{"path":"a.txt"}"#))
            .await
            .unwrap();
        assert_eq!(read.content, "hello");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn edit_tool_runs_through_the_registry() {
        let dir = std::env::temp_dir().join(format!("jai-tools-edit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "one two three\n").unwrap();
        let registry = tools(ToolsConfig::new(&dir));

        let outcome = registry
            .execute(&call(
                FS_EDIT,
                r#"{"path":"a.txt","old_string":"two","new_string":"2"}"#,
            ))
            .await
            .unwrap();
        assert!(
            outcome.content.contains("replaced 1 occurrence"),
            "{outcome:?}"
        );
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "one 2 three\n"
        );

        // Missing arguments are a hard error, never a silent whole-file rewrite.
        let error = registry.execute(&call(FS_EDIT, "{}")).await.unwrap_err();
        assert!(error.to_string().contains("invalid arguments"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn legacy_dotted_calls_still_resolve() {
        let dir = std::env::temp_dir().join(format!("jai-tools-legacy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        let registry = tools(ToolsConfig::new(&dir));

        // A session recorded before the rename replays the dotted spelling; it
        // must still land on the renamed tool (and report the canonical name).
        let described = registry.describe(&call("filesystem.read", r#"{"path":"a.txt"}"#));
        assert_eq!(described.risk, RiskLevel::ReadOnly);
        assert_eq!(described.call.name, FS_READ);
        let outcome = registry
            .execute(&call("filesystem.read", r#"{"path":"a.txt"}"#))
            .await
            .unwrap();
        assert_eq!(outcome.content, "hello");

        // Unknown names stay unknown after normalisation.
        let error = registry
            .execute(&call("database.drop", "{}"))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown tool"), "{error}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
