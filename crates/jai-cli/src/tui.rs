//! Full-screen interactive agent (`jai -a` on a terminal).
//!
//! This module is only glue: keystrokes in, engine events in, [`View`] out.
//! Everything that decides *what* happens (policy, confirmation, tool calls,
//! session switching) lives in [`crate::session::SessionController`], and all
//! layout and wrapping lives in [`crate::view`] — so the TUI, the line REPL and
//! the tests all agree about the conversation.
//!
//! Key bindings:
//!
//! | key | action |
//! |-----|--------|
//! | `Enter` | send the message, or run the slash command |
//! | `Esc`, `Ctrl-C` | cancel the running task; at an idle prompt, clear the line |
//! | `Ctrl-D` | quit |
//! | `Up`/`Down`, `PageUp`/`PageDown` | scroll the transcript |
//! | `End` | jump back to the newest output |
//! | `Ctrl-R` | show or hide the model's reasoning text |
//! | `y` / `n` | answer a confirmation |

use crate::session::{CommandOutcome, ConfirmAction, SessionController};
use crate::transcript::Transcript;
use crate::view::{Prompt, View};
use jai_core::engine::EngineEvent;
use jai_core::error::{Error, Result};
use jai_core::policy::{Approval, ApprovalRequest, Approver};
use jai_core::provider::BoxFuture;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use ratatui::crossterm::execute;
use ratatui::crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use std::io::IsTerminal;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// How often the screen repaints, so the elapsed time ticks while a task runs.
const FRAME_INTERVAL: Duration = Duration::from_millis(100);

/// Whether this run should use the full-screen UI.
///
/// `--no-tui` or a redirected stdout gets line mode: the TUI would either fail
/// or make the answer unreadable.
pub fn wanted(cli: &crate::args::Cli) -> bool {
    !cli.no_tui && std::io::stdout().is_terminal()
}

/// Run the TUI until the user quits.
pub async fn run(mut controller: SessionController) -> Result<i32> {
    if !std::io::stdout().is_terminal() {
        return Err(Error::config(
            "the TUI needs a terminal on stdout; use --no-tui for line mode",
        ));
    }
    // Confirmations are asked in the footer, so the engine must talk to us
    // instead of reading stdin (which raw mode owns).
    let (request_tx, request_rx) = mpsc::unbounded_channel::<UiApprovalRequest>();
    controller = controller.with_approver(Arc::new(ChannelApprover {
        requests: request_tx,
    }))?;

    let title = format!("{} / {}", controller.provider_name(), controller.model());
    let mode = controller.mode().to_string();
    let window = controller.engine_settings().context_window;
    let show_thinking = controller.show_thinking();
    let mut app = App {
        controller,
        view: View::new(Transcript::new(window, show_thinking), title, mode),
        turn: None,
        turn_events: None,
        running: false,
        quit: false,
        requests: request_rx,
        pending_reply: None,
        pending_command: None,
    };
    app.bootstrap();

    let mut terminal =
        Terminal::new(CrosstermBackend::new(std::io::stdout())).map_err(terminal_error)?;
    enable_raw_mode().map_err(terminal_error)?;
    if let Err(err) = execute!(
        terminal.backend_mut(),
        EnterAlternateScreen,
        EnableBracketedPaste
    ) {
        let _ = disable_raw_mode();
        return Err(terminal_error(err));
    }

    let outcome = app.event_loop(&mut terminal).await;

    let _ = execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        LeaveAlternateScreen
    );
    let _ = disable_raw_mode();
    app.controller.warn_if_state_dir_is_readable();
    outcome
}

/// A confirmation the TUI has to ask, plus the channel the answer goes back on.
struct UiApprovalRequest {
    request: ApprovalRequest,
    reply: oneshot::Sender<Approval>,
}

/// Asks the user through the TUI footer.
///
/// The request is handed to the event loop and this future waits for the answer,
/// so the engine blocks on the user exactly like the terminal approver does. The
/// difference is only *where* the question is drawn: raw mode means stdin cannot
/// be read.
struct ChannelApprover {
    requests: mpsc::UnboundedSender<UiApprovalRequest>,
}

impl Approver for ChannelApprover {
    fn approve(&self, request: ApprovalRequest) -> BoxFuture<'_, Approval> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            if self
                .requests
                .send(UiApprovalRequest { request, reply })
                .is_err()
            {
                // The UI is gone: refusing is the safe default.
                return Approval::Denied;
            }
            answer.await.unwrap_or(Approval::Denied)
        })
    }
}

struct App {
    controller: SessionController,
    view: View,
    /// Canceller for the running turn, used by Esc/Ctrl-C.
    turn: Option<jai_core::TurnCanceller>,
    turn_events: Option<mpsc::Receiver<EngineEvent>>,
    running: bool,
    quit: bool,
    requests: mpsc::UnboundedReceiver<UiApprovalRequest>,
    /// Reply channel of the confirmation on screen, when a tool is asking.
    pending_reply: Option<oneshot::Sender<Approval>>,
    /// Action of the confirmation on screen, when a slash command is asking.
    pending_command: Option<ConfirmAction>,
}

impl App {
    /// Fill the transcript with the banner, the resume note and stored history.
    fn bootstrap(&mut self) {
        for line in self.controller.banner() {
            self.view.transcript.push_notice(&line);
        }
        for line in self.controller.interrupted() {
            self.view.transcript.push_notice(&line);
        }
        self.load_history();
    }

    fn load_history(&mut self) {
        let session_id = self.controller.session().id.clone();
        // The status line shows the context the next request would carry; seed it
        // now so the `Cxt:` figure is on screen before the first turn runs (a
        // resumed session starts with the whole stored conversation counted).
        if let Ok(usage) = self.controller.context_estimate() {
            self.view.transcript.apply(&EngineEvent::Context { usage });
        }
        let Ok(messages) = self.controller.app().store.messages(&session_id) else {
            return;
        };
        self.view.transcript.load_history(&messages);
        self.view.scroll_to_bottom();
    }

    async fn event_loop(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    ) -> Result<i32> {
        // Terminal input arrives on a blocking thread; the engine's events arrive
        // on a channel per turn. The loop repaints on a timer so the elapsed time
        // ticks even when nothing else happens.
        let (key_tx, mut key_rx) = mpsc::channel::<Event>(64);
        std::thread::spawn(move || {
            while let Ok(event) = event::read() {
                if key_tx.blocking_send(event).is_err() {
                    return;
                }
            }
        });

        let mut ticker = tokio::time::interval(FRAME_INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        while !self.quit {
            terminal
                .draw(|frame| self.view.draw(frame))
                .map_err(terminal_error)?;
            let events = self.turn_events.as_mut();
            tokio::select! {
                Some(event) = key_rx.recv() => self.on_terminal_event(event),
                Some(request) = self.requests.recv() => self.ask(request),
                Some(event) = next_turn_event(events) => self.on_engine_event(event),
                _ = ticker.tick() => {}
            }
        }
        Ok(0)
    }

    fn on_terminal_event(&mut self, event: Event) {
        match event {
            Event::Paste(text) => {
                for ch in text.chars() {
                    if ch == '\n' || ch == '\r' {
                        self.submit();
                    } else {
                        self.view.insert_char(ch);
                    }
                }
            }
            Event::Key(key) => self.on_key(key),
            _ => {}
        }
    }

    fn on_key(&mut self, key: KeyEvent) {
        if key.kind == KeyEventKind::Release {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // A confirmation owns the keyboard while it is on screen: the engine is
        // blocked waiting for the answer.
        if self.view.prompt.is_some() {
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') => self.answer(true),
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => self.answer(false),
                _ => {}
            }
            return;
        }

        if self.running
            && (matches!(key.code, KeyCode::Esc) || (ctrl && key.code == KeyCode::Char('c')))
        {
            self.cancel_turn();
            return;
        }
        if ctrl && key.code == KeyCode::Char('d') {
            self.quit = true;
            return;
        }
        if ctrl && key.code == KeyCode::Char('r') {
            let show = !self.view.transcript.show_thinking();
            self.view.transcript.set_show_thinking(show);
            self.controller.set_show_thinking(show);
            self.view.set_message(if show {
                "reasoning: shown"
            } else {
                "reasoning: hidden"
            });
            return;
        }

        match key.code {
            KeyCode::Esc => {
                if !self.view.input.is_empty() {
                    self.view.input.clear();
                    self.view.cursor = 0;
                } else {
                    self.view.set_message("Ctrl-D quits");
                }
            }
            KeyCode::Enter => self.submit(),
            KeyCode::Up => self.view.scroll_up(1),
            KeyCode::Down => self.view.scroll_down(1),
            KeyCode::PageUp => self.view.scroll_up(10),
            KeyCode::PageDown => self.view.scroll_down(10),
            KeyCode::End => self.view.scroll_to_bottom(),
            KeyCode::Home => self.view.cursor = 0,
            KeyCode::Left => self.view.cursor = self.view.cursor.saturating_sub(1),
            KeyCode::Right => {
                self.view.cursor = (self.view.cursor + 1).min(self.view.input.chars().count())
            }
            KeyCode::Backspace => self.view.delete_before_cursor(),
            KeyCode::Delete => self.view.delete_at_cursor(),
            KeyCode::Char(ch) if !ctrl => self.view.insert_char(ch),
            _ => {}
        }
    }

    fn on_engine_event(&mut self, event: EngineEvent) {
        let finished = matches!(
            event,
            EngineEvent::Finished { .. } | EngineEvent::Failed { .. }
        );
        self.view.transcript.apply(&event);
        if finished {
            self.running = false;
            self.turn = None;
            self.turn_events = None;
        }
    }

    /// Show a tool confirmation in the footer.
    fn ask(&mut self, pending: UiApprovalRequest) {
        let UiApprovalRequest { request, reply } = pending;
        let tool = &request.request;
        self.view.prompt = Some(Prompt {
            question: format!("run {} ({})?", tool.call.name, tool.risk),
            details: vec![
                format!("summary:   {}", tool.summary),
                format!("arguments: {}", shorten(&tool.call.arguments, 200)),
                format!(
                    "directory: {}",
                    if tool.cwd.is_empty() { "." } else { &tool.cwd }
                ),
                format!(
                    "files:     {}",
                    if tool.paths.is_empty() {
                        "-".to_string()
                    } else {
                        tool.paths.join(", ")
                    }
                ),
                request.reason.clone(),
            ],
        });
        // Keep the reply channel until the user decides.
        self.pending_reply = Some(reply);
    }

    fn answer(&mut self, approved: bool) {
        self.view.prompt = None;
        if let Some(reply) = self.pending_reply.take() {
            let _ = reply.send(if approved {
                Approval::Approved
            } else {
                Approval::Denied
            });
        }
        let pending = self.pending_command.take();
        match pending {
            Some(action) => match self.controller.confirm(action, approved) {
                Ok(lines) => {
                    for line in lines {
                        self.view.transcript.push_notice(&line);
                    }
                    self.reload_if_needed("/delete");
                }
                Err(err) => {
                    let message = err.to_string();
                    self.view.transcript.push_error(&message);
                }
            },
            None => {
                self.view
                    .transcript
                    .push_notice(if approved { "approved" } else { "denied" });
            }
        }
        self.view
            .set_message(if approved { "approved" } else { "denied" });
    }

    fn cancel_turn(&mut self) {
        if let Some(turn) = &self.turn {
            turn.cancel();
        }
        self.view.set_message("cancelling…");
    }

    fn submit(&mut self) {
        let text = self.view.input.trim().to_string();
        if text.is_empty() {
            return;
        }
        self.view.input.clear();
        self.view.cursor = 0;
        self.view.scroll_to_bottom();
        if self.running {
            self.view
                .set_message("a task is already running: Esc cancels it");
            return;
        }

        if crate::session::is_command(&text) {
            self.view.transcript.push_user(&text);
            self.run_command(&text);
            return;
        }

        self.view.transcript.push_user(&text);
        let handle = self.controller.start_turn(&text);
        let canceller = handle.canceller();
        let (tx, rx) = mpsc::channel::<EngineEvent>(64);
        let mut events = handle.events;
        // The pump keeps its own handle: when the UI goes away it must be able
        // to stop the provider stream that the UI's copy is still tracking.
        let pump_canceller = canceller.clone();
        tokio::spawn(async move {
            while let Some(event) = events.recv().await {
                if tx.send(event).await.is_err() {
                    // The UI went away: stop the provider stream too.
                    pump_canceller.cancel();
                    return;
                }
            }
        });
        self.turn = Some(canceller);
        self.turn_events = Some(rx);
        self.running = true;
    }

    fn run_command(&mut self, text: &str) {
        match self.controller.handle_command(text) {
            Ok(CommandOutcome::Exit) => self.quit = true,
            Ok(CommandOutcome::Lines(lines)) => {
                for line in lines {
                    self.view.transcript.push_notice(&line);
                }
                self.reload_if_needed(text);
            }
            Ok(CommandOutcome::Confirm { question, action }) => {
                self.pending_command = Some(action);
                self.view.prompt = Some(Prompt {
                    question,
                    details: Vec::new(),
                });
            }
            Err(err) => {
                let message = err.to_string();
                self.view.transcript.push_error(&message);
            }
        }
    }

    /// Reload the view after commands that replace the conversation.
    fn reload_if_needed(&mut self, command: &str) {
        if command.starts_with("/clear") {
            self.view.transcript.reset();
            return;
        }
        if command.starts_with("/new")
            || command.starts_with("/load")
            || command.starts_with("/delete")
        {
            self.view.transcript.reset();
            self.load_history();
        }
    }
}

/// Await the next engine event, or park forever when no turn is running.
async fn next_turn_event(events: Option<&mut mpsc::Receiver<EngineEvent>>) -> Option<EngineEvent> {
    match events {
        Some(receiver) => receiver.recv().await,
        // Never resolves, so the `select!` arm simply never fires.
        None => std::future::pending().await,
    }
}

fn shorten(text: &str, max: usize) -> String {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return "-".to_string();
    }
    if trimmed.chars().count() <= max {
        return trimmed.to_string();
    }
    let shortened: String = trimmed.chars().take(max).collect();
    format!("{shortened}…")
}

fn terminal_error(err: std::io::Error) -> Error {
    Error::internal(format!("terminal error: {err}"))
}
