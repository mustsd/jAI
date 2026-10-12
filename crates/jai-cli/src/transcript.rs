//! The conversation view, shared by the line REPL and the TUI.
//!
//! Both front ends need the same picture: the user's message, the model's
//! reasoning, the model's answer, tool activity, notices and errors — plus the
//! status line. Keeping that in one place means the two modes cannot drift, and
//! it is what the TUI paints every frame.

use jai_core::engine::EngineEvent;
use jai_core::message::TokenUsage;
use jai_core::usage::{ContextUsage, TaskUsage, UsageLine};
use std::time::{Duration, Instant};

/// What one entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    User,
    Assistant,
    /// The model's reasoning/thinking text.
    Reasoning,
    Tool,
    Notice,
    Error,
}

impl Kind {
    /// Label shown in the transcript gutter.
    pub fn label(self) -> &'static str {
        match self {
            Kind::User => "You",
            Kind::Assistant => "jAI",
            Kind::Reasoning => "thinking",
            Kind::Tool => "tool",
            Kind::Notice => "note",
            Kind::Error => "error",
        }
    }

    /// True for entries the model did not write as user-visible prose.
    pub fn is_meta(self) -> bool {
        matches!(self, Kind::Tool | Kind::Notice | Kind::Error)
    }
}

/// Rows the model's thinking may occupy.
///
/// A reasoning model can emit thousands of characters before the answer starts,
/// which buries the conversation it is supposed to explain — and the answer, once
/// it arrives, is what the user is waiting for. The thinking is therefore shown
/// as a live block of at most this many rows rather than as a transcript of its
/// own: every chunk replaces what was there, the first four rows are kept so the
/// thread can be followed and the last one is what the model is thinking now.
///
/// Five is enough to read a thought and small enough that the answer never has to
/// compete with it for the screen. `crate::view::thinking_lines` decides the rows.
pub const THINKING_LINES: usize = 5;

/// Characters of the model's reasoning kept in memory.
///
/// A bound, not a display decision: the rows that fit are chosen by width when
/// the block is drawn (`crate::view::thinking_lines`). This only has to be
/// generous enough that no realistic terminal runs out of text before it runs
/// out of rows, so a long think cannot grow the transcript without limit.
pub const REASONING_WINDOW: usize = 1000;

/// Marks a thinking line that starts in the middle of what the model said, so the
/// newest words are never mistaken for the beginning of a thought.
pub const ELLIPSIS: char = '…';

/// One rendered piece of the conversation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    pub text: String,
    /// False while the text is still streaming in.
    pub done: bool,
    /// True when the head of the text was dropped at [`REASONING_WINDOW`], so
    /// `text` only holds its newest part. Only ever set on [`Kind::Reasoning`]
    /// entries: the assistant's answer is the point of the turn and is never
    /// cut.
    pub elided: bool,
}

/// Increment the front end should apply after feeding an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Diff {
    /// Nothing visible changed (usage, turn start, ...).
    Silent,
    /// The entry at the end of the transcript grew; append `text`.
    Append { kind: Kind, text: String },
    /// A new entry was added; render it from scratch.
    Added { kind: Kind, text: String },
    /// The entry at the end of the transcript no longer reads the way it did:
    /// the front end must redraw that entry from [`Transcript::entries`] — the
    /// thinking block is rewritten rather than extended, so its text depends on
    /// the width available where it is drawn.
    Replace { kind: Kind },
}

/// The conversation as the user sees it, plus the live status line.
pub struct Transcript {
    entries: Vec<Entry>,
    /// Provider-reported totals for the current task, when available.
    task: TaskUsage,
    /// Local estimate of the tokens the task has moved, used before the first
    /// provider report arrives.
    estimated_task: u64,
    context: ContextUsage,
    show_thinking: bool,
    task_started: Option<Instant>,
    /// Time the last finished task took, kept so the status line survives the
    /// end of a turn.
    last_elapsed: Option<Duration>,
    notice_printed: bool,
}

impl Transcript {
    pub fn new(window: u64, show_thinking: bool) -> Self {
        Self {
            entries: Vec::new(),
            task: TaskUsage::default(),
            estimated_task: 0,
            context: ContextUsage::new(0, window, true),
            show_thinking,
            task_started: None,
            last_elapsed: None,
            notice_printed: false,
        }
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking
    }

    pub fn set_show_thinking(&mut self, show: bool) {
        self.show_thinking = show;
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    /// Record a line the user typed.
    pub fn push_user(&mut self, text: &str) -> Diff {
        self.add(Kind::User, text, true)
    }

    /// Record a front-end message (command output, hints).
    pub fn push_notice(&mut self, text: &str) -> Diff {
        self.add(Kind::Notice, text, true)
    }

    /// Record an error.
    pub fn push_error(&mut self, text: &str) -> Diff {
        self.add(Kind::Error, text, true)
    }

    /// True while a task is running (a turn was started and has not finished).
    pub fn is_running(&self) -> bool {
        self.task_started.is_some()
    }

    /// Milliseconds spent on the current task.
    pub fn elapsed(&self) -> Option<Duration> {
        self.task_started
            .map(|start| start.elapsed())
            .or(self.last_elapsed)
    }

    /// The three facts of the status line.
    pub fn usage_line(&self) -> UsageLine {
        let task = if self.task.is_empty() && self.estimated_task == 0 {
            None
        } else if self.task.is_empty() {
            Some(TaskUsage::estimated(self.estimated_task, 0))
        } else {
            Some(self.task)
        };
        // The context figure is shown as soon as the window is known, including
        // the `0 / 128,000` state before the first turn: a status line that
        // silently renders as an empty row looks like a broken layout.
        let context = (self.context.window > 0).then_some(self.context);
        UsageLine {
            task,
            context,
            elapsed: self.elapsed(),
        }
    }

    /// Feed one engine event.
    pub fn apply(&mut self, event: &EngineEvent) -> Diff {
        match event {
            EngineEvent::TurnStarted { .. } => {
                self.begin_task();
                Diff::Silent
            }
            EngineEvent::Delta { text } => self.stream(Kind::Assistant, text, true),
            EngineEvent::ReasoningDelta { text } => {
                if !self.show_thinking {
                    // Still counted as progress, just not displayed.
                    return Diff::Silent;
                }
                self.stream(Kind::Reasoning, text, true)
            }
            EngineEvent::AssistantMessage { content, .. } => {
                // The streamed entries already hold the text; this only closes
                // them so the next delta starts a new entry. `reasoning` is a
                // copy of what already arrived as `ReasoningDelta` events.
                self.close_last(Kind::Assistant);
                if content.trim().is_empty() {
                    if let Some(last) = self.entries.last() {
                        if last.kind == Kind::Assistant && last.text.trim().is_empty() {
                            self.entries.pop();
                        }
                    }
                }
                Diff::Silent
            }
            EngineEvent::ToolRequested {
                request, decision, ..
            } => {
                self.close_stream();
                let text = match decision {
                    jai_core::Decision::Deny { reason } => {
                        format!("{} denied — {reason}", request.summary)
                    }
                    jai_core::Decision::Allow => {
                        format!("{} [{}]", request.summary, request.risk)
                    }
                    // The approval prompt prints the details itself.
                    jai_core::Decision::Confirm { .. } => return Diff::Silent,
                };
                self.add(Kind::Tool, &text, true)
            }
            EngineEvent::ApprovalRequested { .. } => Diff::Silent,
            EngineEvent::ToolFinished { call, ok, output } => {
                let first_line = output.lines().next().unwrap_or("").trim();
                let text = if *ok {
                    format!("{} → ok{}", call.name, summarize(first_line))
                } else {
                    format!("{} → failed{}", call.name, summarize(first_line))
                };
                self.add(Kind::Tool, &text, true)
            }
            EngineEvent::Usage { usage } => {
                self.record_usage(usage);
                Diff::Silent
            }
            EngineEvent::Context { usage } => {
                self.context = *usage;
                // Until the provider reports real numbers, the task total is the
                // context that was just sent: it is the only honest figure.
                if self.task.is_empty() {
                    self.estimated_task = usage.tokens;
                }
                Diff::Silent
            }
            EngineEvent::Notice { message } => {
                if self.notice_printed {
                    return Diff::Silent;
                }
                self.notice_printed = true;
                self.add(Kind::Notice, message, true)
            }
            EngineEvent::Failed { message, .. } => {
                self.finish_task();
                self.add(Kind::Error, message, true)
            }
            EngineEvent::Finished { .. } => {
                self.finish_task();
                self.close_stream();
                Diff::Silent
            }
        }
    }

    /// Start a task: reset the per-task counters, keep the context estimate.
    fn begin_task(&mut self) {
        self.task = TaskUsage::default();
        self.estimated_task = self.context.tokens;
        self.task_started = Some(Instant::now());
        self.last_elapsed = None;
        self.notice_printed = false;
    }

    fn finish_task(&mut self) {
        if let Some(start) = self.task_started.take() {
            self.last_elapsed = Some(start.elapsed());
        }
    }

    fn record_usage(&mut self, usage: &TokenUsage) {
        if usage.is_empty() {
            return;
        }
        // Provider numbers replace the estimate; the running total keeps the
        // earlier reports of the same task (one per tool round trip).
        if self.task.is_empty() {
            self.task = TaskUsage::reported(usage);
        } else {
            self.task.add_reported(usage);
        }
        self.task.estimated = false;
    }

    fn stream(&mut self, kind: Kind, text: &str, visible: bool) -> Diff {
        if !visible {
            return Diff::Silent;
        }
        if let Some(last) = self.entries.last_mut() {
            if last.kind == kind && !last.done {
                if kind == Kind::Reasoning {
                    // Thinking rewrites itself: the block is the newest window of
                    // what the model said, not everything it has said.
                    roll(last, text);
                    return Diff::Replace { kind };
                }
                last.text.push_str(text);
                return Diff::Append {
                    kind,
                    text: text.to_string(),
                };
            }
        }
        let mut entry = Entry {
            kind,
            text: String::new(),
            done: false,
            elided: false,
        };
        if kind == Kind::Reasoning {
            roll(&mut entry, text);
            self.entries.push(entry);
            return Diff::Replace { kind };
        }
        entry.text.push_str(text);
        self.entries.push(entry);
        Diff::Added {
            kind,
            text: text.to_string(),
        }
    }

    fn close_last(&mut self, kind: Kind) {
        if let Some(last) = self.entries.last_mut() {
            if last.kind == kind {
                last.done = true;
            }
        }
    }

    /// Close every in-flight streaming entry.
    fn close_stream(&mut self) {
        for entry in self.entries.iter_mut() {
            if !entry.done {
                entry.done = true;
            }
        }
    }

    fn add(&mut self, kind: Kind, text: &str, done: bool) -> Diff {
        self.close_stream();
        let (text, elided) = window(kind, text);
        self.entries.push(Entry {
            kind,
            text: text.clone(),
            done,
            elided,
        });
        Diff::Added { kind, text }
    }

    /// True when nothing has been said yet in this task.
    ///
    /// Visible thinking counts as output: the live thinking line is already the
    /// heartbeat, and the dots would be printed over it.
    pub fn awaiting_first_output(&self) -> bool {
        self.is_running()
            && !self.entries().iter().any(|entry| {
                matches!(entry.kind, Kind::Assistant | Kind::Reasoning)
                    && !entry.text.trim().is_empty()
            })
    }

    /// Replace the whole transcript (used when a session is loaded).
    pub fn reset(&mut self) {
        self.entries.clear();
        self.task = TaskUsage::default();
        self.estimated_task = 0;
        self.task_started = None;
        self.last_elapsed = None;
        self.notice_printed = false;
    }

    /// Rebuild the transcript from a stored session.
    pub fn load_history(&mut self, messages: &[jai_core::Message]) {
        self.reset();
        for message in messages {
            match message.role {
                jai_core::Role::User => {
                    self.push_user(&message.content);
                }
                jai_core::Role::Assistant => {
                    if self.show_thinking {
                        if let Some(reasoning) = message.reasoning() {
                            self.add(Kind::Reasoning, reasoning, true);
                        }
                    }
                    if !message.content.trim().is_empty() {
                        self.add(Kind::Assistant, &message.content, true);
                    }
                    for call in &message.tool_calls {
                        self.add(Kind::Tool, &format!("{} requested", call.name), true);
                    }
                }
                jai_core::Role::Tool => {
                    self.add(
                        Kind::Tool,
                        &format!(
                            "{} → {}",
                            message.name.as_deref().unwrap_or("tool"),
                            message.content.lines().next().unwrap_or("").trim()
                        ),
                        true,
                    );
                }
                jai_core::Role::System => {}
            }
        }
    }
}

/// Add one streaming thinking chunk to `entry`, keeping only the newest
/// [`REASONING_WINDOW`] characters.
///
/// The entry's whole text is replaced rather than extended: what the user sees
/// is the tail of the model's thinking, so an older sentence does not linger and
/// push the newest words around.
fn roll(entry: &mut Entry, text: &str) {
    entry.text.push_str(text);
    let (kept, elided) = clip(&entry.text, REASONING_WINDOW);
    // Only a drop counts as elided: text that still fits must not be labelled
    // with an ellipsis it does not need.
    entry.elided |= elided;
    entry.text = kept;
}

/// Apply [`REASONING_WINDOW`] to a piece of text that arrived whole.
///
/// Used for the entries rebuilt from a stored session, which must end up showing
/// exactly what the live ones would have.
fn window(kind: Kind, text: &str) -> (String, bool) {
    if kind != Kind::Reasoning {
        return (text.to_string(), false);
    }
    clip(text, REASONING_WINDOW)
}

/// The newest `limit` characters of `text`, and whether the head was dropped.
///
/// Slices on `char_indices` rather than byte offsets, so thinking in a
/// multi-byte script is never cut in the middle of a character.
fn clip(text: &str, limit: usize) -> (String, bool) {
    let total = text.chars().count();
    if total <= limit {
        return (text.to_string(), false);
    }
    let cut = text
        .char_indices()
        .nth(total - limit)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    (text[cut..].to_string(), true)
}

fn summarize(line: &str) -> String {
    let line = line.trim();
    if line.is_empty() {
        String::new()
    } else {
        format!(" ({})", truncate(line, 60))
    }
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let shortened: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{shortened}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use jai_core::message::{PermissionMode, RiskLevel, ToolCall};
    use jai_core::policy::{Decision, ToolRequest};

    fn delta(text: &str) -> EngineEvent {
        EngineEvent::Delta {
            text: text.to_string(),
        }
    }

    fn reasoning(text: &str) -> EngineEvent {
        EngineEvent::ReasoningDelta {
            text: text.to_string(),
        }
    }

    fn transcript() -> Transcript {
        Transcript::new(128_000, true)
    }

    #[test]
    fn assistant_deltas_stream_into_one_entry() {
        let mut view = transcript();
        view.apply(&EngineEvent::TurnStarted {
            session_id: "s".into(),
        });
        assert_eq!(
            view.apply(&delta("Hel")),
            Diff::Added {
                kind: Kind::Assistant,
                text: "Hel".into()
            }
        );
        assert_eq!(
            view.apply(&delta("lo")),
            Diff::Append {
                kind: Kind::Assistant,
                text: "lo".into()
            }
        );
        assert_eq!(view.entries().len(), 1);
        assert_eq!(view.entries()[0].text, "Hello");
        assert!(!view.entries()[0].done);
    }

    #[test]
    fn reasoning_is_a_separate_entry_before_the_answer() {
        let mut view = transcript();
        view.apply(&reasoning("Let me "));
        view.apply(&reasoning("think."));
        let added = view.apply(&delta("Answer"));
        assert_eq!(
            added,
            Diff::Added {
                kind: Kind::Assistant,
                text: "Answer".into()
            }
        );
        let kinds: Vec<Kind> = view.entries().iter().map(|entry| entry.kind).collect();
        assert_eq!(kinds, vec![Kind::Reasoning, Kind::Assistant]);
        assert_eq!(view.entries()[0].text, "Let me think.");
    }

    #[test]
    fn hidden_thinking_is_not_displayed() {
        let mut view = Transcript::new(128_000, false);
        assert_eq!(view.apply(&reasoning("secret")), Diff::Silent);
        assert!(view.entries().is_empty());
        // ... but it does not appear once shown either, it was never stored.
        view.set_show_thinking(true);
        assert_eq!(
            view.apply(&reasoning("later")),
            Diff::Replace {
                kind: Kind::Reasoning
            }
        );
        assert_eq!(view.entries()[0].text, "later");
    }

    /// The block is redrawn, not extended: the front end is told to re-render the
    /// entry rather than handed a chunk to append.
    #[test]
    fn thinking_replaces_itself_instead_of_growing() {
        let mut view = transcript();
        assert_eq!(
            view.apply(&reasoning("Let me ")),
            Diff::Replace {
                kind: Kind::Reasoning
            }
        );
        assert_eq!(
            view.apply(&reasoning("think.")),
            Diff::Replace {
                kind: Kind::Reasoning
            }
        );
        assert_eq!(view.entries().len(), 1);
        assert_eq!(view.entries()[0].text, "Let me think.");
    }

    /// A long think must not be allowed to grow the transcript without limit.
    #[test]
    fn thinking_keeps_only_its_newest_window() {
        let mut view = transcript();
        view.apply(&reasoning(&"a".repeat(REASONING_WINDOW)));
        view.apply(&reasoning("TAIL"));
        let entry = &view.entries()[0];
        assert_eq!(entry.text.chars().count(), REASONING_WINDOW);
        assert!(entry.text.ends_with("TAIL"));
        assert!(entry.elided, "the dropped head has to be marked");
    }

    /// A think short enough to fit is never marked as if something was missing.
    #[test]
    fn short_thinking_is_not_marked_as_elided() {
        let mut view = transcript();
        view.apply(&reasoning("a thought"));
        assert!(!view.entries()[0].elided);
    }

    #[test]
    fn a_closed_entry_starts_a_new_one() {
        let mut view = transcript();
        view.apply(&delta("first"));
        view.apply(&EngineEvent::AssistantMessage {
            content: "first".into(),
            reasoning: None,
            tool_calls: Vec::new(),
        });
        view.apply(&delta("second"));
        assert_eq!(view.entries().len(), 2);
        assert!(view.entries()[0].done);
        assert_eq!(view.entries()[1].text, "second");
    }

    #[test]
    fn a_tool_call_only_turn_leaves_no_empty_answer() {
        let mut view = transcript();
        view.apply(&EngineEvent::AssistantMessage {
            content: String::new(),
            reasoning: None,
            tool_calls: vec![ToolCall::new("call_1", "shell_execute", "{}")],
        });
        view.apply(&EngineEvent::ToolRequested {
            call: ToolCall::new("call_1", "shell_execute", "{}"),
            request: ToolRequest::new(
                ToolCall::new("call_1", "shell_execute", "{}"),
                "run `ls`",
                RiskLevel::Execute,
            ),
            decision: Decision::Allow,
        });
        view.apply(&EngineEvent::ToolFinished {
            call: ToolCall::new("call_1", "shell_execute", "{}"),
            ok: true,
            output: "Cargo.toml\nsrc".into(),
        });
        assert_eq!(view.entries().len(), 2);
        assert_eq!(view.entries()[0].kind, Kind::Tool);
        assert!(view.entries()[1].text.contains("shell_execute → ok"));
        assert!(view.entries()[1].text.contains("Cargo.toml"));
    }

    #[test]
    fn denials_and_approvals_are_reported_once() {
        let mut view = transcript();
        let call = ToolCall::new("call_1", "filesystem_read", "{}");
        view.apply(&EngineEvent::ToolRequested {
            call: call.clone(),
            request: ToolRequest::new(call.clone(), "read `.env`", RiskLevel::ReadOnly),
            decision: Decision::deny("`/.env` is a protected file"),
        });
        assert!(view.entries()[0].text.contains("denied"));
        assert!(view.entries()[0].text.contains("protected"));

        // Confirmations are announced by the approver, not duplicated here.
        let mut view = transcript();
        view.apply(&EngineEvent::ToolRequested {
            call: call.clone(),
            request: ToolRequest::new(call.clone(), "read `a`", RiskLevel::ReadOnly),
            decision: Decision::confirm("safe mode asks first"),
        });
        assert!(view.entries().is_empty());
    }

    #[test]
    fn the_status_line_tracks_task_context_and_time() {
        let mut view = Transcript::new(128_000, true);
        view.apply(&EngineEvent::TurnStarted {
            session_id: "s".into(),
        });
        view.apply(&EngineEvent::Context {
            usage: ContextUsage::estimate(25_000, 128_000),
        });
        // Before the provider answers, the task total is an estimate.
        let line = view.usage_line().render();
        assert!(line.contains("Tokens: ~25,000"), "{line}");
        assert!(line.contains("Cxt: ~25,000 / 128,000 (19.5%)"), "{line}");
        assert!(line.contains("Time Used:"), "{line}");

        // Provider numbers replace it and are no longer marked as estimates.
        view.apply(&EngineEvent::Usage {
            usage: TokenUsage {
                prompt_tokens: 12_000,
                completion_tokens: 400,
                total_tokens: 12_400,
            },
        });
        view.apply(&EngineEvent::Context {
            usage: ContextUsage::reported(
                &TokenUsage {
                    prompt_tokens: 345_000,
                    completion_tokens: 0,
                    total_tokens: 345_000,
                },
                1_000_000,
            ),
        });
        let line = view.usage_line().render();
        assert!(line.contains("Tokens: 12,400"), "{line}");
        assert!(line.contains("Cxt: 345,000 / 1,000,000 (34.5%)"), "{line}");
        assert!(!line.contains('~'), "{line}");

        view.apply(&EngineEvent::Finished {
            session_id: "s".into(),
            usage: TokenUsage::default(),
        });
        assert!(!view.is_running());
        assert!(
            view.usage_line().elapsed.is_some(),
            "time is kept after the turn"
        );
    }

    #[test]
    fn several_reporting_rounds_accumulate_into_one_task_total() {
        let mut view = transcript();
        view.apply(&EngineEvent::TurnStarted {
            session_id: "s".into(),
        });
        for (prompt, completion) in [(10u64, 2u64), (30, 5)] {
            view.apply(&EngineEvent::Usage {
                usage: TokenUsage {
                    prompt_tokens: prompt,
                    completion_tokens: completion,
                    total_tokens: prompt + completion,
                },
            });
        }
        assert_eq!(view.usage_line().task.unwrap().total_tokens, 47);
    }

    #[test]
    fn notices_are_shown_once_per_task() {
        let mut view = transcript();
        view.apply(&EngineEvent::TurnStarted {
            session_id: "s".into(),
        });
        let first = view.apply(&EngineEvent::Notice {
            message: "context is 95% full".into(),
        });
        assert!(matches!(first, Diff::Added { .. }));
        assert_eq!(
            view.apply(&EngineEvent::Notice {
                message: "again".into()
            }),
            Diff::Silent
        );
        view.apply(&EngineEvent::TurnStarted {
            session_id: "s".into(),
        });
        assert!(matches!(
            view.apply(&EngineEvent::Notice {
                message: "new task".into()
            }),
            Diff::Added { .. }
        ));
    }

    #[test]
    fn stored_history_is_rebuilt_for_the_tui() {
        let mut view = transcript();
        let messages = vec![
            jai_core::Message::user("hi"),
            jai_core::Message::assistant_with_tools("answer", Vec::new())
                .with_reasoning(Some("because".to_string())),
            jai_core::Message::tool("call_1", "filesystem_read", "contents"),
        ];
        view.load_history(&messages);
        let kinds: Vec<Kind> = view.entries().iter().map(|entry| entry.kind).collect();
        assert_eq!(
            kinds,
            vec![Kind::User, Kind::Reasoning, Kind::Assistant, Kind::Tool]
        );
        assert!(view.entries()[3].text.contains("filesystem_read"));
    }

    #[test]
    fn permission_mode_is_not_part_of_the_view() {
        // Guard against accidentally rendering the policy mode as an entry.
        let mut view = transcript();
        view.apply(&EngineEvent::ApprovalRequested {
            call: ToolCall::new("call_1", "shell_execute", "{}"),
            request: ToolRequest::new(
                ToolCall::new("call_1", "shell_execute", "{}"),
                "run `ls`",
                RiskLevel::Execute,
            ),
            reason: "safe mode".into(),
            mode: PermissionMode::Safe,
        });
        assert!(view.entries().is_empty());
    }
}
