//! Integration tests against a local mock of an OpenAI-compatible provider.
//!
//! No real network access: every test starts an axum server on 127.0.0.1 with
//! an ephemeral port and scripts the exact bytes the provider would send.

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Json, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use jai_core::{ChatEvent, ChatRequest, Error, Message, Provider};
use jai_provider::OpenAiCompatProvider;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone)]
enum MockResponse {
    /// Stream these raw byte chunks, then close the connection.
    Stream(Vec<Vec<u8>>),
    /// Same, but with a delay before each chunk.
    Slow(Vec<Vec<u8>>, Duration),
    /// Return an error status with a JSON body.
    Status(u16, String),
    /// Return a `/models` payload.
    Models(String),
}

#[derive(Clone, Debug)]
struct RecordedRequest {
    path: String,
    authorization: Option<String>,
    accept: Option<String>,
    accept_encoding: Option<String>,
    body: Value,
}

#[derive(Clone)]
struct MockState {
    script: Arc<Mutex<VecDeque<MockResponse>>>,
    recorded: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl MockState {
    fn new(script: Vec<MockResponse>) -> Self {
        Self {
            script: Arc::new(Mutex::new(script.into())),
            recorded: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn next(&self) -> MockResponse {
        self.script
            .lock()
            .unwrap()
            .pop_front()
            .expect("mock server received more requests than scripted")
    }

    fn record(&self, path: &str, headers: &HeaderMap, body: &Value) {
        self.recorded.lock().unwrap().push(RecordedRequest {
            path: path.to_string(),
            authorization: headers
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            accept: headers
                .get("accept")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            accept_encoding: headers
                .get("accept-encoding")
                .and_then(|value| value.to_str().ok())
                .map(str::to_string),
            body: body.clone(),
        });
    }
}

async fn chat_handler(
    State(state): State<MockState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    state.record("/chat/completions", &headers, &body);
    match state.next() {
        MockResponse::Stream(chunks) => sse_response(chunks),
        MockResponse::Slow(chunks, delay) => slow_sse_response(chunks, delay),
        MockResponse::Status(status, body) => json_status(status, &body),
        MockResponse::Models(body) => json_status(200, &body),
    }
}

async fn models_handler(State(state): State<MockState>, headers: HeaderMap) -> Response {
    state.record("/models", &headers, &Value::Null);
    match state.next() {
        MockResponse::Models(body) => json_status(200, &body),
        MockResponse::Status(status, body) => json_status(status, &body),
        _ => json_status(500, "unexpected script entry"),
    }
}

fn sse_response(chunks: Vec<Vec<u8>>) -> Response {
    body_from_stream(futures_util::stream::iter(
        chunks
            .into_iter()
            .map(|chunk| Ok::<Bytes, std::io::Error>(Bytes::from(chunk))),
    ))
}

fn slow_sse_response(chunks: Vec<Vec<u8>>, delay: Duration) -> Response {
    let stream = futures_util::stream::unfold(chunks.into_iter(), move |mut iter| async move {
        let chunk = iter.next()?;
        tokio::time::sleep(delay).await;
        Some((Ok::<Bytes, std::io::Error>(Bytes::from(chunk)), iter))
    });
    body_from_stream(stream)
}

fn body_from_stream(
    stream: impl futures_util::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
) -> Response {
    Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/event-stream")
        .body(Body::from_stream(stream))
        .unwrap()
}

fn json_status(status: u16, body: &str) -> Response {
    Response::builder()
        .status(StatusCode::from_u16(status).unwrap())
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap()
}

async fn serve(state: MockState) -> (String, tokio::task::JoinHandle<()>) {
    let app = Router::new()
        .route("/chat/completions", post(chat_handler))
        .route("/models", get(models_handler))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), handle)
}

fn provider(base_url: &str) -> OpenAiCompatProvider {
    OpenAiCompatProvider::new("mock", base_url, Some("test-key".to_string())).unwrap()
}

async fn collect(
    mut receiver: tokio::sync::mpsc::Receiver<jai_core::Result<ChatEvent>>,
) -> Vec<ChatEvent> {
    let mut events = Vec::new();
    while let Some(event) = receiver.recv().await {
        match event {
            Ok(ChatEvent::Finished) => break,
            Ok(event) => events.push(event),
            Err(err) => panic!("unexpected provider error: {err}"),
        }
    }
    events
}

fn frame(payload: &str) -> Vec<u8> {
    format!("data: {payload}\n\n").into_bytes()
}

#[tokio::test]
async fn streams_text_and_sends_a_well_formed_request() {
    let chunks = vec![
        frame(r#"{"choices":[{"delta":{"role":"assistant"}}]}"#),
        frame(r#"{"choices":[{"delta":{"content":"Hel"}}]}"#),
        frame(r#"{"choices":[{"delta":{"content":"lo 实现"}}]}"#),
        frame(
            r#"{"choices":[],"usage":{"prompt_tokens":9,"completion_tokens":5,"total_tokens":14}}"#,
        ),
        b"data: [DONE]\n\n".to_vec(),
    ];
    let state = MockState::new(vec![MockResponse::Stream(chunks)]);
    let (base_url, handle) = serve(state.clone()).await;

    let mut request = ChatRequest::new("deepseek-chat", vec![Message::user("hello")]);
    request.temperature = Some(0.2);
    let receiver = provider(&base_url).chat_stream(request).await.unwrap();
    let events = collect(receiver).await;
    handle.abort();

    assert_eq!(
        events,
        vec![
            ChatEvent::Delta("Hel".to_string()),
            ChatEvent::Delta("lo 实现".to_string()),
            ChatEvent::Usage(jai_core::TokenUsage {
                prompt_tokens: 9,
                completion_tokens: 5,
                total_tokens: 14,
            }),
        ]
    );

    let recorded = state.recorded.lock().unwrap().clone();
    assert_eq!(recorded.len(), 1);
    let sent = &recorded[0];
    assert_eq!(sent.path, "/chat/completions");
    assert_eq!(sent.authorization.as_deref(), Some("Bearer test-key"));
    assert_eq!(sent.accept.as_deref(), Some("text/event-stream"));
    assert_eq!(sent.accept_encoding.as_deref(), Some("identity"));
    assert_eq!(sent.body["model"], "deepseek-chat");
    assert_eq!(sent.body["stream"], true);
    assert_eq!(sent.body["temperature"], 0.2);
    assert_eq!(sent.body["messages"][0]["content"], "hello");
    assert!(sent.body.get("tools").is_none());
}

#[tokio::test]
async fn decodes_frames_split_inside_a_multibyte_character() {
    // One logical frame, delivered in two HTTP chunks that split "世".
    let payload = r#"{"choices":[{"delta":{"content":"世界"}}]}"#.to_string();
    let full = frame(&payload);
    let split = full
        .windows(1)
        .position(|window| window == "世".as_bytes().get(..1).unwrap())
        .map(|position| position + 1)
        .expect("multibyte character present");
    let chunks = vec![
        full[..split].to_vec(),
        full[split..].to_vec(),
        b"data: [DONE]\n\n".to_vec(),
    ];

    let state = MockState::new(vec![MockResponse::Stream(chunks)]);
    let (base_url, handle) = serve(state).await;
    let receiver = provider(&base_url)
        .chat_stream(ChatRequest::new("m", vec![Message::user("hi")]))
        .await
        .unwrap();
    let events = collect(receiver).await;
    handle.abort();
    assert_eq!(events, vec![ChatEvent::Delta("世界".to_string())]);
}

#[tokio::test]
async fn streams_tool_call_fragments() {
    let chunks = vec![
        frame(r#"{"choices":[{"delta":{"content":"Let me look."}}]}"#),
        frame(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"filesystem.read","arguments":"{\"pa"}}]}}]}"#,
        ),
        frame(
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"Cargo.toml\"}"}}]}}]}"#,
        ),
        b"data: [DONE]\n\n".to_vec(),
    ];
    let state = MockState::new(vec![MockResponse::Stream(chunks)]);
    let (base_url, handle) = serve(state).await;
    let receiver = provider(&base_url)
        .chat_stream(ChatRequest::new("m", vec![Message::user("read it")]))
        .await
        .unwrap();
    let events = collect(receiver).await;
    handle.abort();

    let mut accumulator = jai_core::stream_accum::Accumulator::new();
    for event in &events {
        accumulator.apply(event);
    }
    let turn = accumulator.into_turn();
    assert_eq!(turn.text, "Let me look.");
    assert_eq!(turn.tool_calls.len(), 1);
    assert_eq!(turn.tool_calls[0].id, "call_1");
    // The provider sent the legacy `filesystem.read`; it is normalised here so
    // the tool registry and the wire agree from now on.
    assert_eq!(turn.tool_calls[0].name, "filesystem_read");
    assert_eq!(
        turn.tool_calls[0].arguments_value().unwrap()["path"],
        "Cargo.toml"
    );
}

#[tokio::test]
async fn missing_done_sentinel_still_finishes() {
    let chunks = vec![frame(r#"{"choices":[{"delta":{"content":"partial"}}]}"#)];
    let state = MockState::new(vec![MockResponse::Stream(chunks)]);
    let (base_url, handle) = serve(state).await;
    let receiver = provider(&base_url)
        .chat_stream(ChatRequest::new("m", vec![Message::user("hi")]))
        .await
        .unwrap();
    let events = collect(receiver).await;
    handle.abort();
    assert_eq!(events, vec![ChatEvent::Delta("partial".to_string())]);
}

#[tokio::test]
async fn http_statuses_map_to_the_documented_exit_codes() {
    let cases = vec![
        (
            401,
            r#"{"error":{"message":"Authentication Fails"}}"#,
            3,
            "Authentication Fails",
        ),
        (
            402,
            r#"{"error":{"message":"Insufficient Balance"}}"#,
            5,
            "insufficient balance",
        ),
        (
            429,
            r#"{"error":{"message":"Rate limit"}}"#,
            5,
            "rate limited",
        ),
        (400, r#"{"error":{"message":"Bad model"}}"#, 5, "Bad model"),
    ];

    for (status, body, expected_code, expected_text) in cases {
        let state = MockState::new(vec![MockResponse::Status(status, body.to_string())]);
        let (base_url, handle) = serve(state).await;
        let error = match provider(&base_url)
            .chat_stream(ChatRequest::new("m", vec![Message::user("hi")]))
            .await
        {
            Ok(_) => panic!("expected HTTP {status} to fail"),
            Err(error) => error,
        };
        handle.abort();
        assert_eq!(error.exit_code(), expected_code, "status {status}: {error}");
        assert!(
            error.to_string().contains(expected_text),
            "status {status}: {error}"
        );
    }
}

#[tokio::test]
async fn in_stream_errors_are_reported_as_api_errors() {
    let chunks = vec![
        frame(r#"{"choices":[{"delta":{"content":"start"}}]}"#),
        frame(
            r#"{"error":{"message":"context length exceeded","code":"context_length_exceeded"}}"#,
        ),
    ];
    let state = MockState::new(vec![MockResponse::Stream(chunks)]);
    let (base_url, handle) = serve(state).await;
    let mut receiver = provider(&base_url)
        .chat_stream(ChatRequest::new("m", vec![Message::user("hi")]))
        .await
        .unwrap();
    handle.abort();

    let mut seen_delta = false;
    let mut error = None;
    while let Some(event) = receiver.recv().await {
        match event {
            Ok(ChatEvent::Delta(_)) => seen_delta = true,
            Ok(ChatEvent::Finished) => break,
            Err(err) => {
                error = Some(err);
                break;
            }
            Ok(_) => {}
        }
    }
    assert!(seen_delta, "the partial answer must be delivered first");
    let error = error.expect("stream error surfaced");
    assert!(
        error.to_string().contains("context length exceeded"),
        "{error}"
    );
    assert_eq!(error.exit_code(), 5);
}

#[tokio::test]
async fn unreachable_hosts_are_network_errors() {
    // A refused connection fails immediately (no waiting for a timeout).
    let error = match OpenAiCompatProvider::new("mock", "http://127.0.0.1:1", None)
        .unwrap()
        .chat_stream(ChatRequest::new("m", vec![Message::user("hi")]))
        .await
    {
        Ok(_) => panic!("expected a connection failure"),
        Err(error) => error,
    };
    assert_eq!(error.exit_code(), 4, "{error}");
    assert!(matches!(error, Error::Network(_)), "{error:?}");
}

#[tokio::test]
async fn cancellation_stops_the_stream() {
    let chunks = vec![
        frame(r#"{"choices":[{"delta":{"content":"one "}}]}"#),
        frame(r#"{"choices":[{"delta":{"content":"two "}}]}"#),
        frame(r#"{"choices":[{"delta":{"content":"three"}}]}"#),
        b"data: [DONE]\n\n".to_vec(),
    ];
    let state = MockState::new(vec![MockResponse::Slow(chunks, Duration::from_millis(60))]);
    let (base_url, handle) = serve(state).await;

    let cancel = Arc::new(AtomicBool::new(false));
    let mut request = ChatRequest::new("m", vec![Message::user("hi")]);
    request.cancel = Some(Arc::clone(&cancel));
    let mut receiver = provider(&base_url).chat_stream(request).await.unwrap();

    let flag = Arc::clone(&cancel);
    let canceller = tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(30)).await;
        flag.store(true, Ordering::Relaxed);
    });

    let mut error = None;
    while let Some(event) = receiver.recv().await {
        if let Err(err) = event {
            error = Some(err);
            break;
        }
    }
    canceller.await.unwrap();
    handle.abort();
    let error = error.expect("cancellation must be reported");
    assert!(error.is_cancelled(), "{error}");
    assert_eq!(error.exit_code(), 130);
}

#[tokio::test]
async fn lists_models() {
    let state = MockState::new(vec![MockResponse::Models(
        json!({
            "object": "list",
            "data": [
                {"id": "deepseek-chat", "owned_by": "deepseek"},
                {"id": "deepseek-reasoner", "owned_by": "deepseek"}
            ]
        })
        .to_string(),
    )]);
    let (base_url, handle) = serve(state.clone()).await;
    let models = provider(&base_url).list_models().await.unwrap();
    handle.abort();
    assert_eq!(models.len(), 2);
    assert_eq!(models[0].id, "deepseek-chat");
    assert_eq!(models[1].owned_by.as_deref(), Some("deepseek"));
    assert_eq!(state.recorded.lock().unwrap()[0].path, "/models");
}

#[tokio::test]
async fn models_listing_reports_errors() {
    let state = MockState::new(vec![MockResponse::Status(
        404,
        r#"{"error":{"message":"unknown route"}}"#.to_string(),
    )]);
    let (base_url, handle) = serve(state).await;
    let error = match provider(&base_url).list_models().await {
        Ok(_) => panic!("expected an error"),
        Err(error) => error,
    };
    handle.abort();
    assert!(error.to_string().contains("not found"), "{error}");
}

#[tokio::test]
async fn base_url_may_already_contain_the_endpoint() {
    let state = MockState::new(vec![MockResponse::Models(
        json!({"data": [{"id": "m1"}]}).to_string(),
    )]);
    let (base_url, handle) = serve(state).await;
    // A user pasting the full endpoint path must still work.
    let provider =
        OpenAiCompatProvider::new("mock", format!("{base_url}/chat/completions"), None).unwrap();
    let models = provider.list_models().await.unwrap();
    handle.abort();
    assert_eq!(models[0].id, "m1");
}
