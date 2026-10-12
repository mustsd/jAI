//! jAI — local-first AI agent.
//!
//! Entry point: parse arguments, assemble the runtime, dispatch to one-shot,
//! interactive, web or management mode, and translate errors into exit codes.

mod app;
mod approve;
mod args;
mod commands;
mod one_shot;
mod repl;
mod session;
mod transcript;
mod tui;
mod ui;
mod view;
mod web_mode;

use args::Cli;
use clap::Parser;

#[tokio::main]
async fn main() {
    let mut cli = Cli::parse();
    let code = match dispatch(&mut cli).await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("jai: {err}");
            err.exit_code()
        }
    };
    std::process::exit(code);
}

async fn dispatch(cli: &mut Cli) -> jai_core::Result<i32> {
    let app = app::App::from_cli(cli)?;
    // `jai Explain Rust ownership` asks the default provider; the first free word
    // only counts as a provider when it is one we know.
    cli.absorb_unknown_provider(&app.config.known_provider_names());

    if let Some(command) = cli.command.take() {
        return commands::run(&app, cli, command).await;
    }
    if cli.web {
        return web_mode::run(app, cli).await;
    }
    if cli.agent {
        let controller = session::SessionController::new(app, cli)?;
        if tui::wanted(cli) {
            return tui::run(controller).await;
        }
        return repl::run(controller).await;
    }
    one_shot::run(&app, cli).await
}
