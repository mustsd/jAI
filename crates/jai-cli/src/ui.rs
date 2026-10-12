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
    let mut thinking = ThinkingLine::new();
    while let Some(event) = handle.events.recv().await {
        render(&mut transcript, &mut thinking, &event, ui);
        if let EngineEvent::Failed { message, exit_code } = &event {
            outcome.exit_code = *exit_code;
            outcome.message = Some(message.clone());
        }
    }
    let status = transcript.usage_line().render();
    outcome.status = (!status.is_empty()).then_some(status);
    outcome
}

/// The live thinking block, rewritten in place.
///
/// A reasoning model can emit thousands of characters before the answer starts.
/// Letting them scroll buries the answer the user is waiting for, so the newest
/// [`THINKING_LINES`] rows are drawn on screen and replaced as more arrives — the
/// same block the TUI paints, repainted with `\r` and cursor-up instead of by
/// ratatui.
///
/// When stderr is not a terminal there is nothing to rewrite: the text is then
/// held back and printed once, when the thinking ends, so a pipe still gets the
/// model's reasoning without a line per chunk.
pub struct ThinkingLine {
    interactive: bool,
    /// Rows currently on screen, erased before the next block is drawn.
    printed: usize,
    /// Newest text, held back when the output cannot be rewritten.
    held: Option<String>,
    elided: bool,
}

impl ThinkingLine {
    pub fn new() -> Self {
        Self {
            interactive: std::io::stderr().is_terminal(),
            printed: 0,
            held: None,
            elided: false,
        }
    }

    /// Set the live thinking text — the newest window, not a chunk.
    pub fn update(&mut self, text: &str, elided: bool) {
        self.elided = elided;
        if !self.interactive {
            self.held = Some(text.to_string());
            return;
        }
        let width = crossterm::terminal::size()
            .map(|(columns, _)| columns as usize)
            .unwrap_or(FALLBACK_WIDTH);
        let rows = crate::view::thinking_lines(text, width.max(3).saturating_sub(2), elided);
        let mut stderr = std::io::stderr().lock();
        // Walk back up to the first row of the previous block, erasing as we go:
        // the replacement is rarely the same shape as what it replaces.
        for _ in 1..self.printed {
            let _ = stderr.write_all(b"\x1b[K\x1b[1A");
        }
        let _ = stderr.write_all(b"\x1b[K\r");
        for (index, row) in rows.iter().enumerate() {
            if index > 0 {
                let _ = stderr.write_all(b"\n");
            }
            let _ = stderr.write_all(style_thinking(row).as_bytes());
        }
        let _ = stderr.flush();
        // At least one row: the block leaves a row behind even before the first
        // word arrives, so the next frame knows what to erase.
        self.printed = rows.len().max(1);
    }

    /// End the thinking, leaving its last block on screen.
    pub fn finish(&mut self) {
        self.held = None;
        if !self.interactive {
            return;
        }
        if self.printed > 0 {
            let _ = std::io::stderr().write_all(b"\n");
            self.printed = 0;
        }
    }
}

impl Default for ThinkingLine {
    fn default() -> Self {
        Self::new()
    }
}

/// Assumed width when the terminal will not say how wide it is.
const FALLBACK_WIDTH: usize = 100;

/// Print one event the way line mode wants it.
fn render(
    transcript: &mut Transcript,
    thinking: &mut ThinkingLine,
    event: &EngineEvent,
    ui: TurnUi,
) {
    let diff = transcript.apply(event);
    if !matches!(event, EngineEvent::ReasoningDelta { .. }) {
        // Anything that is not thinking ends the live line.
        thinking.finish();
    }
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
        (EngineEvent::ReasoningDelta { .. }, Diff::Added { .. } | Diff::Replace { .. }) => {
            if ui.show_thinking {
                // `Replace` means the block was rewritten: draw the whole newest
                // window, which is what the transcript now holds.
                let text = transcript
                    .entries()
                    .last()
                    .filter(|entry| entry.kind == Kind::Reasoning);
                if let Some(entry) = text {
                    thinking.update(&entry.text, entry.elided);
                }
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
        assert!(
            status.contains("Cxt: 345,000 / 1,000,000 (34.5%)"),
            "{status}"
        );
        assert!(status.contains("Time Used:"), "{status}");
    }
}
