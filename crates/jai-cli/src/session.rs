//! One session, driven by any front end.
//!
//! The line REPL and the full-screen TUI used to duplicate the "switch model,
//! switch provider, switch mode, run a turn" logic. It lives here once: both
//! front ends own a [`SessionController`] and render whatever it returns, which
//! is also what makes the two modes behave identically.

use crate::app::App;
use crate::approve::approver_for;
use crate::args::Cli;
use jai_core::engine::TurnHandle;
use jai_core::error::Result;
use jai_core::message::PermissionMode;
use jai_core::store::{Session, session_markdown};
use jai_core::time::unix_now;
use jai_core::policy::Approver;
use jai_core::{Engine, Provider};
use std::sync::Arc;

/// The text shown by `/help`.
pub const HELP: &str = "\
Commands:
  /help                 show this help
  /model [ID]           show or switch the model
  /provider [NAME]      show or switch the provider
  /mode [safe|yolo]     show or switch the tool permission mode
  /system [TEXT]        show, set (TEXT) or clear (\"-\") the system prompt
  /thinking [on|off]    show or hide the model's reasoning text
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

/// What a slash command asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandOutcome {
    /// Printed lines; the caller decides where they go.
    Lines(Vec<String>),
    Exit,
    /// The command needs an interactive yes/no question (`/delete`).
    Confirm { question: String, action: ConfirmAction },
}

/// Follow-up for a confirmation-requiring command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmAction {
    DeleteCurrentSession,
}

impl CommandOutcome {
    fn lines<I: IntoIterator<Item = String>>(lines: I) -> Self {
        Self::Lines(lines.into_iter().collect())
    }
}

/// How a session was chosen when jAI started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionOrigin {
    /// A brand new conversation.
    New,
    /// Resumed because the workspace matched (`jai -a` in the same directory).
    Workspace { workspace: String },
    /// Resumed because `--continue`/`--session` asked for it.
    Requested,
    /// `--new` forced a fresh session even though one existed.
    ForcedNew { previous: String },
}

impl SessionOrigin {
    /// One line telling the user what happened, or `None` for a plain new
    /// session (which the banner already says).
    pub fn note(&self) -> Option<String> {
        match self {
            SessionOrigin::New => None,
            SessionOrigin::Workspace { workspace } => Some(format!(
                "resumed the last session used in {workspace} (jai -a -c resumes the most recent anywhere, --new starts fresh)"
            )),
            SessionOrigin::Requested => Some("resumed the requested session".to_string()),
            SessionOrigin::ForcedNew { previous } => {
                Some(format!("started a new session (--new); {previous} is still stored"))
            }
        }
    }
}

/// Provider, model, mode, session and engine for one running agent.
pub struct SessionController {
    app: App,
    provider_name: String,
    provider: Arc<dyn Provider>,
    model: String,
    mode: PermissionMode,
    session: Session,
    engine: Arc<Engine>,
    base_url: Option<String>,
    key_override: Option<String>,
    origin: SessionOrigin,
    show_thinking: bool,
    /// Front-end supplied approver (the TUI asks in its own footer). When
    /// `None`, confirmation happens on the terminal or is automatic in yolo.
    approver_override: Option<Arc<dyn Approver>>,
}

impl SessionController {
    /// Resolve the provider/model, pick the session to resume, and build the
    /// engine — the whole "start an agent" sequence.
    pub fn new(app: App, cli: &Cli) -> Result<Self> {
        let provider_name = app.provider_name(cli)?;
        let resolved = app.resolved_provider(cli, &provider_name)?;
        let model = app.model_name(cli, &resolved)?;
        let provider = app.provider(&resolved)?;
        let (session, origin) = app.open_session_with_origin(
            cli.session.as_deref(),
            cli.continue_session,
            cli.new,
            cli.no_resume,
            &provider_name,
            &model,
        )?;
        app.remember(&provider_name, &model);

        let mode = app.mode;
        let engine = app.engine(
            Arc::clone(&provider),
            &provider_name,
            &model,
            mode,
            approver_for(mode),
        )?;
        let workspace = app.workspace.display().to_string();
        app.store.set_session_workspace(&session.id, &workspace)?;

        Ok(Self {
            show_thinking: app.show_thinking,
            app,
            provider_name,
            provider,
            model,
            mode,
            session,
            engine,
            base_url: cli.base_url.clone(),
            key_override: cli.api_key.clone(),
            origin,
            approver_override: None,
        })
    }

    /// Let a front end collect confirmations itself (the TUI does this in its
    /// footer, because the terminal is in raw mode and stdin is unavailable).
    pub fn with_approver(mut self, approver: Arc<dyn Approver>) -> Result<Self> {
        self.approver_override = Some(approver);
        self.rebuild()?;
        Ok(self)
    }

    /// Approver to use for the current mode.
    fn approver(&self) -> Arc<dyn Approver> {
        match &self.approver_override {
            Some(approver) => Arc::clone(approver),
            None => approver_for(self.mode),
        }
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn provider_name(&self) -> &str {
        &self.provider_name
    }

    pub fn model(&self) -> &str {
        &self.model
    }

    pub fn mode(&self) -> PermissionMode {
        self.mode
    }

    pub fn show_thinking(&self) -> bool {
        self.show_thinking
    }

    pub fn set_show_thinking(&mut self, show: bool) {
        self.show_thinking = show;
    }

    /// Start a turn; the caller drives the returned handle.
    pub fn start_turn(&self, input: &str) -> TurnHandle {
        self.engine.start_turn(&self.session.id, input)
    }

    /// Snapshot of the engine settings (context window, mode, ...).
    pub fn engine_settings(&self) -> jai_core::EngineSettings {
        self.engine.settings()
    }

    /// What the next request would carry, for the `Cxt:` figure on the status
    /// line before the first turn of a session.
    pub fn context_estimate(&self) -> Result<jai_core::ContextUsage> {
        self.engine.context_estimate(&self.session.id)
    }

    /// Banner lines shown when a front end starts.
    pub fn banner(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "jAI {} — {} / {}",
            env!("CARGO_PKG_VERSION"),
            self.provider_name,
            self.model
        )];
        let message_count = self
            .app
            .store
            .messages(&self.session.id)
            .map(|messages| messages.len())
            .unwrap_or(0);
        let title = if self.session.title.is_some() {
            format!(" — {}", self.session.display_title())
        } else {
            String::new()
        };
        lines.push(format!(
            "session   {} ({message_count} messages){title}",
            self.session.id
        ));
        match self.mode {
            PermissionMode::Safe => lines.push("mode      safe (every tool call is confirmed)".into()),
            PermissionMode::Yolo => lines.push("mode      yolo (no confirmation prompts)".into()),
        }
        lines.push(format!(
            "thinking  {}",
            if self.show_thinking {
                "shown"
            } else {
                "hidden (/thinking on to show)"
            }
        ));
        match self.app.tools() {
            Ok(Some(tools)) => lines.push(format!("tools     {}", tools.enabled_tools().join(", "))),
            Ok(None) => lines.push("tools     disabled".into()),
            Err(err) => lines.push(format!("tools     unavailable: {err}")),
        }
        lines.push(format!("workspace {}", self.app.workspace.display()));
        lines.push(format!(
            "storage   {} ({})",
            self.app.store.backend(),
            self.app.paths.database_file().display()
        ));
        if let Some(note) = self.origin.note() {
            lines.push(note);
        }
        lines
    }

    /// Tool calls that never finished in an earlier run.
    pub fn interrupted(&self) -> Vec<String> {
        let interrupted = match self.app.store.reconcile_interrupted() {
            Ok(entries) => entries,
            Err(err) => return vec![format!("warning: could not check the audit trail: {err}")],
        };
        if interrupted.is_empty() {
            return Vec::new();
        }
        let mut lines = vec![format!(
            "warning: {} tool call(s) did not finish in an earlier run:",
            interrupted.len()
        )];
        for entry in &interrupted {
            lines.push(format!(
                "  session {} — {} ({}) status: interrupted, started {}",
                entry.session_id,
                entry.tool_name,
                entry.summary.as_deref().unwrap_or(""),
                jai_core::time::format_relative(unix_now(), entry.created_at)
            ));
            if entry.session_id == self.session.id {
                lines.push("  (this is the session you are resuming)".to_string());
            }
        }
        lines.push(
            "jAI will not repeat them automatically; re-run anything that is still needed.".into(),
        );
        lines
    }

    fn rebuild(&mut self) -> Result<()> {
        let approver = self.approver();
        self.engine = self.app.engine(
            Arc::clone(&self.provider),
            &self.provider_name,
            &self.model,
            self.mode,
            approver,
        )?;
        Ok(())
    }

    fn switch_provider(&mut self, name: &str) -> Result<()> {
        let name = name.trim().to_ascii_lowercase();
        let resolved =
            self.app
                .resolved_provider_raw(&name, self.base_url.as_deref(), self.key_override.as_deref())?;
        let provider = self.app.provider(&resolved)?;
        self.provider_name = name;
        self.provider = provider;
        if self.model.is_empty() {
            if let Some(default_model) = &resolved.default_model {
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

    /// Run a slash command.
    pub fn handle_command(&mut self, line: &str) -> Result<CommandOutcome> {
        let mut parts = line.splitn(2, char::is_whitespace);
        let command = parts.next().unwrap_or("").to_ascii_lowercase();
        let argument = parts.next().unwrap_or("").trim().to_string();

        match command.as_str() {
            "/help" | "/?" => Ok(CommandOutcome::Lines(
                HELP.lines().map(str::to_string).collect(),
            )),
            "/exit" | "/quit" => Ok(CommandOutcome::Exit),
            "/model" => {
                if argument.is_empty() {
                    return Ok(CommandOutcome::lines([format!("model: {}", self.model)]));
                }
                self.model = argument.clone();
                self.rebuild()?;
                self.app.store.update_session_model(
                    &self.session.id,
                    &self.provider_name,
                    &self.model,
                )?;
                self.app.remember(&self.provider_name, &self.model);
                Ok(CommandOutcome::lines([format!("model set to {}", self.model)]))
            }
            "/provider" => {
                if argument.is_empty() {
                    return Ok(CommandOutcome::lines([format!(
                        "provider: {} ({})",
                        self.provider_name,
                        self.provider.base_url()
                    )]));
                }
                match self.switch_provider(&argument) {
                    Ok(()) => Ok(CommandOutcome::lines([format!(
                        "provider set to {} ({})",
                        self.provider_name,
                        self.provider.base_url()
                    )])),
                    Err(err) => Ok(CommandOutcome::lines([format!(
                        "could not switch provider: {err}"
                    )])),
                }
            }
            "/mode" => match argument.as_str() {
                "" => Ok(CommandOutcome::lines([format!("mode: {}", self.mode)])),
                other => match other.parse::<PermissionMode>() {
                    Ok(mode) => {
                        self.mode = mode;
                        self.rebuild()?;
                        Ok(CommandOutcome::lines([match mode {
                            PermissionMode::Safe => {
                                "mode: safe (every tool call is confirmed)".to_string()
                            }
                            PermissionMode::Yolo => "mode: yolo (no confirmation prompts; the policy engine still blocks disabled tools, paths outside the workspace and sensitive files)".to_string(),
                        }]))
                    }
                    Err(err) => Ok(CommandOutcome::lines([err])),
                },
            },
            "/thinking" => {
                match argument.as_str() {
                    "" => {
                        return Ok(CommandOutcome::lines([format!(
                            "thinking: {}",
                            if self.show_thinking { "shown" } else { "hidden" }
                        )]));
                    }
                    "on" | "show" | "yes" => self.show_thinking = true,
                    "off" | "hide" | "no" => self.show_thinking = false,
                    other => {
                        return Ok(CommandOutcome::lines([format!(
                            "unexpected value `{other}` (expected on|off)"
                        )]));
                    }
                }
                Ok(CommandOutcome::lines([format!(
                    "thinking: {}",
                    if self.show_thinking {
                        "shown"
                    } else {
                        "hidden"
                    }
                )]))
            }
            "/system" => {
                if argument.is_empty() {
                    return Ok(CommandOutcome::lines([match &self.app.system_prompt {
                        Some(prompt) => format!("system prompt:\n{prompt}"),
                        None => "no system prompt set".to_string(),
                    }]));
                }
                if argument == "-" {
                    self.app.system_prompt = None;
                    self.engine.set_system_prompt(None)?;
                    return Ok(CommandOutcome::lines(["system prompt cleared".to_string()]));
                }
                self.app.system_prompt = Some(argument.clone());
                self.engine.set_system_prompt(Some(argument))?;
                Ok(CommandOutcome::lines(["system prompt updated".to_string()]))
            }
            "/tools" => {
                let mut lines = Vec::new();
                match self.app.tools() {
                    Ok(Some(tools)) => {
                        lines.push(format!("tools: {}", tools.enabled_tools().join(", ")))
                    }
                    Ok(None) => lines.push("tools: disabled".to_string()),
                    Err(err) => lines.push(format!("tools unavailable: {err}")),
                }
                lines.push(format!("workspace: {}", self.app.workspace.display()));
                lines.push(format!("mode: {}", self.mode));
                Ok(CommandOutcome::Lines(lines))
            }
            "/sessions" => Ok(CommandOutcome::lines(
                crate::commands::session_table(self.app.store.as_ref())?,
            )),
            "/session" => {
                let messages = self.app.store.messages(&self.session.id)?;
                Ok(CommandOutcome::lines([
                    format!("session:  {}", self.session.id),
                    format!("title:    {}", self.session.display_title()),
                    format!("provider: {}", self.provider_name),
                    format!("model:    {}", self.model),
                    format!("messages: {}", messages.len()),
                    format!(
                        "workspace: {}",
                        self.app
                            .store
                            .session_workspace(&self.session.id)?
                            .unwrap_or_else(|| "(not recorded)".to_string())
                    ),
                ]))
            }
            "/new" => {
                self.session =
                    self.app
                        .store
                        .create_session(&self.provider_name, &self.model, None)?;
                let workspace = self.app.workspace.display().to_string();
                self.app
                    .store
                    .set_session_workspace(&self.session.id, &workspace)?;
                Ok(CommandOutcome::lines([format!(
                    "started session {}",
                    self.session.id
                )]))
            }
            "/load" => {
                if argument.is_empty() {
                    return Ok(CommandOutcome::lines(["usage: /load <session id>".to_string()]));
                }
                let session = if argument.eq_ignore_ascii_case("latest") {
                    self.app.store.latest_session()?
                } else {
                    self.app.store.get_session(&argument)?
                };
                match session {
                    Some(session) => {
                        let count = self.app.store.messages(&session.id)?.len();
                        self.session = session;
                        Ok(CommandOutcome::lines([format!(
                            "loaded {} ({count} messages)",
                            self.session.id
                        )]))
                    }
                    None => Ok(CommandOutcome::lines([format!(
                        "unknown session `{argument}`"
                    )])),
                }
            }
            "/rename" => {
                if argument.is_empty() {
                    return Ok(CommandOutcome::lines(["usage: /rename <title>".to_string()]));
                }
                self.app.store.rename_session(&self.session.id, &argument)?;
                Ok(CommandOutcome::lines([format!("renamed to `{argument}`")]))
            }
            "/clear" => {
                let removed = self.app.store.clear_messages(&self.session.id)?;
                Ok(CommandOutcome::lines([format!(
                    "cleared {removed} message(s) from {}",
                    self.session.id
                )]))
            }
            "/delete" => Ok(CommandOutcome::Confirm {
                question: format!("Delete session {} and its messages?", self.session.id),
                action: ConfirmAction::DeleteCurrentSession,
            }),
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
                    Ok(CommandOutcome::Lines(markdown.lines().map(str::to_string).collect()))
                } else {
                    std::fs::write(&argument, markdown)?;
                    Ok(CommandOutcome::lines([format!("exported to {argument}")]))
                }
            }
            "/tokens" => {
                let usage = self.app.store.session_usage(&self.session.id)?;
                Ok(CommandOutcome::lines([format!(
                    "{} prompt + {} completion = {} tokens",
                    usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
                )]))
            }
            other => Ok(CommandOutcome::lines([format!(
                "unknown command `{other}` (try /help)"
            )])),
        }
    }

    /// Carry out a confirmation the front end collected.
    pub fn confirm(&mut self, action: ConfirmAction, approved: bool) -> Result<Vec<String>> {
        match action {
            ConfirmAction::DeleteCurrentSession if approved => {
                self.app.store.delete_session(&self.session.id)?;
                self.session =
                    self.app
                        .store
                        .create_session(&self.provider_name, &self.model, None)?;
                let workspace = self.app.workspace.display().to_string();
                self.app
                    .store
                    .set_session_workspace(&self.session.id, &workspace)?;
                Ok(vec![format!("deleted; started session {}", self.session.id)])
            }
            ConfirmAction::DeleteCurrentSession => Ok(vec!["not deleted".to_string()]),
        }
    }

    /// Note to print when the front end exits.
    pub fn farewell(&self) -> String {
        format!(
            "bye — resume this session with: jai -a --session {}",
            self.session.id
        )
    }

    /// Re-check the state directory permissions on the way out.
    pub fn warn_if_state_dir_is_readable(&self) {
        warn_if_state_dir_is_readable(&self.app.paths);
    }

}

/// Warn when the state directory is readable by other users.
pub fn warn_if_state_dir_is_readable(paths: &jai_config::Paths) {
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

/// True when the line drawn by the front end should be treated as a command.
pub fn is_command(line: &str) -> bool {
    line.starts_with('/')
}
