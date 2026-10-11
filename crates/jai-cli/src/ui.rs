//! Terminal presentation of engine events.
//!
//! Assistant text goes to stdout (so it can be piped); tool activity, usage and
//! errors go to stderr (so they never corrupt a pipeline).
//!
//! The rendering decisions themselves live in [`crate::transcript`], which the
//! TUI shares, so line mode and the TUI always show the same conversation.

use crate::transcript::{Diff, Kind, Transcript};
use jai_core::engine::{EngineEvent, TurnHandle};
use std::io::{IsTerminal, Write};
use std::time::Duration;

/// Result of a completed turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnOutcome {
    pub exit_code: i32,
    pub message: Option<String>,
    /// Final status line, when the run produced one.
    pub status: Option<String>,
}

/// Presentation options.
#[derive(Debug, Clone, Copy)]
pub struct TurnUi {
    pub verbose: bool,
    pub show_tool_output: bool,
    pub show_thinking: bool,
}

impl TurnUi {
    pub fn for_app(verbose: bool, show_thinking: bool) -> Self {
        Self {
            verbose,
            show_tool_output: verbose,
            show_thinking,
        }
    }

    /// Presentation without extra chatter (used by tests and non-tty callers).
    #[cfg(test)]
    pub fn quiet() -> Self {
        Self {
            verbose: false,
            show_tool_output: false,
            show_thinking: false,
        }
    }
}

/// Stream a turn to the terminal until it finishes.
pub async fn drive_turn(mut handle: TurnHandle, ui: TurnUi) -> TurnOutcome {
    let mut outcome = TurnOutcome::default();
    let mut transcript = Transcript::new(0, ui.show_thinking);
    while let Some(event) = handle.events.recv().await {
        render(&mut transcript, &event, ui);
        if let EngineEvent::Failed { message, exit_code } = &event {
            outcome.exit_code = *exit_code;
            outcome.message = Some(message.clone());
        }
    }
    let status = transcript.usage_line().render();
    outcome.status = (!status.is_empty()).then_some(status);
    outcome
}

/// Print one event the way line mode wants it.
fn render(transcript: &mut Transcript, event: &EngineEvent, ui: TurnUi) {
    let diff = transcript.apply(event);
    match (event, diff) {
        // Assistant text streams to stdout, unwrapped, byte for byte.
        (EngineEvent::Delta { .. }, Diff::Added { .. } | Diff::Append { .. }) => {
            let text = match event {
                EngineEvent::Delta { text } => text,
                _ => return,
            };
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            let _ = write!(lock, "{text}");
            let _ = lock.flush();
        }
        // Reasoning is a commentary channel: stderr, dimmed if the terminal
        // supports it, and never mixed into a redirected answer.
        (
            EngineEvent::ReasoningDelta { .. },
            Diff::Added { text, .. } | Diff::Append { text, .. },
        ) => {
            if ui.show_thinking {
                let stderr = std::io::stderr();
                let mut lock = stderr.lock();
                let _ = write!(lock, "{}", style_thinking(text));
                let _ = lock.flush();
            }
        }
        (EngineEvent::AssistantMessage { .. }, _) => {
            if transcript
                .entries()
                .iter()
                .any(|entry| entry.kind == Kind::Assistant && !entry.text.is_empty())
            {
                println!();
                if ui.show_thinking {
                    let _ = std::io::stderr().write_all(b"\n");
                }
            }
        }
        (EngineEvent::ToolRequested { .. }, Diff::Added { text, .. }) => {
            eprintln!("[tool] {text}");
        }
        (EngineEvent::ToolFinished { output, .. }, Diff::Added { text, .. }) => {
            eprintln!("[tool] {text}");
            if ui.show_tool_output {
                for line in output.lines().take(TOOL_OUTPUT_LINES) {
                    eprintln!("    {line}");
                }
                if output.lines().count() > TOOL_OUTPUT_LINES {
                    eprintln!("    …");
                }
            }
        }
        (EngineEvent::Notice { message }, Diff::Added { .. }) => {
            eprintln!("[note] {message}");
        }
        (EngineEvent::Failed { message, .. }, _) => {
            eprintln!("error: {message}");
        }
        _ => {
            if ui.verbose {
                if let EngineEvent::Usage { usage } = event {
                    eprintln!(
                        "[usage] prompt {} + completion {} = {} tokens",
                        usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                    );
                }
            }
        }
    }
}

/// Dim the reasoning text when stderr is a terminal; plain text otherwise.
fn style_thinking(text: impl AsRef<str>) -> String {
    let text = text.as_ref();
    if std::io::stderr().is_terminal() {
        format!("\x1b[2m{text}\x1b[0m")
    } else {
        text.to_string()
    }
}

/// One line summary of a turn, printed under the answer when verbose.
pub fn print_status(outcome: &TurnOutcome) {
    if let Some(status) = &outcome.status {
        eprintln!("{status}");
    }
}

/// How often the thinking indicator prints a dot.
pub const THINKING_DOT_INTERVAL: Duration = Duration::from_millis(300);

/// Tool result lines shown in verbose mode before eliding the rest.
const TOOL_OUTPUT_LINES: usize = 20;

#[cfg(test)]
mod tests {
    use super::*;
    use jai_core::message::TokenUsage;
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn failures_are_reported_with_their_exit_code() {
        let (tx, rx) = mpsc::channel(8);
        tx.send(EngineEvent::Failed {
            message: "deepseek returned HTTP 402".to_string(),
            exit_code: 5,
        })
        .await
        .unwrap();
        drop(tx);
        let handle = TurnHandle::from_receiver(rx);
        let outcome = drive_turn(handle, TurnUi::quiet()).await;
        assert_eq!(outcome.exit_code, 5);
        assert!(outcome.message.unwrap().contains("402"));
    }

    #[tokio::test]
    async fn a_finished_turn_exits_zero() {
        let (tx, rx) = mpsc::channel(8);
        tx.send(EngineEvent::Delta {
            text: "hello".to_string(),
        })
        .await
        .unwrap();
        tx.send(EngineEvent::AssistantMessage {
            content: "hello".to_string(),
            reasoning: None,
            tool_calls: Vec::new(),
        })
        .await
        .unwrap();
        tx.send(EngineEvent::Finished {
            session_id: "abc".to_string(),
            usage: TokenUsage::default(),
        })
        .await
        .unwrap();
        drop(tx);
        let outcome = drive_turn(TurnHandle::from_receiver(rx), TurnUi::quiet()).await;
        assert_eq!(outcome.exit_code, 0);
        assert!(outcome.message.is_none());
    }

    #[tokio::test]
    async fn the_run_reports_a_status_line() {
        let (tx, rx) = mpsc::channel(8);
        tx.send(EngineEvent::TurnStarted {
            session_id: "abc".to_string(),
        })
        .await
        .unwrap();
        tx.send(EngineEvent::Context {
            usage: jai_core::ContextUsage::new(345_000, 1_000_000, false),
        })
        .await
        .unwrap();
        tx.send(EngineEvent::Usage {
            usage: TokenUsage {
                prompt_tokens: 12_000,
                completion_tokens: 400,
                total_tokens: 12_400,
            },
        })
        .await
        .unwrap();
        tx.send(EngineEvent::Finished {
            session_id: "abc".to_string(),
            usage: TokenUsage::default(),
        })
        .await
        .unwrap();
        drop(tx);
        let outcome = drive_turn(TurnHandle::from_receiver(rx), TurnUi::quiet()).await;
        let status = outcome.status.expect("a status line");
        assert!(status.contains("Tokens: 12,400"), "{status}");
        assert!(status.contains("Cxt: 345,000 / 1,000,000 (34.5%)"), "{status}");
        assert!(status.contains("Time Used:"), "{status}");
    }
}
