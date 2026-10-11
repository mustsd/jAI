//! Management subcommands: sessions, session, models, providers, config,
//! doctor and purge.

use crate::app::App;
use crate::args::{Cli, Command};
use jai_config::providers;
use jai_core::error::{Error, Result};
use jai_core::store::{Session, SessionStore, session_markdown};
use jai_core::time;
use jai_db::SqliteStore;
use std::io::{BufRead, Write};
use std::path::Path;

/// Run a management subcommand, returning a process exit code.
pub async fn run(app: &App, cli: &Cli, command: Command) -> Result<i32> {
    match command {
        Command::Sessions { json } => {
            print_sessions(app.store.as_ref(), json)?;
            Ok(0)
        }
        Command::Session {
            id,
            delete,
            export,
            output,
            rename,
            audit,
        } => run_session(
            app,
            &id,
            delete,
            export,
            output.as_deref(),
            rename.as_deref(),
            audit,
        ),
        Command::Models { provider } => run_models(app, cli, provider.as_deref()).await,
        Command::Providers => {
            print_providers(app);
            Ok(0)
        }
        Command::Config { init } => run_config(app, init),
        Command::Doctor => run_doctor(app),
        Command::Purge { yes } => run_purge(app, yes),
    }
}

/// Lines of the session table, for front ends that render it themselves.
pub fn session_table(store: &dyn SessionStore) -> Result<Vec<String>> {
    let summaries = store.list_sessions()?;
    if summaries.is_empty() {
        return Ok(vec![
            "no stored sessions yet. Start one with `jai -a` or `jai deepseek \"hello\"`."
                .to_string(),
        ]);
    }
    let now = time::unix_now();
    let mut lines = vec![format!(
        "{:<16}  {:<34}  {:>8}  {:<12}  MODEL",
        "ID", "TITLE", "MESSAGES", "UPDATED"
    )];
    for summary in summaries {
        lines.push(format!(
            "{:<16}  {:<34}  {:>8}  {:<12}  {}",
            summary.session.id,
            truncate(summary.session.display_title(), 34),
            summary.message_count,
            time::format_relative(now, summary.session.updated_at),
            summary.session.model,
        ));
    }
    Ok(lines)
}

/// Table of stored sessions.
pub fn print_sessions(store: &dyn SessionStore, json: bool) -> Result<()> {
    let summaries = store.list_sessions()?;
    if json {
        let payload = serde_json::Value::Array(
            summaries
                .iter()
                .map(|summary| {
                    serde_json::json!({
                        "id": summary.session.id,
                        "title": summary.session.display_title(),
                        "provider": summary.session.provider,
                        "model": summary.session.model,
                        "messages": summary.message_count,
                        "created_at": summary.session.created_at,
                        "updated_at": summary.session.updated_at,
                    })
                })
                .collect(),
        );
        println!(
            "{}",
            serde_json::to_string_pretty(&payload).unwrap_or_default()
        );
        return Ok(());
    }

    if summaries.is_empty() {
        println!("no stored sessions yet. Start one with `jai -a` or `jai deepseek \"hello\"`.");
        return Ok(());
    }
    let now = time::unix_now();
    println!(
        "{:<16}  {:<34}  {:>8}  {:<12}  MODEL",
        "ID", "TITLE", "MESSAGES", "UPDATED"
    );
    for summary in summaries {
        println!(
            "{:<16}  {:<34}  {:>8}  {:<12}  {}",
            summary.session.id,
            truncate(summary.session.display_title(), 34),
            summary.message_count,
            time::format_relative(now, summary.session.updated_at),
            summary.session.model,
        );
    }
    Ok(())
}

fn truncate(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let shortened: String = text.chars().take(max.saturating_sub(1)).collect();
    format!("{shortened}…")
}

fn resolve_session(store: &dyn SessionStore, id: &str) -> Result<Session> {
    let session = if id.eq_ignore_ascii_case("latest") {
        store.latest_session()?
    } else {
        store.get_session(id)?
    };
    session.ok_or_else(|| {
        Error::config(format!(
            "unknown session `{id}`; run `jai sessions` to list stored conversations"
        ))
    })
}

#[allow(clippy::too_many_arguments)]
fn run_session(
    app: &App,
    id: &str,
    delete: bool,
    export: bool,
    output: Option<&Path>,
    rename: Option<&str>,
    audit: bool,
) -> Result<i32> {
    let store = app.store.as_ref();
    let session = resolve_session(store, id)?;
    let mut acted = false;

    if let Some(title) = rename {
        if store.rename_session(&session.id, title)? {
            println!("renamed {} to `{title}`", session.id);
        } else {
            return Err(Error::config(format!("could not rename `{}`", session.id)));
        }
        acted = true;
    }

    if delete {
        if store.delete_session(&session.id)? {
            println!("deleted session {} and its messages", session.id);
        } else {
            return Err(Error::config(format!("could not delete `{}`", session.id)));
        }
        acted = true;
    }

    if export {
        let session = store
            .get_session(&session.id)?
            .unwrap_or_else(|| session.clone());
        let markdown = session_markdown(
            &session,
            &store.messages(&session.id)?,
            &store.tool_audit(&session.id)?,
        );
        match output {
            Some(path) => {
                if let Some(parent) = path.parent() {
                    if !parent.as_os_str().is_empty() {
                        std::fs::create_dir_all(parent)?;
                    }
                }
                std::fs::write(path, markdown)?;
                println!("exported {} to {}", session.id, path.display());
            }
            None => print!("{markdown}"),
        }
        acted = true;
    }

    if !acted {
        let session = store
            .get_session(&session.id)?
            .unwrap_or_else(|| session.clone());
        println!("session:  {}", session.id);
        println!("title:    {}", session.display_title());
        println!("provider: {}", session.provider);
        println!("model:    {}", session.model);
        println!("created:  {}", time::format_timestamp(session.created_at));
        println!("updated:  {}", time::format_timestamp(session.updated_at));
        let usage = store.session_usage(&session.id)?;
        if !usage.is_empty() {
            println!(
                "tokens:   {} prompt + {} completion = {}",
                usage.prompt_tokens, usage.completion_tokens, usage.total_tokens
            );
        }
        println!(
            "\nTo continue this session: jai -a --session {}",
            session.id
        );
        for message in store.messages(&session.id)? {
            if message.content.trim().is_empty() && message.tool_calls.is_empty() {
                continue;
            }
            println!("\n--- {} ---", message.role);
            println!("{}", message.content.trim_end());
            for call in &message.tool_calls {
                println!("[tool call] {} {}", call.name, call.arguments);
            }
        }
    }

    if audit {
        let entries = store.tool_audit(&session.id)?;
        if entries.is_empty() {
            println!("\nNo tool activity recorded for {}.", session.id);
        } else {
            println!("\nTOOL ACTIVITY (audit)");
            for entry in entries {
                println!(
                    "{}  {:<20} [{} / {}] decision={} status={}  {}",
                    time::format_timestamp(entry.created_at),
                    entry.tool_name,
                    entry.mode,
                    entry.risk,
                    entry.decision.as_deref().unwrap_or("-"),
                    entry.status,
                    entry.summary.as_deref().unwrap_or(""),
                );
            }
        }
    }
    Ok(0)
}

async fn run_models(app: &App, cli: &Cli, provider: Option<&str>) -> Result<i32> {
    let name = match provider {
        Some(name) => name.to_ascii_lowercase(),
        None => app.provider_name(cli)?,
    };
    let resolved = app.resolved_provider(cli, &name)?;
    let provider = app.provider(&resolved)?;
    let models = provider.list_models().await?;
    if models.is_empty() {
        println!("{name} advertised no models at {}", resolved.base_url);
        return Ok(0);
    }
    for model in models {
        match model.owned_by {
            Some(owner) if !owner.is_empty() => println!("{}  ({owner})", model.id),
            _ => println!("{}", model.id),
        }
    }
    Ok(0)
}

fn print_providers(app: &App) {
    println!(
        "{:<12}  {:<34}  {:<22}  KEY",
        "PROVIDER", "BASE URL", "API KEY ENV"
    );
    for preset in providers::presets() {
        let base_url = app
            .config
            .providers
            .get(preset.name)
            .and_then(|entry| entry.base_url.clone())
            .unwrap_or_else(|| preset.base_url.to_string());
        let env_var = app
            .config
            .providers
            .get(preset.name)
            .and_then(|entry| entry.api_key_env.clone())
            .unwrap_or_else(|| preset.api_key_env.to_string());
        let key_state = if !preset.requires_key {
            "- (not required)"
        } else if std::env::var(&env_var)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
            || std::env::var("JAI_API_KEY").is_ok()
        {
            "set"
        } else {
            "missing"
        };
        println!(
            "{:<12}  {:<34}  {:<22}  {}",
            preset.name,
            if base_url.is_empty() {
                "(set base_url)"
            } else {
                &base_url
            },
            env_var,
            key_state
        );
    }
    for name in app.config.providers.keys() {
        if providers::preset(name).is_none() {
            println!("{name:<12}  (custom endpoint from configuration)");
        }
    }
}

fn run_config(app: &App, init: bool) -> Result<i32> {
    let config_path = app.paths.config_file();
    if init {
        if config_path.exists() {
            println!("configuration already exists: {}", config_path.display());
        } else {
            let defaults = jai_config::Config::default();
            defaults
                .save(config_path)
                .map_err(|err| Error::config(err.to_string()))?;
            println!("wrote a default configuration to {}", config_path.display());
        }
        return Ok(0);
    }

    println!("state directory: {}", app.paths.root().display());
    println!("config file:     {}", config_path.display());
    println!("database:        {}", app.paths.database_file().display());
    println!("workspace:       {}", app.workspace.display());
    println!("permission mode: {}", app.mode);
    println!("storage backend: {}", app.store.backend());
    println!();
    match std::fs::read_to_string(config_path) {
        Ok(text) => print!("{text}"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!(
                "# {} does not exist yet; defaults are in effect.",
                config_path.display()
            );
            let defaults = jai_config::Config::default();
            let text = defaults
                .to_toml()
                .map_err(|err| Error::config(err.to_string()))?;
            println!("{text}");
        }
        Err(err) => {
            return Err(Error::config(format!(
                "could not read {}: {err}",
                config_path.display()
            )));
        }
    }
    Ok(0)
}

/// Collects doctor results.
#[derive(Debug, Default)]
struct Checks {
    failures: usize,
}

impl Checks {
    fn ok(&self, label: &str, detail: impl std::fmt::Display) {
        println!("ok    {label:<24} {detail}");
    }

    fn warn(&self, label: &str, detail: impl std::fmt::Display) {
        println!("warn  {label:<24} {detail}");
    }

    fn fail(&mut self, label: &str, detail: impl std::fmt::Display) {
        self.failures += 1;
        println!("FAIL  {label:<24} {detail}");
    }
}

fn run_doctor(app: &App) -> Result<i32> {
    let mut checks = Checks::default();

    // State directory and permissions.
    match std::fs::metadata(app.paths.root()) {
        Ok(metadata) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = metadata.permissions().mode() & 0o777;
                if mode == 0o700 {
                    checks.ok(
                        "state directory",
                        format!("{} ({mode:o})", app.paths.root().display()),
                    );
                } else {
                    checks.warn(
                        "state directory",
                        format!("{} is {mode:o}, expected 700", app.paths.root().display()),
                    );
                }
            }
            #[cfg(not(unix))]
            {
                let _ = metadata;
                checks.ok("state directory", app.paths.root().display());
            }
        }
        Err(err) => checks.fail(
            "state directory",
            format!("{}: {err}", app.paths.root().display()),
        ),
    }

    // Configuration.
    match std::fs::read_to_string(app.paths.config_file()) {
        Ok(_) => checks.ok("configuration", app.paths.config_file().display()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            checks.ok("configuration", "using built-in defaults (no config.toml)")
        }
        Err(err) => checks.fail("configuration", err),
    }

    // Database.
    match SqliteStore::open(&app.paths.database_file()) {
        Ok(store) => {
            match store.integrity_check() {
                Ok(()) => checks.ok(
                    "database",
                    format!("{} is healthy", app.paths.database_file().display()),
                ),
                Err(err) => checks.fail("database", err),
            }
            match store.applied_migrations() {
                Ok(versions) => checks.ok("migrations", format!("applied {versions:?}")),
                Err(err) => checks.fail("migrations", err),
            }
            match store.list_sessions() {
                Ok(sessions) => checks.ok("sessions", format!("{} stored", sessions.len())),
                Err(err) => checks.fail("sessions", err),
            }
        }
        Err(err) => checks.fail("database", err),
    }

    // Workspace.
    if app.workspace.is_dir() {
        let probe = app.workspace.join(".jai-write-test");
        match std::fs::write(&probe, b"") {
            Ok(()) => {
                let _ = std::fs::remove_file(&probe);
                checks.ok(
                    "workspace",
                    format!("{} is writable", app.workspace.display()),
                );
            }
            Err(err) => checks.warn("workspace", format!("not writable: {err}")),
        }
    } else {
        checks.fail(
            "workspace",
            format!("{} is not a directory", app.workspace.display()),
        );
    }

    // Providers and keys.
    for preset in providers::presets() {
        if !preset.requires_key {
            continue;
        }
        let env_var = preset.api_key_env;
        let has_key = std::env::var(env_var)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        let has_global = std::env::var("JAI_API_KEY")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false);
        if has_key || has_global {
            checks.ok("provider key", format!("{env_var} set"));
        } else {
            println!("--    provider key             {env_var} not set");
        }
    }

    // Tools.
    println!(
        "--    tools                    filesystem={} shell={} network={}",
        app.config.tools.filesystem.enabled,
        app.config.tools.shell.enabled,
        app.config.tools.network.enabled
    );
    println!("--    permission mode          {}", app.mode);
    println!(
        "--    allowed paths            {:?}",
        app.config.permissions.allowed_paths
    );

    let failures = checks.failures;
    if failures == 0 {
        println!("\nno problems found");
        Ok(0)
    } else {
        println!("\n{failures} problem(s) found");
        Ok(1)
    }
}

fn run_purge(app: &App, yes: bool) -> Result<i32> {
    let store = app.store.as_ref();
    let sessions = store.list_sessions()?;
    if sessions.is_empty() {
        println!("nothing to delete.");
        return Ok(0);
    }
    if store.backend() != "sqlite" {
        println!("persistence is disabled (--no-persist): nothing is stored on disk.");
        return Ok(0);
    }
    if !yes {
        println!(
            "This deletes {} session(s) and their messages from {}.",
            sessions.len(),
            app.paths.database_file().display()
        );
        print!("Type 'yes' to confirm: ");
        std::io::stdout().flush()?;
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line)?;
        if line.trim() != "yes" {
            println!("aborted; nothing was deleted.");
            return Ok(0);
        }
    }
    let removed = store.purge()?;
    println!("deleted {removed} session(s)");
    Ok(0)
}
