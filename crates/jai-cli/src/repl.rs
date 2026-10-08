//! Interactive agent mode (`jai -a` / `jai --agent`).

use crate::app::App;
use crate::approve::approver_for;
use crate::args::Cli;
use crate::ui::{TurnUi, drive_turn};
use jai_core::error::{Error, Result};
use jai_core::message::PermissionMode;
use jai_core::store::{Session, session_markdown};
use jai_core::time::unix_now;
use jai_core::{Engine, Provider};
use rustyline::DefaultEditor;
use rustyline::error::ReadlineError;
use std::sync::Arc;

const HELP: &str = "\
Commands:
  /help                 show this help
  /model [ID]           show or switch the model
  /provider [NAME]      show or switch the provider
  /mode [safe|yolo]     show or switch the tool permission mode
  /system [TEXT]        show, set (TEXT) or clear (\"-\") the system prompt
  /tools                list the tools this session may use
  /sessions             list stored sessions
  /session              show the current session
  /new                  start a new session
  /load ID              switch to a stored session
  /rename TITLE         rename the current session
  /clear                delete the messages of the current session
  /delete               delete the current session and start a new one
  /export [FILE]        export the session as Markdown
  /tokens               show token usage for the current session
  /exit, /quit          leave jAI

Anything else is sent to the model.";

/// What a command asked for.
enum Outcome {
    Continue,
    Exit,
}

/// Mutable REPL state: the engine is rebuilt when provider, model or mode
/// changes, always sharing the same store, tools and policy rules.
struct ReplState {
    app: App,
    provider_name: String,
    provider: Arc<dyn Provider>,
    model: String,
    mode: PermissionMode,
    session: Session,
    engine: Arc<Engine>,
    base_url: Option<String>,
    api_key: Option<String>,
}

impl ReplState {
    fn rebuild(&mut self) -> Result<()> {
        self.engine = self.app.engine(
            Arc::clone(&self.provider),
            &self.provider_name,
            &self.model,
            self.mode,
            approver_for(self.mode),
        )?;
        Ok(())
    }

    fn switch_provider(&mut self, name: &str) -> Result<()> {
        let name = name.trim().to_ascii_lowercase();
        let resolved = self.app.resolved_provider_raw(
            &name,
            self.base_url.as_deref(),
            self.api_key.as_deref(),
        )?;
        let provider = self.app.provider(&resolved)?;
        self.provider_name = name;
        self.provider = provider;
        if let Some(default_model) = &resolved.default_model {
            if self.model.is_empty() {
                self.model = default_model.clone();
            }
        }
        self.rebuild()?;
        self.app
            .store
            .update_session_model(&self.session.id, &self.provider_name, &self.model)?;
        self.app.remember(&self.provider_name, &self.model);
        Ok(())
    }

    async fn handle_command(&mut self, line: &str) -> Result<Outcome> {
        let mut parts = line.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("").to_ascii_lowercase();
        let argument = parts.next().unwrap_or("").trim().to_string();

        match command.as_str() {
            "/help" | "/?" => println!("{HELP}"),
            "/exit" | "/quit" => return Ok(Outcome::Exit),
            "/model" => {
                if argument.is_empty() {
                    println!("model: {}", self.model);
                } else {
                    self.model = argument.clone();
                    self.rebuild()?;
                    self.app.store.update_session_model(
                        &self.session.id,
                        &self.provider_name,
                        &self.model,
                    )?;
                    self.app.remember(&self.provider_name, &self.model);
                    println!("model set to {}", self.model);
                }
            }
            "/provider" => {
                if argument.is_empty() {
                    println!(
                        "provider: {} ({})",
                        self.provider_name,
                        self.provider.base_url()
                    );
                } else {
                    match self.switch_provider(&argument) {
                        Ok(()) => println!(
                            "provider set to {} ({})",
                            self.provider_name,
                            self.provider.base_url()
                        ),
                        Err(err) => eprintln!("could not switch provider: {err}"),
                    }
                }
            }
            "/mode" => match argument.as_str() {
                "" => println!("mode: {}", self.mode),
                other => match other.parse::<PermissionMode>() {
                    Ok(mode) => {
                        self.mode = mode;
                        self.rebuild()?;
                        match mode {
                            PermissionMode::Safe => {
                                println!("mode: safe (every tool call is confirmed)")
                            }
                            PermissionMode::Yolo => println!(
                                "mode: yolo (no confirmation prompts; the policy engine still blocks disabled tools, paths outside the workspace and sensitive files)"
                            ),
                        }
                    }
                    Err(err) => eprintln!("{err}"),
                },
            },
            "/system" => {
                if argument.is_empty() {
                    match &self.app.system_prompt {
                        Some(prompt) => println!("system prompt:\n{prompt}"),
                        None => println!("no system prompt set"),
                    }
                } else if argument == "-" {
                    self.app.system_prompt = None;
                    self.engine.set_system_prompt(None)?;
                    println!("system prompt cleared");
                } else {
                    self.app.system_prompt = Some(argument.clone());
                    self.engine.set_system_prompt(Some(argument))?;
                    println!("system prompt updated");
                }
            }
            "/tools" => {
                match self.app.tools()? {
                    Some(tools) => println!("tools: {}", tools.enabled_tools().join(", ")),
                    None => println!("tools: disabled"),
                }
                println!("workspace: {}", self.app.workspace.display());
                println!("mode: {}", self.mode);
            }
            "/sessions" => crate::commands::print_sessions(self.app.store.as_ref(), false)?,
            "/session" => {
                println!("session:  {}", self.session.id);
                println!("title:    {}", self.session.display_title());
                println!("provider: {}", self.provider_name);
                println!("model:    {}", self.model);
                let messages = self.app.store.messages(&self.session.id)?;
                println!("messages: {}", messages.len());
            }
            "/new" => {
                self.session =
                    self.app
                        .store
                        .create_session(&self.provider_name, &self.model, None)?;
                println!("started session {}", self.session.id);
            }
            "/load" => {
                if argument.is_empty() {
                    eprintln!("usage: /load <session id>");
                } else {
                    let session = if argument.eq_ignore_ascii_case("latest") {
                        self.app.store.latest_session()?
                    } else {
                        self.app.store.get_session(&argument)?
                    };
                    match session {
                        Some(session) => {
                            self.session = session;
                            println!(
                                "loaded {} ({} messages)",
                                self.session.id,
                                self.app.store.messages(&self.session.id)?.len()
                            );
                        }
                        None => eprintln!("unknown session `{argument}`"),
                    }
                }
            }
            "/rename" => {
                if argument.is_empty() {
                    eprintln!("usage: /rename <title>");
                } else {
                    self.app.store.rename_session(&self.session.id, &argument)?;
                    println!("renamed to `{argument}`");
                }
            }
            "/clear" => {
                let removed = self.app.store.clear_messages(&self.session.id)?;
                println!("cleared {removed} message(s) from {}", self.session.id);
            }
            "/delete" => {
                if confirm(&format!(
                    "Delete session {} and its messages? [y/N] ",
                    self.session.id
                )) {
                    self.app.store.delete_session(&self.session.id)?;
                    self.session =
                        self.app
                            .store
                            .create_session(&self.provider_name, &self.model, None)?;
                    println!("deleted; started session {}", self.session.id);
                } else {
                    println!("not deleted");
                }
            }
            "/export" => {
                let session = self
                    .app
                    .store
                    .get_session(&self.session.id)?
                    .unwrap_or_else(|| self.session.clone());
                let markdown = session_markdown(
                    &session,
                    &self.app.store.messages(&self.session.id)?,
                    &self.app.store.tool_audit(&self.session.id)?,
                );
                if argument.is_empty() {
                    print!("{markdown}");
                } else {
                    std::fs::write(&argument, markdown)?;
                    println!("exported to {argument}");
                }
            }
            "/tokens" => {
                let usage = self.app.store.session_usage(&self.session.id)?;
                println!(
                    "{} prompt + {} completion = {} tokens",
                    usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                );
            }
            other => eprintln!("unknown command `{other}` (try /help)"),
        }
        Ok(Outcome::Continue)
    }
}

/// Run the interactive agent.
pub async fn run(app: App, cli: &Cli) -> Result<i32> {
    let provider_name = app.provider_name(cli)?;
    let resolved = app.resolved_provider(cli, &provider_name)?;
    let model = app.model_name(cli, &resolved)?;
    let provider = app.provider(&resolved)?;
    let session = app.open_session(
        cli.session.as_deref(),
        cli.continue_session,
        cli.new,
        &provider_name,
        &model,
    )?;

    report_interrupted(&app, &session)?;
    app.remember(&provider_name, &model);

    let mode = app.mode;
    let engine = app.engine(
        Arc::clone(&provider),
        &provider_name,
        &model,
        mode,
        approver_for(mode),
    )?;

    let mut state = ReplState {
        app,
        provider_name,
        provider,
        model,
        mode,
        session,
        engine,
        base_url: cli.base_url.clone(),
        api_key: cli.api_key.clone(),
    };

    print_banner(&state);

    let mut editor = DefaultEditor::new()
        .map_err(|err| Error::internal(format!("could not start the line editor: {err}")))?;
    let history_path = state.app.paths.root().join("history.txt");
    let _ = editor.load_history(&history_path);

    loop {
        match editor.readline("You> ") {
            Ok(line) => {
                let text = line.trim().to_string();
                if text.is_empty() {
                    continue;
                }
                let _ = editor.add_history_entry(text.as_str());
                if text.starts_with('/') {
                    match state.handle_command(&text).await {
                        Ok(Outcome::Exit) => break,
                        Ok(Outcome::Continue) => {}
                        Err(err) => eprintln!("error: {err}"),
                    }
                    continue;
                }

                let handle = state.engine.start_turn(&state.session.id, &text);
                let canceller = handle.canceller();
                let interrupt = tokio::spawn(async move {
                    if tokio::signal::ctrl_c().await.is_ok() {
                        canceller.cancel();
                    }
                });
                let outcome = drive_turn(handle, TurnUi::for_app(state.app.verbose)).await;
                interrupt.abort();
                if let Some(message) = &outcome.message {
                    eprintln!("error: {message}");
                }
            }
            // Ctrl-C at the prompt clears the line instead of leaving jAI.
            Err(ReadlineError::Interrupted) => continue,
            Err(ReadlineError::Eof) => break,
            Err(err) => {
                eprintln!("input error: {err}");
                break;
            }
        }
    }

    let _ = editor.save_history(&history_path);
    warn_if_state_dir_is_readable(&state.app.paths);
    println!(
        "bye — resume this session with: jai -a --session {}",
        state.session.id
    );
    Ok(0)
}

fn print_banner(state: &ReplState) {
    println!(
        "jAI {} — {} / {}",
        env!("CARGO_PKG_VERSION"),
        state.provider_name,
        state.model
    );
    println!(
        "session   {} ({:?} messages){}",
        state.session.id,
        state
            .app
            .store
            .messages(&state.session.id)
            .map(|m| m.len())
            .unwrap_or(0),
        if state.session.title.is_some() {
            format!(" — {}", state.session.display_title())
        } else {
            String::new()
        }
    );
    match state.mode {
        PermissionMode::Safe => println!("mode      safe (every tool call is confirmed)"),
        PermissionMode::Yolo => println!("mode      yolo (no confirmation prompts)"),
    }
    match state.app.tools() {
        Ok(Some(tools)) => println!("tools     {}", tools.enabled_tools().join(", ")),
        Ok(None) => println!("tools     disabled"),
        Err(err) => println!("tools     unavailable: {err}"),
    }
    println!("workspace {}", state.app.workspace.display());
    println!(
        "storage   {} ({})",
        state.app.store.backend(),
        state.app.paths.database_file().display()
    );
    println!("type /help for commands, Ctrl-D to exit\n");
}

/// Warn about tool calls that did not finish before the last exit.
fn report_interrupted(app: &App, session: &Session) -> Result<()> {
    let interrupted = app.store.reconcile_interrupted()?;
    if interrupted.is_empty() {
        return Ok(());
    }
    eprintln!(
        "warning: {} tool call(s) did not finish in an earlier run:",
        interrupted.len()
    );
    for entry in &interrupted {
        eprintln!(
            "  session {} — {} ({}) status: interrupted, started {}",
            entry.session_id,
            entry.tool_name,
            entry.summary.as_deref().unwrap_or(""),
            jai_core::time::format_relative(unix_now(), entry.created_at)
        );
        if entry.session_id == session.id {
            eprintln!("  (this is the session you are resuming)");
        }
    }
    eprintln!("jAI will not repeat them automatically; re-run anything that is still needed.\n");
    Ok(())
}

fn confirm(prompt: &str) -> bool {
    use std::io::Write;
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

fn warn_if_state_dir_is_readable(paths: &jai_config::Paths) {
    // Re-check permissions on exit so a user notices a loosened state directory.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(metadata) = std::fs::metadata(paths.root()) {
            if metadata.permissions().mode() & 0o077 != 0 {
                eprintln!(
                    "note: {} is readable by other users (mode {:o}); run `chmod 700 {}`",
                    paths.root().display(),
                    metadata.permissions().mode() & 0o777,
                    paths.root().display()
                );
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = paths;
    }
}
