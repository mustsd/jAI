//! jAI's local web interface.
//!
//! Design constraints (see `docs/security.md`):
//! * the server binds loopback only unless the configuration explicitly says
//!   otherwise;
//! * every request needs a per-run token, which is printed at startup and
//!   required as `?token=…` or the `x-jai-token` header. This stops a web page
//!   in another tab from driving your local agent (CSRF) and stops DNS
//!   rebinding from being useful;
//! * the `Host` header must be a loopback name;
//! * tool calls still go through the policy engine; the browser only sees the
//!   approval prompt when the CLI would have shown one.

use axum::Router;
use axum::extract::{Path as UrlPath, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::{self, Next};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, extract::rejection::JsonRejection};
use jai_core::error::{Error, Result as JaiResult};
use jai_core::message::{ModelInfo, PermissionMode};
use jai_core::policy::{Approval, ApprovalRequest, Approver};
use jai_core::provider::BoxFuture;
use jai_core::store::SessionStore;
use jai_core::time;
use jai_core::{ToolCall, TurnHandle};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;

/// Everything the web layer needs from a front end (implemented by the CLI).
pub trait WebBackend: Send + Sync {
    fn provider_name(&self) -> String;
    fn model(&self) -> String;
    fn mode(&self) -> PermissionMode;
    fn workspace(&self) -> PathBuf;
    /// Tools currently offered to the model.
    fn tools(&self) -> Vec<String>;
    fn store(&self) -> Arc<dyn SessionStore>;
    fn list_models(&self) -> BoxFuture<'_, JaiResult<Vec<ModelInfo>>>;
    /// Start one agent turn, using `approver` for tool confirmations.
    fn start_turn(
        &self,
        session_id: &str,
        input: &str,
        approver: Arc<dyn Approver>,
    ) -> JaiResult<TurnHandle>;
}

/// Server options.
#[derive(Debug, Clone)]
pub struct WebOptions {
    pub bind: String,
    pub port: u16,
    /// Refuse to bind a non-loopback address.
    pub local_only: bool,
}

impl Default for WebOptions {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 7699,
            local_only: true,
        }
    }
}

/// Tool approvals waiting for an answer from the browser.
#[derive(Debug, Default)]
pub struct PendingApprovals {
    waiting: Mutex<HashMap<String, oneshot::Sender<bool>>>,
}

impl PendingApprovals {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Approver bound to this registry, with a timeout for unanswered prompts.
    pub fn approver(self: &Arc<Self>, timeout: Duration) -> Arc<dyn Approver> {
        Arc::new(WebApprover {
            pending: Arc::clone(self),
            timeout,
        })
    }

    /// Register interest in `call_id`; returns the receiver for the answer.
    fn register(&self, call_id: &str) -> Option<oneshot::Receiver<bool>> {
        let (tx, rx) = oneshot::channel();
        let mut waiting = self.waiting.lock().ok()?;
        waiting.insert(call_id.to_string(), tx);
        Some(rx)
    }

    fn forget(&self, call_id: &str) {
        if let Ok(mut waiting) = self.waiting.lock() {
            waiting.remove(call_id);
        }
    }

    /// Answer a pending approval; false when nothing was waiting.
    pub fn resolve(&self, call_id: &str, approved: bool) -> bool {
        let sender = self
            .waiting
            .lock()
            .ok()
            .and_then(|mut waiting| waiting.remove(call_id));
        match sender {
            Some(sender) => sender.send(approved).is_ok(),
            None => false,
        }
    }

    /// Number of approvals currently waiting (used by tests and metrics).
    #[cfg(test)]
    fn pending_count(&self) -> usize {
        self.waiting
            .lock()
            .map(|waiting| waiting.len())
            .unwrap_or(0)
    }
}

/// Approver that asks the browser and waits for the answer.
#[derive(Debug)]
struct WebApprover {
    pending: Arc<PendingApprovals>,
    timeout: Duration,
}

impl Approver for WebApprover {
    fn approve(&self, request: ApprovalRequest) -> BoxFuture<'_, Approval> {
        Box::pin(async move {
            let call_id = request.request.call.id.clone();
            let receiver = match self.pending.register(&call_id) {
                Some(receiver) => receiver,
                // No way to ask: refuse rather than run unprompted.
                None => return Approval::Denied,
            };
            match tokio::time::timeout(self.timeout, receiver).await {
                Ok(Ok(true)) => Approval::Approved,
                Ok(Ok(false)) => Approval::Denied,
                Ok(Err(_)) => Approval::Denied,
                Err(_) => {
                    self.pending.forget(&call_id);
                    Approval::Denied
                }
            }
        })
    }
}

struct WebState {
    backend: Arc<dyn WebBackend>,
    pending: Arc<PendingApprovals>,
    token: String,
    approval_timeout: Duration,
}

/// Errors returned to the browser as JSON.
struct ApiError {
    status: StatusCode,
    message: String,
    code: i32,
}

impl From<Error> for ApiError {
    fn from(error: Error) -> Self {
        let status = match error.exit_code() {
            2 => StatusCode::BAD_REQUEST,
            3..=5 => StatusCode::BAD_GATEWAY,
            6 => StatusCode::FORBIDDEN,
            130 => StatusCode::REQUEST_TIMEOUT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        Self {
            status,
            message: error.to_string(),
            code: error.exit_code(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({"error": self.message, "exit_code": self.code})),
        )
            .into_response()
    }
}

/// Build the application router. Exposed so tests (and embedders) can serve it
/// on their own listener.
pub fn build_router(
    backend: Arc<dyn WebBackend>,
    token: &str,
    approval_timeout: Duration,
) -> (Router, Arc<PendingApprovals>) {
    let pending = PendingApprovals::new();
    let state = Arc::new(WebState {
        backend,
        pending: Arc::clone(&pending),
        token: token.to_string(),
        approval_timeout,
    });

    let router = Router::new()
        .route("/", get(index))
        .route("/api/state", get(state_handler))
        .route("/api/models", get(models_handler))
        .route("/api/sessions", get(list_sessions).post(create_session))
        .route(
            "/api/sessions/{id}",
            get(show_session).delete(delete_session),
        )
        .route("/api/sessions/{id}/messages", post(chat))
        .route("/api/approvals/{call_id}", post(resolve_approval))
        .layer(middleware::from_fn_with_state(Arc::clone(&state), guard))
        .with_state(state);
    (router, pending)
}

/// Start the web server and block until Ctrl-C.
pub async fn serve(backend: Arc<dyn WebBackend>, options: WebOptions) -> JaiResult<()> {
    let addr = resolve_addr(&options)?;
    let token = jai_core::ids::session_id();
    let (router, _pending) = build_router(backend, &token, Duration::from_secs(300));

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|err| Error::network(format!("could not bind {addr}: {err}")))?;
    let local = listener
        .local_addr()
        .map_err(|err| Error::network(err.to_string()))?;

    println!(
        "jAI web UI: http://{local}/?token={token}\n\
         (the link contains a one-run access token; requests without it are rejected)"
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .map_err(|err| Error::network(format!("web server stopped: {err}")))?;
    Ok(())
}

fn resolve_addr(options: &WebOptions) -> JaiResult<SocketAddr> {
    let bind = options.bind.trim();
    let host = if bind.is_empty() || bind == "localhost" {
        "127.0.0.1"
    } else {
        bind
    };
    let addr: SocketAddr = format!("{host}:{}", options.port)
        .parse()
        .map_err(|_| Error::config(format!("`{bind}` is not a valid bind address")))?;
    if options.local_only && !addr.ip().is_loopback() {
        return Err(Error::config(format!(
            "refusing to bind {addr}: the web UI is loopback-only unless web.local_only = false"
        )));
    }
    Ok(addr)
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nstopping the web server");
}

/// Token and `Host` checks for every request.
async fn guard(State(state): State<Arc<WebState>>, request: Request, next: Next) -> Response {
    if !host_allowed(request.headers()) {
        return (
            StatusCode::FORBIDDEN,
            "invalid Host header (the web UI only accepts loopback host names)",
        )
            .into_response();
    }
    let provided = request
        .headers()
        .get("x-jai-token")
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
        .or_else(|| query_param(request.uri().query(), "token"));
    match provided {
        Some(value) if constant_time_eq(&value, &state.token) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            "missing or invalid token: reopen the URL printed by `jai --web`",
        )
            .into_response(),
    }
}

fn host_allowed(headers: &HeaderMap) -> bool {
    let Some(host) = headers.get("host").and_then(|value| value.to_str().ok()) else {
        return false;
    };
    let name = host.rsplit_once(':').map(|(name, _)| name).unwrap_or(host);
    let name = name.trim_matches(|c| c == '[' || c == ']');
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

fn query_param(query: Option<&str>, key: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            return Some(
                value
                    .replace("%2F", "/")
                    .replace("%2B", "+")
                    .replace("%3D", "=")
                    .replace("%20", " "),
            );
        }
    }
    None
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

async fn index() -> Html<&'static str> {
    Html(include_str!("../web/index.html"))
}

async fn state_handler(
    State(state): State<Arc<WebState>>,
) -> std::result::Result<Json<Value>, ApiError> {
    let sessions = state.backend.store().list_sessions()?;
    let now = time::unix_now();
    Ok(Json(json!({
        "provider": state.backend.provider_name(),
        "model": state.backend.model(),
        "mode": state.backend.mode().to_string(),
        "workspace": state.backend.workspace().display().to_string(),
        "tools": state.backend.tools(),
        "sessions": sessions
            .iter()
            .map(|summary| json!({
                "id": summary.session.id,
                "title": summary.session.display_title(),
                "provider": summary.session.provider,
                "model": summary.session.model,
                "messages": summary.message_count,
                "updated": time::format_relative(now, summary.session.updated_at),
            }))
            .collect::<Vec<_>>(),
    })))
}

async fn models_handler(
    State(state): State<Arc<WebState>>,
) -> std::result::Result<Json<Value>, ApiError> {
    let models = state.backend.list_models().await?;
    Ok(Json(json!({
        "provider": state.backend.provider_name(),
        "models": models.iter().map(|model| model.id.clone()).collect::<Vec<_>>(),
    })))
}

async fn list_sessions(
    State(state): State<Arc<WebState>>,
) -> std::result::Result<Json<Value>, ApiError> {
    let sessions = state.backend.store().list_sessions()?;
    Ok(Json(json!({
        "sessions": sessions
            .iter()
            .map(|summary| json!({
                "id": summary.session.id,
                "title": summary.session.display_title(),
                "messages": summary.message_count,
                "model": summary.session.model,
            }))
            .collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Deserialize)]
struct NewSession {
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

async fn create_session(
    State(state): State<Arc<WebState>>,
    body: std::result::Result<Json<NewSession>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let body = body.map(|Json(body)| body).unwrap_or(NewSession {
        provider: None,
        model: None,
        title: None,
    });
    let provider = body
        .provider
        .unwrap_or_else(|| state.backend.provider_name());
    let model = body.model.unwrap_or_else(|| state.backend.model());
    let session = state
        .backend
        .store()
        .create_session(&provider, &model, body.title.as_deref())?;
    Ok(Json(
        json!({"id": session.id, "provider": provider, "model": model}),
    ))
}

async fn show_session(
    State(state): State<Arc<WebState>>,
    UrlPath(id): UrlPath<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    let store = state.backend.store();
    let session = store
        .get_session(&id)?
        .ok_or_else(|| ApiError::not_found(&id))?;
    let messages = store.messages(&id)?;
    let audit = store.tool_audit(&id)?;
    let usage = store.session_usage(&id)?;
    Ok(Json(json!({
        "session": {
            "id": session.id,
            "title": session.display_title(),
            "provider": session.provider,
            "model": session.model,
            "created_at": time::format_timestamp(session.created_at),
            "updated_at": time::format_timestamp(session.updated_at),
        },
        "usage": {
            "prompt_tokens": usage.prompt_tokens,
            "completion_tokens": usage.completion_tokens,
            "total_tokens": usage.total_tokens,
        },
        "messages": messages
            .iter()
            .map(|message| json!({
                "role": message.role.as_str(),
                "content": message.content,
                "tool_call_id": message.tool_call_id,
                "name": message.name,
                "tool_calls": message.tool_calls.iter().map(tool_call_json).collect::<Vec<_>>(),
            }))
            .collect::<Vec<_>>(),
        "audit": audit
            .iter()
            .map(|entry| json!({
                "tool": entry.tool_name,
                "risk": entry.risk,
                "mode": entry.mode,
                "decision": entry.decision,
                "status": entry.status,
                "summary": entry.summary,
                "result": entry.result,
                "created_at": time::format_timestamp(entry.created_at),
            }))
            .collect::<Vec<_>>(),
    })))
}

fn tool_call_json(call: &ToolCall) -> Value {
    json!({"id": call.id, "name": call.name, "arguments": call.arguments})
}

async fn delete_session(
    State(state): State<Arc<WebState>>,
    UrlPath(id): UrlPath<String>,
) -> std::result::Result<Json<Value>, ApiError> {
    let deleted = state.backend.store().delete_session(&id)?;
    if !deleted {
        return Err(ApiError::not_found(&id));
    }
    Ok(Json(json!({"deleted": id})))
}

#[derive(Debug, Deserialize)]
struct ChatRequest {
    input: String,
}

async fn chat(
    State(state): State<Arc<WebState>>,
    UrlPath(id): UrlPath<String>,
    body: std::result::Result<Json<ChatRequest>, JsonRejection>,
) -> std::result::Result<Response, ApiError> {
    let Json(body) = body.map_err(|err| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid request body: {err}"),
        code: 2,
    })?;
    if body.input.trim().is_empty() {
        return Err(ApiError {
            status: StatusCode::BAD_REQUEST,
            message: "input must not be empty".to_string(),
            code: 2,
        });
    }
    let store = state.backend.store();
    let session = store
        .get_session(&id)?
        .ok_or_else(|| ApiError::not_found(&id))?;

    let approver = state.pending.approver(state.approval_timeout);
    let handle = state
        .backend
        .start_turn(&session.id, &body.input, approver)?;
    let canceller = handle.canceller();
    let mut events = handle.events;
    let (tx, rx) = mpsc::channel::<std::result::Result<Event, Infallible>>(64);

    tokio::spawn(async move {
        while let Some(event) = events.recv().await {
            let payload = match serde_json::to_string(&event) {
                Ok(payload) => payload,
                Err(_) => continue,
            };
            if tx.send(Ok(Event::default().data(payload))).await.is_err() {
                // The browser went away: stop the provider stream too.
                canceller.cancel();
                break;
            }
        }
    });

    let sse = Sse::new(ReceiverStream::new(rx)).keep_alive(KeepAlive::default());
    Ok(sse.into_response())
}

#[derive(Debug, Deserialize)]
struct ApprovalBody {
    approved: bool,
}

async fn resolve_approval(
    State(state): State<Arc<WebState>>,
    UrlPath(call_id): UrlPath<String>,
    body: std::result::Result<Json<ApprovalBody>, JsonRejection>,
) -> std::result::Result<Json<Value>, ApiError> {
    let Json(body) = body.map_err(|err| ApiError {
        status: StatusCode::BAD_REQUEST,
        message: format!("invalid request body: {err}"),
        code: 2,
    })?;
    if !state.pending.resolve(&call_id, body.approved) {
        return Err(ApiError {
            status: StatusCode::NOT_FOUND,
            message: format!("no tool call `{call_id}` is waiting for approval"),
            code: 1,
        });
    }
    Ok(Json(json!({"call_id": call_id, "approved": body.approved})))
}

impl ApiError {
    fn not_found(id: &str) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: format!("unknown session `{id}`"),
            code: 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_compared_in_constant_time() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("", "a"));
    }

    #[test]
    fn only_loopback_hosts_are_accepted() {
        let mut headers = HeaderMap::new();
        headers.insert("host", "127.0.0.1:7699".parse().unwrap());
        assert!(host_allowed(&headers));
        headers.insert("host", "localhost:7699".parse().unwrap());
        assert!(host_allowed(&headers));
        headers.insert("host", "evil.example.com".parse().unwrap());
        assert!(!host_allowed(&headers));
        headers.insert("host", "127.0.0.1.evil.com".parse().unwrap());
        assert!(!host_allowed(&headers));
        assert!(!host_allowed(&HeaderMap::new()));
    }

    #[test]
    fn query_parameters_are_extracted() {
        assert_eq!(
            query_param(Some("token=abc123&x=1"), "token").as_deref(),
            Some("abc123")
        );
        assert_eq!(query_param(Some("x=1"), "token"), None);
        assert_eq!(query_param(None, "token"), None);
    }

    #[test]
    fn non_loopback_binding_is_refused_by_default() {
        let error = resolve_addr(&WebOptions {
            bind: "0.0.0.0".to_string(),
            port: 7699,
            local_only: true,
        })
        .unwrap_err();
        assert!(error.to_string().contains("loopback-only"), "{error}");

        let addr = resolve_addr(&WebOptions {
            bind: "0.0.0.0".to_string(),
            port: 7699,
            local_only: false,
        })
        .unwrap();
        assert_eq!(addr.port(), 7699);
    }

    #[test]
    fn localhost_and_empty_binds_mean_loopback() {
        for bind in ["", "localhost", "127.0.0.1"] {
            let addr = resolve_addr(&WebOptions {
                bind: bind.to_string(),
                port: 1234,
                local_only: true,
            })
            .unwrap();
            assert!(addr.ip().is_loopback(), "{bind}");
        }
    }

    #[tokio::test]
    async fn pending_approvals_round_trip() {
        let pending = PendingApprovals::new();
        assert!(!pending.resolve("missing", true));

        let pending_clone = Arc::clone(&pending);
        let waiter = tokio::spawn(async move {
            match pending_clone.register("call_1") {
                Some(receiver) => receiver.await.unwrap_or(false),
                None => false,
            }
        });
        // Give the waiter a chance to register.
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(pending.pending_count(), 1);
        assert!(pending.resolve("call_1", true));
        assert!(waiter.await.unwrap());
        assert_eq!(pending.pending_count(), 0);
    }

    #[tokio::test]
    async fn unanswered_approvals_are_denied_when_the_timeout_expires() {
        let pending = PendingApprovals::new();
        let approver = pending.approver(Duration::from_millis(50));
        let request = ApprovalRequest {
            request: jai_core::policy::ToolRequest::new(
                ToolCall::new("call_9", "shell_execute", "{}"),
                "run something",
                jai_core::message::RiskLevel::Execute,
            ),
            reason: "safe mode".to_string(),
            mode: PermissionMode::Safe,
        };
        assert_eq!(approver.approve(request).await, Approval::Denied);
        assert_eq!(
            pending.pending_count(),
            0,
            "timed-out prompts are cleaned up"
        );
    }

    #[test]
    fn api_errors_map_to_http_statuses() {
        let policy = ApiError::from(Error::policy("outside the workspace"));
        assert_eq!(policy.status, StatusCode::FORBIDDEN);
        let auth = ApiError::from(Error::auth("deepseek", "bad key"));
        assert_eq!(auth.status, StatusCode::BAD_GATEWAY);
        let config = ApiError::from(Error::config("bad flag"));
        assert_eq!(config.status, StatusCode::BAD_REQUEST);
    }
}
