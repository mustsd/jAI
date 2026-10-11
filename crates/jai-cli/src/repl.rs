//! Line-mode interactive agent (`jai -a --no-tui`, or when stdout is a pipe).
//!
//! Same session controller as the TUI, plain `rustyline` editing and a scrolling
//! transcript. Ctrl-C cancels the running task (and clears the line at the
//! prompt); Ctrl-D exits.

use crate::session::{CommandOutcome, SessionController};
use crate::transcript::{Diff, Kind, Transcript};
use crate::ui::THINKING_DOT_INTERVAL;
use jai_core::engine::EngineEvent;
use jai_core::error::{Error, Result};
use jai_core::usage::UsageLine;
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::io::{IsTerminal, Write};
use tokio::sync::mpsc;

/// Run the interactive agent.
pub async fn run(controller: SessionController) -> Result<i32> {
    let mut repl = Repl::new(controller);
    repl.run().await
}

struct Repl {
    controller: SessionController,
    transcript: Transcript,
    /// Events of the running turn, drained while waiting for input.
    turn: Option<mpsc::Receiver<EngineEvent>>,
    cancel: Option<jai_core::TurnCanceller>,
}

impl Repl {
    fn new(controller: SessionController) -> Self {
        let window = controller.engine_settings().context_window;
        let show_thinking = controller.show_thinking();
        Self {
            controller,
            transcript: Transcript::new(window, show_thinking),
            turn: None,
            cancel: None,
        }
    }

    async fn run(&mut self) -> Result<i32> {
        for line in self.controller.banner() {
            println!("{line}");
        }
        for line in self.controller.interrupted() {
            eprintln!("{line}");
        }
        let session_id = self.controller.session().id.clone();
        let messages = self.controller.app().store.messages(&session_id)?;
        if !messages.is_empty() {
            self.transcript.load_history(&messages);
        }
        println!("type /help for commands, Ctrl-D to exit\n");

        let mut editor = DefaultEditor::new()
            .map_err(|err| Error::internal(format!("could not start the line editor: {err}")))?;
        let history_path = self.controller.app().paths.root().join("history.txt");
        let _ = editor.load_history(&history_path);

        loop {
            // Drain anything a task still has to say before prompting again.
            self.drain_turn().await;
            self.print_status();
            match editor.readline("You> ") {
                Ok(line) => {
                    let text = line.trim().to_string();
                    if text.is_empty() {
                        continue;
                    }
                    let _ = editor.add_history_entry(text.as_str());
                    if crate::session::is_command(&text) {
                        match self.handle_command(&text) {
                            Ok(true) => break,
                            Ok(false) => {}
                            Err(err) => eprintln!("error: {err}"),
                        }
                        continue;
                    }
                    self.submit(&text).await;
                }
                // Ctrl-C at the prompt clears the line instead of leaving jAI,
                // unless a task is running: then it cancels the task.
                Err(ReadlineError::Interrupted) => {
                    if self.turn.is_some() {
                        self.cancel_turn();
                        self.drain_turn().await;
                    } else {
                        eprintln!("(press Ctrl-D to exit)");
                    }
                }
                Err(ReadlineError::Eof) => break,
                Err(err) => {
                    eprintln!("input error: {err}");
                    break;
                }
            }
        }

        let _ = editor.save_history(&history_path);
        self.controller.warn_if_state_dir_is_readable();
        println!("{}", self.controller.farewell());
        Ok(0)
    }

    /// Run a slash command; returns whether the REPL should exit.
    fn handle_command(&mut self, text: &str) -> Result<bool> {
        self.transcript.push_user(text);
        match self.controller.handle_command(text)? {
            CommandOutcome::Exit => Ok(true),
            CommandOutcome::Lines(lines) => {
                for line in lines {
                    println!("{line}");
                }
                self.reload_if_needed(text);
                Ok(false)
            }
            CommandOutcome::Confirm { question, action } => {
                print!("{question} [y/N] ");
                let _ = std::io::stdout().flush();
                let mut answer = String::new();
                let approved = std::io::stdin().read_line(&mut answer).is_ok()
                    && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes");
                for line in self.controller.confirm(action, approved)? {
                    println!("{line}");
                }
                Ok(false)
            }
        }
    }

    fn reload_if_needed(&mut self, command: &str) {
        if command.starts_with("/clear") {
            self.transcript.reset();
            return;
        }
        if !(command.starts_with("/new")
            || command.starts_with("/load")
            || command.starts_with("/delete"))
        {
            return;
        }
        let show_thinking = self.transcript.show_thinking();
        let window = self.controller.engine_settings().context_window;
        let session_id = self.controller.session().id.clone();
        let mut fresh = Transcript::new(window, show_thinking);
        if let Ok(messages) = self.controller.app().store.messages(&session_id) {
            fresh.load_history(&messages);
        }
        self.transcript = fresh;
    }

    async fn submit(&mut self, text: &str) {
        let handle = self.controller.start_turn(text);
        let canceller = handle.canceller();
        let cancel = handle.canceller();
        let (tx, rx) = mpsc::channel(128);
        let mut events = handle.events;
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if tx.send(event).await.is_err() {
                    canceller.cancel();
                    return;
                }
            }
        });
        self.turn = Some(rx);
        self.cancel = Some(cancel);
        self.drain_turn().await;
    }

    fn cancel_turn(&mut self) {
        if let Some(cancel) = &self.cancel {
            cancel.cancel();
            eprintln!("cancelling…");
        }
    }

    /// Render everything the running task has produced so far.
    ///
    /// While the model has not produced anything yet a `.` is printed every
    /// [`THINKING_DOT_INTERVAL`], so a slow first token still looks alive; the
    /// dots are erased as a group before the first real output. Ctrl-C during
    /// the wait cancels the task instead of killing jAI.
    async fn drain_turn(&mut self) {
        // The receiver is taken out of `self` so rendering the events can borrow
        // the rest of the state.
        let Some(mut receiver) = self.turn.take() else {
            return;
        };
        let show_thinking = self.transcript.show_thinking();
        let mut dots = ThinkingDots::stderr();
        let mut ticker = tokio::time::interval(THINKING_DOT_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut interrupt = Box::pin(tokio::signal::ctrl_c());
        // Dots are heartbeat feedback while the model has not said anything yet;
        // once output is on screen they would only get in the way.
        let mut waiting = self.transcript.awaiting_first_output();

        loop {
            tokio::select! {
                next = receiver.recv() => {
                    let Some(event) = next else { break };
                    dots.clear();
                    self.render(&event, show_thinking);
                    waiting = self.transcript.awaiting_first_output();
                }
                _ = ticker.tick(), if waiting => dots.wait(),
                _ = &mut interrupt => {
                    dots.clear();
                    self.cancel_turn();
                    interrupt = Box::pin(tokio::signal::ctrl_c());
                }
            }
        }

        dots.clear();
        self.cancel = None;
    }

    fn render(&mut self, event: &EngineEvent, show_thinking: bool) {
        let diff = self.transcript.apply(event);
        match event {
            EngineEvent::Delta { text } => {
                if matches!(diff, Diff::Added { .. } | Diff::Append { .. }) {
                    print!("{text}");
                    let _ = std::io::stdout().flush();
                }
            }
            EngineEvent::ReasoningDelta { text } => {
                if !show_thinking {
                    return;
                }
                if matches!(diff, Diff::Added { .. } | Diff::Append { .. }) {
                    let styled = if std::io::stderr().is_terminal() {
                        format!("\x1b[2m{text}\x1b[0m")
                    } else {
                        text.clone()
                    };
                    eprint!("{styled}");
                    let _ = std::io::stderr().flush();
                }
            }
            EngineEvent::AssistantMessage { content, .. } => {
                if !content.trim().is_empty() {
                    println!();
                    if show_thinking {
                        eprintln!();
                    }
                }
            }
            EngineEvent::ToolRequested { decision, .. } => {
                let _ = decision;
                if let Diff::Added { text, .. } = &diff {
                    eprintln!("[tool] {text}");
                }
            }
            EngineEvent::ToolFinished { .. } => {
                if let Diff::Added { text, .. } = &diff {
                    eprintln!("[tool] {text}");
                }
            }
            EngineEvent::Notice { message } => {
                if matches!(diff, Diff::Added { .. }) {
                    eprintln!("[note] {message}");
                }
            }
            EngineEvent::Failed { message, .. } => {
                eprintln!("error: {message}");
            }
            _ => {}
        }
    }

    /// Print the status line after a task finishes.
    fn print_status(&self) {
        let UsageLine {
            task,
            context,
            elapsed,
        } = self.transcript.usage_line();
        if task.is_none() && context.is_none() && elapsed.is_none() {
            return;
        }
        let line = self.transcript.usage_line().render();
        if !line.is_empty() {
            println!();
            eprintln!("{line}");
        }
    }

    #[allow(dead_code)]
    fn thinking_kinds(&self) -> usize {
        self.transcript
            .entries()
            .iter()
            .filter(|entry| entry.kind == Kind::Reasoning)
            .count()
    }
}

/// The "the model is thinking" heartbeat: one `.` per interval, erased as a
/// group before the first output line.
struct ThinkingDots {
    enabled: bool,
    printed: usize,
}

impl ThinkingDots {
    fn stderr() -> Self {
        Self {
            enabled: std::io::stderr().is_terminal(),
            printed: 0,
        }
    }

    fn wait(&mut self) {
        if !self.enabled {
            return;
        }
        let mut stderr = std::io::stderr();
        if stderr.write_all(b".").is_ok() && stderr.flush().is_ok() {
            self.printed += 1;
        }
    }

    fn clear(&mut self) {
        if self.printed == 0 {
            return;
        }
        let mut stderr = std::io::stderr();
        let _ = write!(stderr, "\r{:width$}\r", "", width = self.printed);
        let _ = stderr.flush();
        self.printed = 0;
    }
}
