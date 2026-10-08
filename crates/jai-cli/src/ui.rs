//! Terminal presentation of engine events.
//!
//! Assistant text goes to stdout (so it can be piped); tool activity, usage and
//! errors go to stderr (so they never corrupt a pipeline).

use jai_core::engine::{EngineEvent, TurnHandle};
use jai_core::policy::Decision;
use std::io::Write;

/// Result of a completed turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TurnOutcome {
    pub exit_code: i32,
    pub message: Option<String>,
}

/// Presentation options.
#[derive(Debug, Clone, Copy)]
pub struct TurnUi {
    pub verbose: bool,
    pub show_tool_output: bool,
}

impl TurnUi {
    pub fn for_app(verbose: bool) -> Self {
        Self {
            verbose,
            show_tool_output: verbose,
        }
    }

    /// Presentation without extra chatter (used by tests and non-tty callers).
    #[cfg(test)]
    pub fn quiet() -> Self {
        Self {
            verbose: false,
            show_tool_output: false,
        }
    }
}

/// Stream a turn to the terminal until it finishes.
pub async fn drive_turn(mut handle: TurnHandle, ui: TurnUi) -> TurnOutcome {
    let mut outcome = TurnOutcome::default();
    while let Some(event) = handle.events.recv().await {
        match event {
            EngineEvent::TurnStarted { .. } => {}
            EngineEvent::Delta { text } => {
                let stdout = std::io::stdout();
                let mut lock = stdout.lock();
                let _ = write!(lock, "{text}");
                let _ = lock.flush();
            }
            EngineEvent::AssistantMessage { content, .. } => {
                if !content.trim().is_empty() {
                    println!();
                }
            }
            EngineEvent::ToolRequested {
                request, decision, ..
            } => match &decision {
                Decision::Deny { reason } => {
                    eprintln!("[tool] {} denied: {reason}", request.call.name);
                }
                Decision::Allow => {
                    eprintln!("[tool] {} — {}", request.summary, request.risk);
                }
                // The approver prints the confirmation prompt itself.
                Decision::Confirm { .. } => {}
            },
            EngineEvent::ApprovalRequested { .. } => {}
            EngineEvent::ToolFinished { call, ok, output } => {
                eprintln!(
                    "[tool] {} → {}",
                    call.name,
                    if ok { "ok" } else { "failed" }
                );
                if ui.show_tool_output {
                    for line in output.lines().take(20) {
                        eprintln!("    {line}");
                    }
                    if output.lines().count() > 20 {
                        eprintln!("    …");
                    }
                }
            }
            EngineEvent::Usage { usage } => {
                if ui.verbose {
                    eprintln!(
                        "[usage] prompt {} + completion {} = {} tokens",
                        usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                    );
                }
            }
            EngineEvent::Failed { message, exit_code } => {
                outcome.exit_code = exit_code;
                outcome.message = Some(message);
            }
            EngineEvent::Finished { usage, .. } => {
                if ui.verbose && !usage.is_empty() {
                    eprintln!("[done] {} tokens total", usage.total_tokens);
                }
            }
        }
    }
    outcome
}

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
        assert_eq!(outcome, TurnOutcome::default());
    }
}
