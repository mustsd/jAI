//! The agent loop.
//!
//! One implementation is shared by the CLI, the web UI and the tests. It owns
//! the turn structure — persist the user message, stream the assistant answer,
//! ask the policy engine about tool calls, persist the audit trail — and talks
//! to the outside world only through the traits in [`crate::provider`],
//! [`crate::store`], [`crate::policy`] and [`crate::tool`].

use crate::error::{Error, Result};
use crate::message::{
    CancelFlag, ChatEvent, ChatRequest, Message, PermissionMode, TokenUsage, ToolCall, ToolSpec,
};
use crate::policy::{Approval, ApprovalRequest, Approver, Decision, Policy, ToolRequest};
use crate::provider::Provider;
use crate::redact::Redactor;
use crate::store::{SessionStore, ToolAuditRecord, status};
use crate::stream_accum::Accumulator;
use crate::text::truncate_bytes;
use crate::tokens;
use crate::usage::ContextUsage;
use crate::tool::ToolExecutor;
use serde::Serialize;
use serde_json::{Map, Value};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

/// Immutable-per-turn engine configuration.
#[derive(Debug, Clone)]
pub struct EngineSettings {
    pub provider: String,
    pub model: String,
    pub system_prompt: Option<String>,
    pub max_tool_rounds: u32,
    pub tools_enabled: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Extra request-body fields (`--extra-json`).
    pub extra: Map<String, Value>,
    pub mode: PermissionMode,
    pub redact: bool,
    pub store_tool_results: bool,
    /// Persist assistant reasoning/thinking text (it is never sent back to the
    /// provider, only stored and displayed).
    pub store_reasoning: bool,
    /// Model context window used for the `Cxt:` figure.
    pub context_window: u64,
    pub max_tool_output_bytes: usize,
}

impl EngineSettings {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            provider: provider.into(),
            model: model.into(),
            system_prompt: None,
            max_tool_rounds: 8,
            tools_enabled: true,
            temperature: None,
            max_tokens: None,
            extra: Map::new(),
            mode: PermissionMode::Safe,
            redact: true,
            store_tool_results: true,
            store_reasoning: true,
            context_window: tokens::DEFAULT_CONTEXT_WINDOW,
            max_tool_output_bytes: 32 * 1024,
        }
    }
}

/// Events a UI renders while a turn is running.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum EngineEvent {
    TurnStarted {
        session_id: String,
    },
    Delta {
        text: String,
    },
    /// Reasoning/thinking text (DeepSeek `reasoning_content`, ...). Never sent
    /// back to the provider.
    ReasoningDelta {
        text: String,
    },
    AssistantMessage {
        content: String,
        /// Reasoning text the model produced, when it produced any.
        reasoning: Option<String>,
        tool_calls: Vec<ToolCall>,
    },
    /// A tool call reached the policy engine.
    ToolRequested {
        call: ToolCall,
        request: ToolRequest,
        decision: Decision,
    },
    /// The user must approve before the tool runs.
    ApprovalRequested {
        call: ToolCall,
        request: ToolRequest,
        reason: String,
        mode: PermissionMode,
    },
    ToolFinished {
        call: ToolCall,
        ok: bool,
        output: String,
    },
    Usage {
        usage: TokenUsage,
    },
    /// Size of the active context (system prompt + tools + conversation).
    Context {
        usage: ContextUsage,
    },
    /// Something the user should know that is not an error.
    Notice {
        message: String,
    },
    Failed {
        message: String,
        exit_code: i32,
    },
    Finished {
        session_id: String,
        usage: TokenUsage,
    },
}

/// Handle for one in-flight turn.
pub struct TurnHandle {
    pub events: mpsc::Receiver<EngineEvent>,
    cancel: CancelFlag,
}

impl TurnHandle {
    /// Ask the provider stream to stop as soon as possible.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// A clone of the cancellation flag, usable from another task (for example
    /// a Ctrl-C handler) while this handle is being consumed.
    pub fn canceller(&self) -> TurnCanceller {
        TurnCanceller {
            cancel: Arc::clone(&self.cancel),
        }
    }

    /// Wrap an existing event receiver in a turn handle. Front ends use this
    /// when they own the event pump (tests, stored replays).
    pub fn from_receiver(events: mpsc::Receiver<EngineEvent>) -> Self {
        Self {
            events,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// Cancellation handle that does not borrow the event stream.
#[derive(Clone)]
pub struct TurnCanceller {
    cancel: CancelFlag,
}

impl TurnCanceller {
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

/// The agent: provider + storage + tools + policy.
pub struct Engine {
    provider: Arc<dyn Provider>,
    store: Arc<dyn SessionStore>,
    tools: Option<Arc<dyn ToolExecutor>>,
    policy: Arc<dyn Policy>,
    approver: Arc<dyn Approver>,
    settings: Mutex<EngineSettings>,
    redactor: Redactor,
}

impl Engine {
    pub fn new(
        provider: Arc<dyn Provider>,
        store: Arc<dyn SessionStore>,
        tools: Option<Arc<dyn ToolExecutor>>,
        policy: Arc<dyn Policy>,
        approver: Arc<dyn Approver>,
        settings: EngineSettings,
    ) -> Self {
        let redactor = if settings.redact {
            Redactor::from_env()
        } else {
            Redactor::none()
        };
        Self {
            provider,
            store,
            tools,
            policy,
            approver,
            settings: Mutex::new(settings),
            redactor,
        }
    }

    /// Replace the redactor (tests, or a redactor pre-seeded with known keys).
    pub fn with_redactor(mut self, redactor: Redactor) -> Self {
        self.redactor = redactor;
        self
    }

    pub fn settings(&self) -> EngineSettings {
        self.settings
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| EngineSettings::new("unknown", "unknown"))
    }

    pub fn store(&self) -> &Arc<dyn SessionStore> {
        &self.store
    }
    /// What the *next* request would carry for this session: the system prompt,
    /// the tool schemas and the stored conversation, estimated locally.
    ///
    /// Front ends show this as the `Cxt:` figure before the first turn, so the
    /// status line has a real number from the moment the agent starts rather
    /// than appearing only once a provider reports usage.
    pub fn context_estimate(&self, session_id: &str) -> Result<ContextUsage> {
        let settings = self.settings();
        let messages = self.history(session_id, &settings)?;
        let specs = self.tool_specs(&settings);
        Ok(ContextUsage::estimate(
            tokens::estimate_context(&messages, &specs),
            settings.context_window,
        ))
    }


    pub fn provider(&self) -> &Arc<dyn Provider> {
        &self.provider
    }

    pub fn set_model(&self, model: impl Into<String>) -> Result<()> {
        let mut settings = self
            .settings
            .lock()
            .map_err(|_| Error::internal("engine settings lock poisoned"))?;
        settings.model = model.into();
        Ok(())
    }

    pub fn set_mode(&self, mode: PermissionMode) -> Result<()> {
        let mut settings = self
            .settings
            .lock()
            .map_err(|_| Error::internal("engine settings lock poisoned"))?;
        settings.mode = mode;
        Ok(())
    }

    pub fn set_system_prompt(&self, prompt: Option<String>) -> Result<()> {
        let mut settings = self
            .settings
            .lock()
            .map_err(|_| Error::internal("engine settings lock poisoned"))?;
        settings.system_prompt = prompt;
        Ok(())
    }

    /// Start a turn and stream its events.
    pub fn start_turn(self: &Arc<Self>, session_id: &str, input: &str) -> TurnHandle {
        let (tx, rx) = mpsc::channel(128);
        let cancel: CancelFlag = Arc::new(AtomicBool::new(false));
        let engine = Arc::clone(self);
        let session_id = session_id.to_string();
        let input = input.to_string();
        let turn_cancel = Arc::clone(&cancel);
        tokio::spawn(async move {
            let mut usage = TokenUsage::default();
            let outcome = engine
                .run_turn(&session_id, &input, &tx, &turn_cancel, &mut usage)
                .await;
            let event = match outcome {
                Ok(()) => EngineEvent::Finished { session_id, usage },
                Err(err) => EngineEvent::Failed {
                    message: err.to_string(),
                    exit_code: err.exit_code(),
                },
            };
            let _ = tx.send(event).await;
        });
        TurnHandle { events: rx, cancel }
    }

    async fn run_turn(
        &self,
        session_id: &str,
        input: &str,
        tx: &mpsc::Sender<EngineEvent>,
        cancel: &CancelFlag,
        total_usage: &mut TokenUsage,
    ) -> Result<()> {
        let settings = self.settings();
        emit(
            tx,
            EngineEvent::TurnStarted {
                session_id: session_id.to_string(),
            },
        )
        .await?;

        let user_message = Message::user(self.redact_text(input));
        self.store.append_message(session_id, &user_message)?;
        self.store
            .set_title_if_empty(session_id, &crate::store::session_title_from(input))?;

        let tool_specs = self.tool_specs(&settings);
        for round in 0..settings.max_tool_rounds.max(1) {
            if cancel.load(Ordering::Relaxed) {
                return Err(Error::Cancelled);
            }
            let request = ChatRequest {
                model: settings.model.clone(),
                messages: self.history(session_id, &settings)?,
                tools: tool_specs.clone(),
                temperature: settings.temperature,
                max_tokens: settings.max_tokens,
                extra: settings.extra.clone(),
                cancel: Some(Arc::clone(cancel)),
            };

            // What the next request will actually carry is known before it is
            // sent; report it so the UI can show context growth as it happens.
            self.report_context(tx, &request.messages, &tool_specs, &settings)
                .await?;

            let mut stream = self.provider.chat_stream(request).await?;
            let mut accumulator = Accumulator::new();
            while let Some(event) = stream.recv().await {
                if cancel.load(Ordering::Relaxed) {
                    return Err(Error::Cancelled);
                }
                let event = event?;
                match &event {
                    ChatEvent::Delta(text) => {
                        emit(tx, EngineEvent::Delta { text: text.clone() }).await?;
                    }
                    ChatEvent::ReasoningDelta(text) => {
                        emit(tx, EngineEvent::ReasoningDelta { text: text.clone() }).await?;
                    }
                    _ => {}
                }
                accumulator.apply(&event);
            }

            let turn = accumulator.into_turn();
            if !turn.usage.is_empty() {
                self.store
                    .record_usage(session_id, &settings.model, &turn.usage)?;
                total_usage.add(&turn.usage);
                emit(tx, EngineEvent::Usage { usage: turn.usage }).await?;
                // Provider numbers are authoritative: replace the local estimate
                // with the real prompt size of the request that just ran.
                emit(
                    tx,
                    EngineEvent::Context {
                        usage: ContextUsage::reported(&turn.usage, settings.context_window),
                    },
                )
                .await?;
                self.warn_near_context_limit(tx, &settings, &turn.usage, total_usage)
                    .await?;
            }

            if turn.text.is_empty()
                && turn.reasoning.trim().is_empty()
                && turn.tool_calls.is_empty()
                && round == 0
            {
                return Err(Error::internal(format!(
                    "{} returned an empty response for model `{}`",
                    settings.provider, settings.model
                )));
            }

            let stored_text = self.redact_text(&turn.text);
            let stored_reasoning = if settings.store_reasoning {
                Some(self.redact_text(&turn.reasoning))
            } else {
                None
            };
            self.store.append_message(
                session_id,
                &Message::assistant_with_tools(stored_text, turn.tool_calls.clone())
                    .with_reasoning(stored_reasoning),
            )?;
            emit(
                tx,
                EngineEvent::AssistantMessage {
                    content: self.redact_text(&turn.text),
                    reasoning: if settings.store_reasoning {
                        Some(self.redact_text(&turn.reasoning))
                            .filter(|text| !text.trim().is_empty())
                    } else {
                        None
                    },
                    tool_calls: turn.tool_calls.clone(),
                },
            )
            .await?;

            if turn.tool_calls.is_empty() || tool_specs.is_empty() {
                return Ok(());
            }

            for call in &turn.tool_calls {
                self.run_tool_call(session_id, call, &settings, tx, cancel)
                    .await?;
            }
        }

        Ok(())
    }

    fn history(&self, session_id: &str, settings: &EngineSettings) -> Result<Vec<Message>> {
        let mut messages = Vec::new();
        if let Some(prompt) = settings.system_prompt.as_deref() {
            if !prompt.trim().is_empty() {
                messages.push(Message::system(prompt));
            }
        }
        messages.extend(self.store.messages(session_id)?);
        Ok(messages)
    }

    /// Report what the next request would carry.
    async fn report_context(
        &self,
        tx: &mpsc::Sender<EngineEvent>,
        messages: &[Message],
        specs: &[ToolSpec],
        settings: &EngineSettings,
    ) -> Result<()> {
        let estimate = tokens::estimate_context(messages, specs);
        emit(
            tx,
            EngineEvent::Context {
                usage: ContextUsage::estimate(estimate, settings.context_window),
            },
        )
        .await
    }

    /// Mention once per turn when the reported context is close to the window,
    /// so a run that is about to lose its older history says so out loud.
    async fn warn_near_context_limit(
        &self,
        tx: &mpsc::Sender<EngineEvent>,
        settings: &EngineSettings,
        usage: &TokenUsage,
        total_usage: &TokenUsage,
    ) -> Result<()> {
        let context = ContextUsage::reported(usage, settings.context_window);
        if !context.is_near_limit() {
            return Ok(());
        }
        let percent = context.percent().unwrap_or(0.0);
        // Only the first round that trips the threshold warns.
        if total_usage.prompt_tokens > usage.prompt_tokens {
            return Ok(());
        }
        emit(
            tx,
            EngineEvent::Notice {
                message: format!(
                    "context is {percent:.0}% full ({} of {} tokens); older messages may be dropped by the provider",
                    crate::usage::format_count(context.tokens),
                    crate::usage::format_count(context.window)
                ),
            },
        )
        .await
    }

    fn tool_specs(&self, settings: &EngineSettings) -> Vec<ToolSpec> {
        if !settings.tools_enabled {
            return Vec::new();
        }
        self.tools
            .as_ref()
            .map(|tools| tools.specs())
            .unwrap_or_default()
    }

    async fn run_tool_call(
        &self,
        session_id: &str,
        call: &ToolCall,
        settings: &EngineSettings,
        tx: &mpsc::Sender<EngineEvent>,
        cancel: &CancelFlag,
    ) -> Result<()> {
        let Some(tools) = self.tools.as_ref() else {
            return Ok(());
        };
        let request = tools.describe(call);
        let decision = self.policy.evaluate(&request);
        emit(
            tx,
            EngineEvent::ToolRequested {
                call: call.clone(),
                request: request.clone(),
                decision: decision.clone(),
            },
        )
        .await?;

        let denial_reason = match decision {
            Decision::Allow => None,
            Decision::Deny { reason } => Some(reason),
            Decision::Confirm { reason } => {
                emit(
                    tx,
                    EngineEvent::ApprovalRequested {
                        call: call.clone(),
                        request: request.clone(),
                        reason: reason.clone(),
                        mode: settings.mode,
                    },
                )
                .await?;
                match self
                    .approver
                    .approve(ApprovalRequest {
                        request: request.clone(),
                        reason,
                        mode: settings.mode,
                    })
                    .await
                {
                    Approval::Approved => None,
                    Approval::Denied => Some("the user denied this tool call".to_string()),
                }
            }
        };

        if let Some(reason) = denial_reason {
            let audit_id = self.record_tool_start(session_id, &request, settings, "deny")?;
            self.store
                .finish_tool_call(audit_id, status::DENIED, Some(&reason))?;
            let output = format!("DENIED: {reason}");
            self.store.append_message(
                session_id,
                &Message::tool(call.id.clone(), call.name.clone(), output.clone()),
            )?;
            emit(
                tx,
                EngineEvent::ToolFinished {
                    call: call.clone(),
                    ok: false,
                    output,
                },
            )
            .await?;
            return Ok(());
        }

        let audit_id =
            self.record_tool_start(session_id, &request, settings, decision_label(settings))?;
        let (ok, raw_output) = match tools.execute(call).await {
            Ok(outcome) => (outcome.ok, outcome.content),
            Err(err) => (false, err.to_string()),
        };
        if cancel.load(Ordering::Relaxed) {
            self.store
                .finish_tool_call(audit_id, status::INTERRUPTED, None)?;
            return Err(Error::Cancelled);
        }

        let redacted = self.redact_text(&raw_output);
        let (content, _truncated) = truncate_bytes(&redacted, settings.max_tool_output_bytes);
        let status_label = if ok {
            status::SUCCEEDED
        } else {
            status::FAILED
        };
        self.store.finish_tool_call(
            audit_id,
            status_label,
            settings.store_tool_results.then_some(content.as_str()),
        )?;

        let for_model = if ok {
            content.clone()
        } else {
            format!("ERROR: {content}")
        };
        self.store.append_message(
            session_id,
            &Message::tool(call.id.clone(), call.name.clone(), for_model),
        )?;
        emit(
            tx,
            EngineEvent::ToolFinished {
                call: call.clone(),
                ok,
                output: content,
            },
        )
        .await?;
        Ok(())
    }

    fn record_tool_start(
        &self,
        session_id: &str,
        request: &ToolRequest,
        settings: &EngineSettings,
        decision: &str,
    ) -> Result<i64> {
        self.store.record_tool_start(&ToolAuditRecord {
            session_id: session_id.to_string(),
            call: request.call.clone(),
            mode: settings.mode,
            risk: request.risk,
            summary: request.summary.clone(),
            decision: decision.to_string(),
        })
    }

    fn redact_text(&self, text: &str) -> String {
        if self
            .settings
            .lock()
            .map(|guard| guard.redact)
            .unwrap_or(true)
        {
            self.redactor.redact(text)
        } else {
            text.to_string()
        }
    }
}

fn decision_label(settings: &EngineSettings) -> &'static str {
    match settings.mode {
        PermissionMode::Safe => "confirm",
        PermissionMode::Yolo => "allow",
    }
}

async fn emit(tx: &mpsc::Sender<EngineEvent>, event: EngineEvent) -> Result<()> {
    tx.send(event).await.map_err(|_| Error::Cancelled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{ModelInfo, ToolSpec};
    use crate::policy::{AlwaysApprove, AlwaysDeny, Decision};
    use crate::provider::BoxFuture;
    use crate::store::MemoryStore;
    use crate::tool::ToolOutcome;
    use std::sync::Mutex as StdMutex;

    /// Scripted provider: each `chat_stream` call returns the next scripted
    /// stream of events.
    struct ScriptedProvider {
        scripts: StdMutex<Vec<Vec<Result<ChatEvent>>>>,
        requests: StdMutex<Vec<ChatRequest>>,
    }

    impl ScriptedProvider {
        fn new(scripts: Vec<Vec<Result<ChatEvent>>>) -> Arc<Self> {
            Arc::new(Self {
                scripts: StdMutex::new(scripts),
                requests: StdMutex::new(Vec::new()),
            })
        }

        fn requests(&self) -> Vec<ChatRequest> {
            self.requests.lock().unwrap().clone()
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
            request: ChatRequest,
        ) -> BoxFuture<'_, Result<mpsc::Receiver<Result<ChatEvent>>>> {
            self.requests.lock().unwrap().push(request);
            let script = {
                let mut scripts = self.scripts.lock().unwrap();
                if scripts.is_empty() {
                    Vec::new()
                } else {
                    scripts.remove(0)
                }
            };
            Box::pin(async move {
                let (tx, rx) = mpsc::channel(32);
                tokio::spawn(async move {
                    for event in script {
                        if tx.send(event).await.is_err() {
                            return;
                        }
                    }
                });
                Ok(rx)
            })
        }

        fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>>> {
            Box::pin(async {
                Ok(vec![ModelInfo {
                    id: "scripted-1".into(),
                    owned_by: None,
                }])
            })
        }
    }

    struct EchoTools;

    impl ToolExecutor for EchoTools {
        fn specs(&self) -> Vec<ToolSpec> {
            vec![ToolSpec {
                name: "shell_execute".to_string(),
                description: "run a command".to_string(),
                parameters: serde_json::json!({"type": "object"}),
            }]
        }

        fn describe(&self, call: &ToolCall) -> ToolRequest {
            ToolRequest::new(call.clone(), "echo", crate::message::RiskLevel::Execute)
                .with_cwd("/workspace")
        }

        fn execute(&self, call: &ToolCall) -> crate::provider::BoxFuture<'_, Result<ToolOutcome>> {
            let arguments = call.arguments.clone();
            Box::pin(async move { Ok(ToolOutcome::success(format!("ran {arguments}"))) })
        }
    }

    struct AllowAll;
    impl Policy for AllowAll {
        fn evaluate(&self, _request: &ToolRequest) -> Decision {
            Decision::Allow
        }
    }

    struct HardDeny;
    impl Policy for HardDeny {
        fn evaluate(&self, _request: &ToolRequest) -> Decision {
            Decision::deny("outside the workspace")
        }
    }

    struct ConfirmAll;
    impl Policy for ConfirmAll {
        fn evaluate(&self, _request: &ToolRequest) -> Decision {
            Decision::confirm("safe mode asks first")
        }
    }

    fn text_delta(text: &str) -> Vec<Result<ChatEvent>> {
        vec![
            Ok(ChatEvent::Delta(text.to_string())),
            Ok(ChatEvent::Usage(TokenUsage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
            })),
            Ok(ChatEvent::Finished),
        ]
    }

    fn tool_call_delta(id: &str, name: &str, arguments: &str) -> Result<ChatEvent> {
        Ok(ChatEvent::ToolCallDelta {
            index: 0,
            id: Some(id.to_string()),
            name: Some(name.to_string()),
            arguments_delta: Some(arguments.to_string()),
        })
    }

    fn engine(
        provider: Arc<dyn Provider>,
        tools: Option<Arc<dyn ToolExecutor>>,
        policy: Arc<dyn Policy>,
        approver: Arc<dyn Approver>,
    ) -> (Arc<Engine>, Arc<MemoryStore>) {
        let store = Arc::new(MemoryStore::new());
        let settings = EngineSettings {
            redact: false,
            ..EngineSettings::new("scripted", "scripted-1")
        };
        let engine = Arc::new(Engine::new(
            provider,
            store.clone(),
            tools,
            policy,
            approver,
            settings,
        ));
        (engine, store)
    }

    async fn drain(mut handle: TurnHandle) -> Vec<EngineEvent> {
        let mut events = Vec::new();
        while let Some(event) = handle.events.recv().await {
            events.push(event);
        }
        events
    }

    #[tokio::test]
    async fn one_turn_persists_messages_and_reports_usage() {
        let provider = ScriptedProvider::new(vec![text_delta("Hello there")]);
        let (engine, store) = engine(
            provider.clone(),
            None,
            Arc::new(AllowAll),
            Arc::new(AlwaysApprove),
        );
        let session = store
            .create_session("scripted", "scripted-1", None)
            .unwrap();

        let events = drain(engine.start_turn(&session.id, "hi")).await;

        assert!(
            events
                .iter()
                .any(|e| matches!(e, EngineEvent::TurnStarted { .. }))
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EngineEvent::Delta { text } if text == "Hello there"))
        );
        assert!(matches!(events.last(), Some(EngineEvent::Finished { .. })));

        let messages = store.messages(&session.id).unwrap();
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[1].content, "Hello there");
        assert_eq!(store.session_usage(&session.id).unwrap().total_tokens, 12);
        assert_eq!(
            store
                .get_session(&session.id)
                .unwrap()
                .unwrap()
                .title
                .as_deref(),
            Some("hi")
        );
        assert_eq!(provider.requests().len(), 1);
    }

    #[tokio::test]
    async fn tool_calls_run_then_feed_the_model() {
        let provider = ScriptedProvider::new(vec![
            vec![
                tool_call_delta("call_1", "shell_execute", "{\"command\":\"echo hi\"}"),
                Ok(ChatEvent::Finished),
            ],
            text_delta("done"),
        ]);
        let (engine, store) = engine(
            provider.clone(),
            Some(Arc::new(EchoTools)),
            Arc::new(AllowAll),
            Arc::new(AlwaysApprove),
        );
        let session = store
            .create_session("scripted", "scripted-1", None)
            .unwrap();

        let events = drain(engine.start_turn(&session.id, "run it")).await;
        assert!(
            events
                .iter()
                .any(|e| matches!(e, EngineEvent::ToolFinished { ok: true, .. }))
        );

        let audit = store.tool_audit(&session.id).unwrap();
        assert_eq!(audit.len(), 1);
        assert_eq!(audit[0].status, status::SUCCEEDED);
        assert_eq!(audit[0].tool_name, "shell_execute");
        assert_eq!(audit[0].decision.as_deref(), Some("confirm"));

        let messages = store.messages(&session.id).unwrap();
        let roles: Vec<_> = messages.iter().map(|m| m.role).collect();
        assert_eq!(
            roles,
            vec![
                crate::message::Role::User,
                crate::message::Role::Assistant,
                crate::message::Role::Tool,
                crate::message::Role::Assistant
            ]
        );
        assert_eq!(messages[2].tool_call_id.as_deref(), Some("call_1"));
        // The second request must carry the tool schema and the tool result.
        let requests = provider.requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[1].tools.len(), 1);
        assert_eq!(
            requests[1].messages.last().unwrap().role,
            crate::message::Role::Tool
        );
    }

    #[tokio::test]
    async fn policy_denials_are_recorded_and_never_executed() {
        let provider = ScriptedProvider::new(vec![
            vec![
                tool_call_delta("call_1", "shell_execute", "{}"),
                Ok(ChatEvent::Finished),
            ],
            text_delta("understood"),
        ]);
        let (engine, store) = engine(
            provider,
            Some(Arc::new(EchoTools)),
            Arc::new(HardDeny),
            Arc::new(AlwaysApprove),
        );
        let session = store
            .create_session("scripted", "scripted-1", None)
            .unwrap();

        let events = drain(engine.start_turn(&session.id, "run it")).await;
        assert!(events.iter().any(|event| matches!(
            event,
            EngineEvent::ToolFinished { ok: false, output, .. } if output.contains("DENIED")
        )));
        let audit = store.tool_audit(&session.id).unwrap();
        assert_eq!(audit[0].status, status::DENIED);
        assert!(
            audit[0]
                .result
                .as_deref()
                .unwrap()
                .contains("outside the workspace")
        );
    }

    #[tokio::test]
    async fn denied_approvals_do_not_execute_the_tool() {
        let provider = ScriptedProvider::new(vec![
            vec![
                tool_call_delta("call_1", "shell_execute", "{}"),
                Ok(ChatEvent::Finished),
            ],
            text_delta("ok, skipped"),
        ]);
        let (engine, store) = engine(
            provider,
            Some(Arc::new(EchoTools)),
            Arc::new(ConfirmAll),
            Arc::new(AlwaysDeny),
        );
        let session = store
            .create_session("scripted", "scripted-1", None)
            .unwrap();

        let events = drain(engine.start_turn(&session.id, "run it")).await;
        assert!(
            events
                .iter()
                .any(|event| matches!(event, EngineEvent::ApprovalRequested { .. }))
        );
        let messages = store.messages(&session.id).unwrap();
        assert!(messages[2].content.contains("DENIED"));
        assert!(!messages[2].content.contains("ran "), "tool must not run");
    }

    #[tokio::test]
    async fn empty_responses_are_reported_as_failures() {
        let provider = ScriptedProvider::new(vec![vec![Ok(ChatEvent::Finished)]]);
        let (engine, store) = engine(provider, None, Arc::new(AllowAll), Arc::new(AlwaysApprove));
        let session = store
            .create_session("scripted", "scripted-1", None)
            .unwrap();

        let events = drain(engine.start_turn(&session.id, "hi")).await;
        match events.last() {
            Some(EngineEvent::Failed { message, exit_code }) => {
                assert!(message.contains("empty response"), "{message}");
                assert_eq!(*exit_code, 1);
            }
            other => panic!("expected a failure event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn provider_errors_keep_their_exit_code() {
        let provider = ScriptedProvider::new(vec![vec![Err(Error::api(
            "scripted",
            402,
            "Insufficient Balance",
        ))]]);
        let (engine, store) = engine(provider, None, Arc::new(AllowAll), Arc::new(AlwaysApprove));
        let session = store
            .create_session("scripted", "scripted-1", None)
            .unwrap();

        let events = drain(engine.start_turn(&session.id, "hi")).await;
        match events.last() {
            Some(EngineEvent::Failed { message, exit_code }) => {
                assert!(message.contains("402"), "{message}");
                assert_eq!(*exit_code, 5);
            }
            other => panic!("expected a failure event, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tool_rounds_are_bounded() {
        let mut scripts = Vec::new();
        for _ in 0..10 {
            scripts.push(vec![
                tool_call_delta("call_1", "shell_execute", "{}"),
                Ok(ChatEvent::Finished),
            ]);
        }
        let provider = ScriptedProvider::new(scripts);
        let (engine, store) = engine(
            provider.clone(),
            Some(Arc::new(EchoTools)),
            Arc::new(AllowAll),
            Arc::new(AlwaysApprove),
        );
        let session = store
            .create_session("scripted", "scripted-1", None)
            .unwrap();
        drain(engine.start_turn(&session.id, "loop")).await;
        assert_eq!(provider.requests().len(), 8, "max_tool_rounds default");
    }

    #[tokio::test]
    async fn secrets_in_tool_output_are_redacted_before_storage() {
        struct LeakyTools;
        impl ToolExecutor for LeakyTools {
            fn specs(&self) -> Vec<ToolSpec> {
                vec![ToolSpec {
                    name: "shell_execute".into(),
                    description: "leak".into(),
                    parameters: serde_json::json!({}),
                }]
            }
            fn describe(&self, call: &ToolCall) -> ToolRequest {
                ToolRequest::new(call.clone(), "leak", crate::message::RiskLevel::Execute)
            }
            fn execute(
                &self,
                _call: &ToolCall,
            ) -> crate::provider::BoxFuture<'_, Result<ToolOutcome>> {
                Box::pin(async {
                    Ok(ToolOutcome::success(
                        "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz\n",
                    ))
                })
            }
        }

        let provider = ScriptedProvider::new(vec![
            vec![
                tool_call_delta("call_1", "shell_execute", "{}"),
                Ok(ChatEvent::Finished),
            ],
            text_delta("done"),
        ]);
        let store = Arc::new(MemoryStore::new());
        let engine = Arc::new(
            Engine::new(
                provider,
                store.clone(),
                Some(Arc::new(LeakyTools)),
                Arc::new(AllowAll),
                Arc::new(AlwaysApprove),
                EngineSettings {
                    redact: true,
                    ..EngineSettings::new("scripted", "scripted-1")
                },
            )
            .with_redactor(
                Redactor::none()
                    .with_secret_named("OPENAI_API_KEY", "sk-abcdefghijklmnopqrstuvwxyz"),
            ),
        );
        let session = store
            .create_session("scripted", "scripted-1", None)
            .unwrap();

        drain(engine.start_turn(&session.id, "leak it")).await;
        let audit = store.tool_audit(&session.id).unwrap();
        let stored = audit[0].result.clone().unwrap();
        assert!(
            !stored.contains("sk-abcdefghijklmnopqrstuvwxyz"),
            "{stored}"
        );
        assert!(stored.contains("REDACTED"), "{stored}");
    }

    #[tokio::test]
    async fn cancellation_stops_the_turn() {
        struct SlowProvider;
        impl Provider for SlowProvider {
            fn name(&self) -> &str {
                "slow"
            }
            fn base_url(&self) -> &str {
                "http://slow.test"
            }
            fn chat_stream(
                &self,
                _request: ChatRequest,
            ) -> BoxFuture<'_, Result<mpsc::Receiver<Result<ChatEvent>>>> {
                Box::pin(async {
                    let (tx, rx) = mpsc::channel(8);
                    tokio::spawn(async move {
                        loop {
                            if tx.send(Ok(ChatEvent::Delta("tick ".into()))).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                        }
                    });
                    Ok(rx)
                })
            }
            fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>>> {
                Box::pin(async { Ok(Vec::new()) })
            }
        }

        let (engine, store) = engine(
            Arc::new(SlowProvider),
            None,
            Arc::new(AllowAll),
            Arc::new(AlwaysApprove),
        );
        let session = store.create_session("slow", "slow-1", None).unwrap();
        let handle = engine.start_turn(&session.id, "go");
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        handle.cancel();
        let events = drain(handle).await;
        match events.last() {
            Some(EngineEvent::Failed { exit_code, .. }) => assert_eq!(*exit_code, 130),
            other => panic!("expected cancellation, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn tools_can_be_disabled_per_turn() {
        let provider = ScriptedProvider::new(vec![text_delta("no tools")]);
        let store = Arc::new(MemoryStore::new());
        let engine = Arc::new(Engine::new(
            provider.clone(),
            store.clone(),
            Some(Arc::new(EchoTools)),
            Arc::new(AllowAll),
            Arc::new(AlwaysApprove),
            EngineSettings {
                tools_enabled: false,
                redact: false,
                ..EngineSettings::new("scripted", "scripted-1")
            },
        ));
        let session = store
            .create_session("scripted", "scripted-1", None)
            .unwrap();
        drain(engine.start_turn(&session.id, "hi")).await;
        assert!(provider.requests()[0].tools.is_empty());
    }
}
