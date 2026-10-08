//! End-to-end tests for the `jai` binary.
//!
//! Every test runs the real executable against a local mock of an
//! OpenAI-compatible provider, with an isolated `JAI_HOME` so nothing touches
//! the developer's own `~/.jai`.

use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use serde_json::Value;
use std::collections::VecDeque;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
enum Reply {
    /// Frames of an SSE stream (`data: {...}` payloads without the prefix).
    Sse(Vec<String>),
    Status(u16, String),
}

#[derive(Clone)]
struct MockState {
    chat: Arc<Mutex<VecDeque<Reply>>>,
    models: Vec<String>,
    seen: Arc<Mutex<Vec<Value>>>,
}

impl MockState {
    fn new(chat: Vec<Reply>, models: Vec<&str>) -> Self {
        Self {
            chat: Arc::new(Mutex::new(chat.into())),
            models: models.into_iter().map(str::to_string).collect(),
            seen: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn next(&self) -> Option<Reply> {
        self.chat.lock().unwrap().pop_front()
    }

    fn requests(&self) -> Vec<Value> {
        self.seen.lock().unwrap().clone()
    }
}

fn text_reply(text: &str) -> Reply {
    Reply::Sse(vec![
        serde_json::json!({"choices": [{"delta": {"content": text}}]}).to_string(),
        "[DONE]".to_string(),
    ])
}

fn tool_reply(id: &str, name: &str, arguments: &str) -> Reply {
    Reply::Sse(vec![
        serde_json::json!({
            "choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": id, "type": "function",
                "function": {"name": name, "arguments": arguments}
            }]}}]
        })
        .to_string(),
        "[DONE]".to_string(),
    ])
}

/// The pattern OpenAI-compatible APIs enforce for `tools[].function.name`
/// (`^[a-zA-Z0-9_-]+$`, at most 64 characters). DeepSeek answers HTTP 400 for
/// anything else, so this is asserted on the real request bodies.
fn wire_safe_tool_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

/// Names of the tools a recorded request offered to the model.
fn offered_tool_names(body: &Value) -> Vec<String> {
    body["tools"]
        .as_array()
        .map(|tools| {
            tools
                .iter()
                .map(|tool| {
                    tool["function"]["name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string()
                })
                .collect()
        })
        .unwrap_or_default()
}

async fn chat_handler(State(state): State<MockState>, body: Option<axum::Json<Value>>) -> Response {
    if let Some(axum::Json(body)) = body {
        state.seen.lock().unwrap().push(body);
    }
    match state.next() {
        Some(Reply::Sse(frames)) => {
            let body: String = frames
                .iter()
                .map(|frame| format!("data: {frame}\n\n"))
                .collect();
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from(body))
                .unwrap()
        }
        Some(Reply::Status(status, body)) => Response::builder()
            .status(StatusCode::from_u16(status).unwrap())
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
        None => Response::builder()
            .status(StatusCode::INTERNAL_SERVER_ERROR)
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"error":{"message":"mock received an unscripted request"}}"#,
            ))
            .unwrap(),
    }
}

async fn models_handler(State(state): State<MockState>, _headers: HeaderMap) -> Response {
    let body = serde_json::json!({
        "object": "list",
        "data": state.models.iter().map(|id| serde_json::json!({"id": id})).collect::<Vec<_>>(),
    });
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

/// Start the mock provider on its own thread so the tests can block on the
/// `jai` child process without freezing the server.
fn serve_mock(state: MockState) -> String {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        runtime.block_on(async move {
            let app = Router::new()
                .route("/chat/completions", post(chat_handler))
                .route("/models", get(models_handler))
                .with_state(state);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            sender.send(format!("http://{addr}")).unwrap();
            let _ = axum::serve(listener, app).await;
        });
    });
    receiver.recv().expect("mock server started")
}

/// Per-test state directory plus a scratch workspace.
struct Sandbox {
    home: PathBuf,
    workspace: PathBuf,
}

impl Sandbox {
    fn new(name: &str) -> Self {
        let base = std::env::temp_dir().join(format!("jai-cli-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let home = base.join("home");
        let workspace = base.join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&workspace).unwrap();
        Self { home, workspace }
    }

    fn db(&self) -> PathBuf {
        self.home.join("jai.db")
    }

    /// Run `jai` with isolated state and a fake provider.
    fn run(&self, args: &[&str], base_url: &str, stdin: Option<&str>) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_jai"));
        command
            .args(args)
            .current_dir(&self.workspace)
            .env("JAI_HOME", &self.home)
            .env("JAI_CONFIG", self.home.join("config.toml"))
            .env("JAI_API_KEY", "test-key")
            // The mock provider is reached through JAI_BASE_URL, which every
            // subcommand honours (unlike a flag only one of them accepts).
            .env("JAI_BASE_URL", base_url)
            .env_remove("JAI_PROVIDER")
            .env_remove("JAI_MODEL")
            .env_remove("JAI_MODE")
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = command.spawn().expect("jai binary runs");
        if let Some(input) = stdin {
            child
                .stdin
                .as_mut()
                .expect("stdin piped")
                .write_all(input.as_bytes())
                .expect("stdin writable");
        }
        let output = child.wait_with_output().expect("jai finishes");
        Output {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.home.parent().unwrap_or(Path::new(".")));
    }
}

#[derive(Debug)]
struct Output {
    code: i32,
    stdout: String,
    stderr: String,
}

#[test]
fn help_and_version_work_without_configuration() {
    let sandbox = Sandbox::new("help");
    let mut command = Command::new(env!("CARGO_BIN_EXE_jai"));
    let output = command
        .arg("--help")
        .env("JAI_HOME", &sandbox.home)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{text}");
    assert!(text.contains("EXIT CODES"), "{text}");
    assert!(text.contains("jai -a"), "{text}");

    let output = Command::new(env!("CARGO_BIN_EXE_jai"))
        .arg("--version")
        .env("JAI_HOME", &sandbox.home)
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("jai"));
}
#[test]
fn a_one_shot_prompt_streams_an_answer_and_is_stored() {
    let sandbox = Sandbox::new("one-shot");
    let state = MockState::new(vec![text_reply("Hello from the mock")], vec!["test-model"]);
    let base_url = serve_mock(state.clone());

    let output = sandbox.run(
        &["custom", "-m", "test-model", "Explain Rust ownership"],
        &base_url,
        None,
    );
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert!(
        output.stdout.contains("Hello from the mock"),
        "{}",
        output.stdout
    );
    assert!(sandbox.db().exists(), "the session must be persisted");

    // The request carried the prompt and the model.
    let requests = state.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0]["model"], "test-model");
    assert_eq!(
        requests[0]["messages"][0]["content"],
        "Explain Rust ownership"
    );

    // Every tool name on the wire satisfies the provider's pattern: a single dot
    // here used to make DeepSeek reject the whole request with HTTP 400.
    let names = offered_tool_names(&requests[0]);
    assert!(names.contains(&"filesystem_read".to_string()), "{names:?}");
    assert!(names.contains(&"shell_execute".to_string()), "{names:?}");
    for name in &names {
        assert!(wire_safe_tool_name(name), "`{name}` is not wire-safe");
    }

    // The session shows up in `jai sessions` and can be exported.
    let sessions = sandbox.run(&["sessions"], &base_url, None);
    assert_eq!(sessions.code, 0);
    assert!(
        sessions.stdout.contains("Explain Rust ownership"),
        "{}",
        sessions.stdout
    );

    let id = sessions
        .stdout
        .lines()
        .nth(1)
        .and_then(|line| line.split_whitespace().next())
        .expect("a session id column")
        .to_string();
    let export = sandbox.run(&["session", &id, "--export"], &base_url, None);
    assert_eq!(export.code, 0);
    assert!(export.stdout.contains("# jAI session"), "{}", export.stdout);
    assert!(
        export.stdout.contains("Hello from the mock"),
        "{}",
        export.stdout
    );
}

#[test]
fn safe_mode_asks_before_running_a_tool() {
    let sandbox = Sandbox::new("safe");
    let state = MockState::new(
        vec![
            tool_reply("call_1", "shell_execute", r#"{"command":"echo hello"}"#),
            text_reply("I did not run it."),
        ],
        vec!["test-model"],
    );
    let base_url = serve_mock(state.clone());

    // Answer "n" to the confirmation prompt.
    let output = sandbox.run(
        &["custom", "-m", "test-model", "run echo", "--mode", "safe"],
        &base_url,
        Some("n\n"),
    );
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert!(
        output.stderr.contains("jAI wants to execute"),
        "{}",
        output.stderr
    );
    assert!(output.stderr.contains("shell_execute"), "{}", output.stderr);
    assert!(
        output.stdout.contains("I did not run it."),
        "{}",
        output.stdout
    );

    // The denied call was fed back to the model instead of being executed.
    let requests = state.requests();
    assert_eq!(requests.len(), 2);
    let tool_message = &requests[1]["messages"][2];
    assert_eq!(tool_message["role"], "tool");
    assert!(
        tool_message["content"].as_str().unwrap().contains("DENIED"),
        "{tool_message}"
    );
}

#[test]
fn yolo_mode_runs_tools_without_prompting() {
    let sandbox = Sandbox::new("yolo");
    let state = MockState::new(
        vec![
            tool_reply("call_1", "shell_execute", r#"{"command":"echo hello"}"#),
            text_reply("Done."),
        ],
        vec!["test-model"],
    );
    let base_url = serve_mock(state.clone());

    let output = sandbox.run(
        &["custom", "-m", "test-model", "run echo", "--mode", "yolo"],
        &base_url,
        None,
    );
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert!(
        !output.stderr.contains("jAI wants to execute"),
        "yolo must not prompt: {}",
        output.stderr
    );
    assert!(
        output.stderr.contains("shell_execute → ok"),
        "{}",
        output.stderr
    );
    assert!(output.stdout.contains("Done."), "{}", output.stdout);

    // The tool result really contains the command output.
    let requests = state.requests();
    assert!(
        requests[1]["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("hello"),
        "{}",
        requests[1]["messages"][2]
    );

    // The replayed history also carries wire-safe names: the assistant turn that
    // requested the tool and the tool result message that answers it.
    let replayed_call = requests[1]["messages"][1]["tool_calls"][0]["function"]["name"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    assert!(wire_safe_tool_name(&replayed_call), "{replayed_call}");
    assert_eq!(replayed_call, "shell_execute");
    assert_eq!(requests[1]["messages"][2]["name"], "shell_execute");
}

#[test]
fn policy_denials_are_reported_and_never_executed() {
    let sandbox = Sandbox::new("denied");
    let state = MockState::new(
        vec![
            tool_reply("call_1", "filesystem_read", r#"{"path":".env"}"#),
            text_reply("Understood."),
        ],
        vec!["test-model"],
    );
    let base_url = serve_mock(state.clone());

    // Even in yolo mode, sensitive files stay protected.
    let output = sandbox.run(
        &["custom", "-m", "test-model", "read .env", "--mode", "yolo"],
        &base_url,
        None,
    );
    assert_eq!(output.code, 0, "stderr: {}", output.stderr);
    assert!(output.stderr.contains("denied"), "{}", output.stderr);
    let requests = state.requests();
    assert!(
        requests[1]["messages"][2]["content"]
            .as_str()
            .unwrap()
            .contains("protected"),
        "{}",
        requests[1]["messages"][2]
    );
}

#[test]
fn provider_failures_map_to_exit_codes() {
    let sandbox = Sandbox::new("exit-codes");
    let state = MockState::new(
        vec![
            Reply::Status(
                401,
                r#"{"error":{"message":"Authentication Fails"}}"#.into(),
            ),
            Reply::Status(
                402,
                r#"{"error":{"message":"Insufficient Balance"}}"#.into(),
            ),
            Reply::Status(429, r#"{"error":{"message":"Rate limit reached"}}"#.into()),
        ],
        vec![],
    );
    let base_url = serve_mock(state);

    let unauthorized = sandbox.run(&["custom", "-m", "m", "hi"], &base_url, None);
    assert_eq!(unauthorized.code, 3, "{}", unauthorized.stderr);
    assert!(unauthorized.stderr.contains("Authentication Fails"));

    let unpaid = sandbox.run(&["custom", "-m", "m", "hi"], &base_url, None);
    assert_eq!(unpaid.code, 5, "{}", unpaid.stderr);
    assert!(unpaid.stderr.contains("insufficient balance"));

    let limited = sandbox.run(&["custom", "-m", "m", "hi"], &base_url, None);
    assert_eq!(limited.code, 5, "{}", limited.stderr);
}

#[test]
fn unusable_configuration_exits_with_code_two() {
    let sandbox = Sandbox::new("config");
    let state = MockState::new(vec![], vec![]);
    let base_url = serve_mock(state);

    // Unknown provider.
    let output = sandbox.run(&["not-a-provider", "hi"], &base_url, None);
    assert_eq!(output.code, 2, "{} {}", output.stdout, output.stderr);
    assert!(
        output.stderr.contains("unknown provider") || output.stderr.contains("not-a-provider"),
        "{}",
        output.stderr
    );
}

#[test]
fn models_and_providers_commands_work() {
    let sandbox = Sandbox::new("models");
    let state = MockState::new(vec![], vec!["alpha-1", "beta-2"]);
    let base_url = serve_mock(state);

    let models = sandbox.run(&["models", "custom"], &base_url, None);
    assert_eq!(models.code, 0, "{}", models.stderr);
    assert!(models.stdout.contains("alpha-1"), "{}", models.stdout);
    assert!(models.stdout.contains("beta-2"), "{}", models.stdout);

    let providers = sandbox.run(&["providers"], &base_url, None);
    assert_eq!(providers.code, 0);
    for expected in ["deepseek", "openai", "ollama", "DEEPSEEK_API_KEY"] {
        assert!(providers.stdout.contains(expected), "{}", providers.stdout);
    }
}

#[test]
fn doctor_reports_a_healthy_installation() {
    let sandbox = Sandbox::new("doctor");
    let state = MockState::new(vec![], vec![]);
    let base_url = serve_mock(state);

    // Create the database first so doctor has something to inspect.
    sandbox.run(&["sessions"], &base_url, None);
    let output = sandbox.run(&["doctor"], &base_url, None);
    assert_eq!(output.code, 0, "{}", output.stdout);
    assert!(output.stdout.contains("database"), "{}", output.stdout);
    assert!(
        output.stdout.contains("no problems found"),
        "{}",
        output.stdout
    );
}

#[test]
fn sessions_can_be_deleted_and_purged() {
    let sandbox = Sandbox::new("purge");
    let state = MockState::new(
        vec![text_reply("first"), text_reply("second")],
        vec!["test-model"],
    );
    let base_url = serve_mock(state);

    sandbox.run(&["custom", "-m", "test-model", "one"], &base_url, None);
    sandbox.run(&["custom", "-m", "test-model", "two"], &base_url, None);

    let sessions = sandbox.run(&["sessions", "--json"], &base_url, None);
    let parsed: Value = serde_json::from_str(&sessions.stdout).expect("valid JSON");
    assert_eq!(parsed.as_array().unwrap().len(), 2, "{}", sessions.stdout);

    let first_id = parsed[0]["id"].as_str().unwrap().to_string();
    let deleted = sandbox.run(&["session", &first_id, "--delete"], &base_url, None);
    assert_eq!(deleted.code, 0, "{}", deleted.stderr);
    assert!(deleted.stdout.contains("deleted"), "{}", deleted.stdout);

    let purged = sandbox.run(&["purge", "--yes"], &base_url, None);
    assert_eq!(purged.code, 0);
    assert!(
        purged.stdout.contains("deleted 1 session"),
        "{}",
        purged.stdout
    );

    let after = sandbox.run(&["sessions"], &base_url, None);
    assert!(
        after.stdout.contains("no stored sessions"),
        "{}",
        after.stdout
    );
}

#[test]
fn prompts_can_be_piped_on_stdin() {
    let sandbox = Sandbox::new("stdin");
    let state = MockState::new(vec![text_reply("piped answer")], vec!["test-model"]);
    let base_url = serve_mock(state.clone());

    let output = sandbox.run(
        &["custom", "-m", "test-model"],
        &base_url,
        Some("hello from stdin"),
    );
    assert_eq!(output.code, 0, "{}", output.stderr);
    assert!(output.stdout.contains("piped answer"), "{}", output.stdout);
    assert_eq!(
        state.requests()[0]["messages"][0]["content"],
        "hello from stdin"
    );
}

#[test]
fn no_persist_leaves_no_database_behind() {
    let sandbox = Sandbox::new("no-persist");
    let state = MockState::new(vec![text_reply("ephemeral")], vec!["test-model"]);
    let base_url = serve_mock(state);

    let output = sandbox.run(
        &["custom", "-m", "test-model", "hi", "--no-persist"],
        &base_url,
        None,
    );
    assert_eq!(output.code, 0, "{}", output.stderr);
    assert!(!sandbox.db().exists(), "no database should be created");
}

#[test]
fn the_interactive_agent_runs_with_scripted_input() {
    let sandbox = Sandbox::new("repl");
    let state = MockState::new(vec![text_reply("hi from the repl")], vec!["test-model"]);
    let base_url = serve_mock(state.clone());

    let output = sandbox.run(
        &["custom", "-m", "test-model", "-a"],
        &base_url,
        Some("/help\nhello there\n/exit\n"),
    );
    assert_eq!(
        output.code, 0,
        "stdout: {} stderr: {}",
        output.stdout, output.stderr
    );
    assert!(output.stdout.contains("Commands:"), "{}", output.stdout);
    assert!(
        output.stdout.contains("hi from the repl"),
        "{}",
        output.stdout
    );
    assert!(
        output.stdout.contains("jai -a --session"),
        "the REPL must print how to resume: {}",
        output.stdout
    );
    assert_eq!(state.requests()[0]["messages"][0]["content"], "hello there");
}

#[test]
fn web_mode_refuses_a_non_loopback_bind() {
    let sandbox = Sandbox::new("web-bind");
    let state = MockState::new(vec![], vec![]);
    let base_url = serve_mock(state);

    let output = sandbox.run(
        &["custom", "-m", "test-model", "--web", "--bind", "0.0.0.0"],
        &base_url,
        None,
    );
    assert_eq!(output.code, 2, "{} {}", output.stdout, output.stderr);
    assert!(output.stderr.contains("loopback-only"), "{}", output.stderr);
}
