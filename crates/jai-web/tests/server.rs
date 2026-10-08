//! HTTP-level tests for the local web interface.
//!
//! A real server is started on 127.0.0.1 and driven over HTTP, so the token
//! guard, the `Host` check, JSON endpoints, SSE streaming and the tool-approval
//! round trip are all exercised end to end.

use futures_util::StreamExt;
use jai_core::message::{
    ChatEvent, ChatRequest, ModelInfo, PermissionMode, RiskLevel, TokenUsage, ToolCall,
};
use jai_core::policy::{Approver, Decision, Policy, ToolRequest};
use jai_core::provider::BoxFuture;
use jai_core::store::{MemoryStore, SessionStore};
use jai_core::tool::{ToolExecutor, ToolOutcome};
use jai_core::{Engine, EngineSettings, Provider, TurnHandle};
use jai_web::{WebBackend, build_router};
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;

/// Provider that first asks for a tool, then answers with text.
struct ScriptedProvider {
    scripts: Mutex<Vec<Vec<ChatEvent>>>,
}

impl ScriptedProvider {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            scripts: Mutex::new(vec![
                vec![ChatEvent::ToolCallDelta {
                    index: 0,
                    id: Some("call_1".to_string()),
                    name: Some("shell_execute".to_string()),
                    arguments_delta: Some(r#"{"command":"echo hi"}"#.to_string()),
                }],
                vec![
                    ChatEvent::Delta("Hello from the web".to_string()),
                    ChatEvent::Usage(TokenUsage {
                        prompt_tokens: 5,
                        completion_tokens: 7,
                        total_tokens: 12,
                    }),
                ],
            ]),
        })
    }
}

impl Provider for ScriptedProvider {
    fn name(&self) -> &str {
        "scripted"
    }

    fn base_url(&self) -> &str {
        "http://scripted.test"
    }

    fn chat_stream(
        &self,
        _request: ChatRequest,
    ) -> BoxFuture<'_, jai_core::Result<mpsc::Receiver<jai_core::Result<ChatEvent>>>> {
        let script = {
            let mut scripts = self.scripts.lock().unwrap();
            if scripts.is_empty() {
                Vec::new()
            } else {
                scripts.remove(0)
            }
        };
        Box::pin(async move {
            let (tx, rx) = mpsc::channel(16);
            tokio::spawn(async move {
                for event in script {
                    if tx.send(Ok(event)).await.is_err() {
                        return;
                    }
                }
            });
            Ok(rx)
        })
    }

    fn list_models(&self) -> BoxFuture<'_, jai_core::Result<Vec<ModelInfo>>> {
        Box::pin(async {
            Ok(vec![ModelInfo {
                id: "scripted-1".to_string(),
                owned_by: Some("tests".to_string()),
            }])
        })
    }
}

/// Tool that records whether it was called and returns a fixed result.
struct RecordingTools {
    calls: Mutex<Vec<String>>,
}

impl ToolExecutor for RecordingTools {
    fn specs(&self) -> Vec<jai_core::ToolSpec> {
        vec![jai_core::ToolSpec {
            name: "shell_execute".to_string(),
            description: "Run a command".to_string(),
            parameters: json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        }]
    }

    fn describe(&self, call: &ToolCall) -> ToolRequest {
        ToolRequest::new(call.clone(), "run `echo hi`", RiskLevel::Execute).with_cwd("/work")
    }

    fn execute(&self, call: &ToolCall) -> BoxFuture<'_, jai_core::Result<ToolOutcome>> {
        self.calls.lock().unwrap().push(call.arguments.clone());
        Box::pin(async { Ok(ToolOutcome::success("exit: 0\n\n--- stdout ---\nhi")) })
    }
}

/// Safe-mode policy: confirm everything, deny unknown families.
struct ConfirmAll;

impl Policy for ConfirmAll {
    fn evaluate(&self, request: &ToolRequest) -> Decision {
        // `shell_execute` (and the legacy `shell.execute`) are confirmable.
        if request.call.name.starts_with("shell") {
            Decision::confirm("safe mode asks first")
        } else {
            Decision::deny("unknown tool")
        }
    }
}

struct TestBackend {
    provider: Arc<dyn Provider>,
    store: Arc<MemoryStore>,
    tools: Arc<RecordingTools>,
    workspace: PathBuf,
}

impl WebBackend for TestBackend {
    fn provider_name(&self) -> String {
        "scripted".to_string()
    }

    fn model(&self) -> String {
        "scripted-1".to_string()
    }

    fn mode(&self) -> PermissionMode {
        PermissionMode::Safe
    }

    fn workspace(&self) -> PathBuf {
        self.workspace.clone()
    }

    fn tools(&self) -> Vec<String> {
        vec!["shell_execute".to_string()]
    }

    fn store(&self) -> Arc<dyn SessionStore> {
        self.store.clone()
    }

    fn list_models(&self) -> BoxFuture<'_, jai_core::Result<Vec<ModelInfo>>> {
        self.provider.list_models()
    }

    fn start_turn(
        &self,
        session_id: &str,
        input: &str,
        approver: Arc<dyn Approver>,
    ) -> jai_core::Result<TurnHandle> {
        let settings = EngineSettings {
            redact: false,
            tools_enabled: true,
            mode: PermissionMode::Safe,
            ..EngineSettings::new("scripted", "scripted-1")
        };
        let engine = Arc::new(Engine::new(
            Arc::clone(&self.provider),
            self.store.clone(),
            Some(self.tools.clone()),
            Arc::new(ConfirmAll),
            approver,
            settings,
        ));
        // `start_turn` spawns the turn task, which keeps the engine alive.
        Ok(engine.start_turn(session_id, input))
    }
}

struct Server {
    base: String,
    token: String,
    _backend: Arc<TestBackend>,
}

async fn start_server() -> Server {
    let backend = Arc::new(TestBackend {
        provider: ScriptedProvider::new(),
        store: Arc::new(MemoryStore::new()),
        tools: Arc::new(RecordingTools {
            calls: Mutex::new(Vec::new()),
        }),
        workspace: PathBuf::from("/work"),
    });
    let token = "test-token-1234".to_string();
    let (router, _pending) = build_router(backend.clone(), &token, Duration::from_secs(30));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    Server {
        base: format!("http://{addr}"),
        token,
        _backend: backend,
    }
}

impl Server {
    fn client(&self) -> reqwest::Client {
        reqwest::Client::builder()
            .default_headers({
                let mut headers = reqwest::header::HeaderMap::new();
                headers.insert(
                    "x-jai-token",
                    reqwest::header::HeaderValue::from_str(&self.token).unwrap(),
                );
                headers
            })
            .build()
            .unwrap()
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }
}

#[tokio::test]
async fn requests_without_a_token_are_rejected() {
    let server = start_server().await;
    let client = reqwest::Client::new();

    let response = client.get(server.url("/api/state")).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
    let body = response.text().await.unwrap();
    assert!(body.contains("token"), "{body}");

    // The token may also be passed as a query parameter (what the browser does
    // when the page is first opened).
    let response = client
        .get(server.url(&format!("/api/state?token={}", server.token)))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    // A wrong token is refused too.
    let response = client
        .get(server.url("/api/state?token=wrong"))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn foreign_host_headers_are_refused() {
    let server = start_server().await;
    let response = server
        .client()
        .get(server.url("/api/state"))
        .header("host", "evil.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn the_index_page_is_served() {
    let server = start_server().await;
    let response = server.client().get(server.url("/")).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(content_type.starts_with("text/html"), "{content_type}");
    let body = response.text().await.unwrap();
    assert!(body.contains("jAI"), "the page must render the app");
    assert!(body.contains("/api/sessions"), "the page must call the API");
}

#[tokio::test]
async fn state_reports_the_backend_configuration() {
    let server = start_server().await;
    let state: Value = server
        .client()
        .get(server.url("/api/state"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(state["provider"], "scripted");
    assert_eq!(state["model"], "scripted-1");
    assert_eq!(state["mode"], "safe");
    assert_eq!(state["workspace"], "/work");
    assert_eq!(state["tools"][0], "shell_execute");
    assert_eq!(state["sessions"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn session_crud_and_chat_streaming() {
    let server = start_server().await;
    let client = server.client();

    // Create a session.
    let created: Value = client
        .post(server.url("/api/sessions"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();
    assert_eq!(created["provider"], "scripted");

    // It shows up in the list.
    let sessions: Value = client
        .get(server.url("/api/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(sessions["sessions"].as_array().unwrap().len(), 1);

    // Chat: the scripted provider asks for a tool first, which needs approval.
    let response = client
        .post(server.url(&format!("/api/sessions/{id}/messages")))
        .json(&json!({"input": "please greet me"}))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let mut stream = response.bytes_stream();

    let mut buffer = String::new();
    let mut approved = false;
    let mut events: Vec<Value> = Vec::new();
    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(index) = buffer.find("\n\n") {
            let frame = buffer[..index].to_string();
            buffer.drain(..index + 2);
            for line in frame.lines() {
                if let Some(payload) = line.strip_prefix("data:") {
                    let event: Value = serde_json::from_str(payload.trim()).unwrap();
                    if event["event"] == "approval_requested" && !approved {
                        approved = true;
                        let call_id = event["call"]["id"].as_str().unwrap().to_string();
                        let resolved = server
                            .client()
                            .post(server.url(&format!("/api/approvals/{call_id}")))
                            .json(&json!({"approved": true}))
                            .send()
                            .await
                            .unwrap();
                        assert_eq!(resolved.status(), reqwest::StatusCode::OK);
                    }
                    events.push(event);
                }
            }
        }
    }

    let names: Vec<String> = events
        .iter()
        .map(|event| event["event"].as_str().unwrap_or_default().to_string())
        .collect();
    assert!(names.contains(&"turn_started".to_string()), "{names:?}");
    assert!(
        names.contains(&"approval_requested".to_string()),
        "{names:?}"
    );
    assert!(names.contains(&"tool_finished".to_string()), "{names:?}");
    assert!(names.contains(&"delta".to_string()), "{names:?}");
    assert!(names.contains(&"finished".to_string()), "{names:?}");
    let assistant_contents: Vec<&str> = events
        .iter()
        .filter(|event| event["event"] == "assistant_message")
        .map(|event| event["content"].as_str().unwrap_or_default())
        .collect();
    assert!(
        assistant_contents.contains(&"Hello from the web"),
        "{assistant_contents:?}"
    );

    // The conversation is stored and readable through the API.
    let session: Value = client
        .get(server.url(&format!("/api/sessions/{id}")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let messages = session["messages"].as_array().unwrap();
    assert_eq!(messages[0]["role"], "user");
    assert_eq!(messages[0]["content"], "please greet me");
    assert!(
        messages
            .iter()
            .any(|message| message["content"] == "Hello from the web"),
        "{messages:?}"
    );
    let audit = session["audit"].as_array().unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["status"], "succeeded");
    assert_eq!(audit[0]["decision"], "confirm");

    // Deleting works and is reported.
    let deleted = client
        .delete(server.url(&format!("/api/sessions/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(deleted.status(), reqwest::StatusCode::OK);
    let missing = client
        .get(server.url(&format!("/api/sessions/{id}")))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn denying_an_approval_stops_the_tool() {
    let server = start_server().await;
    let client = server.client();
    let created: Value = client
        .post(server.url("/api/sessions"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let response = client
        .post(server.url(&format!("/api/sessions/{id}/messages")))
        .json(&json!({"input": "run it"}))
        .send()
        .await
        .unwrap();
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut denied = false;
    let mut tool_failed = false;
    while let Some(chunk) = stream.next().await {
        buffer.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
        while let Some(index) = buffer.find("\n\n") {
            let frame = buffer[..index].to_string();
            buffer.drain(..index + 2);
            for line in frame.lines() {
                if let Some(payload) = line.strip_prefix("data:") {
                    let event: Value = serde_json::from_str(payload.trim()).unwrap();
                    if event["event"] == "approval_requested" && !denied {
                        denied = true;
                        let call_id = event["call"]["id"].as_str().unwrap().to_string();
                        server
                            .client()
                            .post(server.url(&format!("/api/approvals/{call_id}")))
                            .json(&json!({"approved": false}))
                            .send()
                            .await
                            .unwrap();
                    }
                    if event["event"] == "tool_finished" && event["ok"] == false {
                        tool_failed = true;
                        assert!(
                            event["output"].as_str().unwrap().contains("DENIED"),
                            "{event}"
                        );
                    }
                }
            }
        }
    }
    assert!(denied, "the browser was asked");
    assert!(tool_failed, "the denied tool is reported as failed");
}

#[tokio::test]
async fn unknown_sessions_and_approvals_are_404() {
    let server = start_server().await;
    let client = server.client();

    let missing = client
        .get(server.url("/api/sessions/nope"))
        .send()
        .await
        .unwrap();
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);

    let unknown_approval = client
        .post(server.url("/api/approvals/call_404"))
        .json(&json!({"approved": true}))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown_approval.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn invalid_input_is_a_bad_request() {
    let server = start_server().await;
    let client = server.client();
    let created: Value = client
        .post(server.url("/api/sessions"))
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = created["id"].as_str().unwrap().to_string();

    let empty = client
        .post(server.url(&format!("/api/sessions/{id}/messages")))
        .json(&json!({"input": "   "}))
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), reqwest::StatusCode::BAD_REQUEST);

    let malformed = client
        .post(server.url(&format!("/api/sessions/{id}/messages")))
        .header("content-type", "application/json")
        .body("{not json")
        .send()
        .await
        .unwrap();
    assert_eq!(malformed.status(), reqwest::StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn models_are_listed_for_the_configured_provider() {
    let server = start_server().await;
    let models: Value = server
        .client()
        .get(server.url("/api/models"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(models["provider"], "scripted");
    assert_eq!(models["models"][0], "scripted-1");
}
