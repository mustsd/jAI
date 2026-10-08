//! One-shot prompts: `jai deepseek -m deepseek-chat "Explain Rust ownership"`.

use crate::app::App;
use crate::approve::approver_for;
use crate::args::Cli;
use crate::ui::{TurnUi, drive_turn};
use jai_core::error::{Error, Result};
use std::io::{IsTerminal, Read};

/// Run a single prompt and return the process exit code.
pub async fn run(app: &App, cli: &Cli) -> Result<i32> {
    let prompt = match cli.prompt_text() {
        Some(text) => text,
        None => read_stdin()?,
    };
    if prompt.trim().is_empty() {
        return Err(Error::config(
            "no prompt given: pass the text as an argument or pipe it on stdin",
        ));
    }

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
    let engine = app.engine(
        provider,
        &provider_name,
        &model,
        app.mode,
        approver_for(app.mode),
    )?;
    app.remember(&provider_name, &model);

    if app.verbose {
        eprintln!(
            "[session] {} — {} / {} — {}",
            session.id,
            provider_name,
            model,
            app.store.backend()
        );
    }

    let handle = engine.start_turn(&session.id, &prompt);
    let outcome = drive_turn(handle, TurnUi::for_app(app.verbose)).await;
    if let Some(message) = &outcome.message {
        eprintln!("error: {message}");
    }
    Ok(outcome.exit_code)
}

fn read_stdin() -> Result<String> {
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Err(Error::config(
            "no prompt given: pass the text as an argument, or pipe it on stdin",
        ));
    }
    let mut buffer = String::new();
    stdin.lock().read_to_string(&mut buffer)?;
    Ok(buffer)
}
