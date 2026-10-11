//! Runtime wiring: configuration, storage, provider, tools, policy, engine.
//!
//! Everything a command needs is built exactly once here, so the CLI, the web
//! UI and the tests all assemble the agent the same way.

use crate::args::Cli;
use crate::session::SessionOrigin;
use jai_config::{Config, Paths, ProviderOverrides, ResolvedProvider};
use jai_core::error::{Error, Result};
use jai_core::message::PermissionMode;
use jai_core::policy::Approver;
use jai_core::provider::Provider;
use jai_core::store::{MemoryStore, Session, SessionStore};
use jai_core::{Engine, EngineSettings, ToolExecutor};
use jai_db::SqliteStore;
use jai_policy::{PolicyConfig, ToolPolicy};
use jai_provider::OpenAiCompatProvider;
use jai_tools::{ToolRegistry, Tools, ToolsConfig};
use serde_json::{Map, Value};
use std::path::PathBuf;
use std::sync::Arc;

/// Shared runtime state for one jAI invocation.
pub struct App {
    pub paths: Paths,
    pub config: Config,
    pub store: Arc<dyn SessionStore>,
    pub workspace: PathBuf,
    pub mode: PermissionMode,
    pub system_prompt: Option<String>,
    pub extra: Map<String, Value>,
    pub verbose: bool,
    pub no_tools: bool,
    pub show_thinking: bool,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
}

impl App {
    /// Build the runtime from parsed arguments.
    pub fn from_cli(cli: &Cli) -> Result<Self> {
        let paths = Paths::discover();
        paths.ensure().map_err(|err| {
            Error::config(format!(
                "could not prepare the state directory {}: {err}",
                paths.root().display()
            ))
        })?;

        let mut config = Config::load_or_default(paths.config_file())
            .map_err(|err| Error::config(err.to_string()))?;
        config.apply_env();
        if let Some(bind) = &cli.bind {
            config.web.bind = bind.clone();
        }
        if let Some(port) = cli.port {
            config.web.port = port;
        }

        let workspace = resolve_workspace(cli, &config)?;
        let store: Arc<dyn SessionStore> = if cli.no_persist || !config.persistence.enabled {
            Arc::new(MemoryStore::new())
        } else {
            Arc::new(SqliteStore::open(&paths.database_file())?)
        };
        let mode = cli
            .mode
            .map(PermissionMode::from)
            .unwrap_or(config.agent.permission_mode);
        // Reasoning text is on unless the run asked for quiet output: it is the
        // whole point of models that expose their thinking.
        let show_thinking = !cli.no_thinking;
        let temperature = cli.temperature.or(config.agent.temperature);
        let max_tokens = cli.max_tokens.or(config.agent.max_tokens);
        let system_prompt = cli
            .system
            .clone()
            .or_else(|| config.agent.system_prompt.clone());

        Ok(Self {
            paths,
            config,
            store,
            workspace,
            mode,
            system_prompt,
            extra: parse_extra_json(&cli.extra_json)?,
            verbose: cli.verbose,
            no_tools: cli.no_tools,
            show_thinking,
            temperature,
            max_tokens,
        })
    }

    /// Provider name: flag, config, last used, or an error that lists options.
    pub fn provider_name(&self, cli: &Cli) -> Result<String> {
        if let Some(name) = cli
            .provider
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
        {
            return Ok(name.to_ascii_lowercase());
        }
        if let Some(name) = self.config.defaults.provider.as_deref() {
            return Ok(name.to_ascii_lowercase());
        }
        if let Some(name) = self.setting("last_provider")? {
            return Ok(name);
        }
        Err(Error::config(format!(
            "no provider selected. Pass one (`jai deepseek -a`), set defaults.provider in {}, or export JAI_PROVIDER. Known providers: {}",
            self.paths.config_file().display(),
            self.config.known_provider_names().join(", ")
        )))
    }

    /// Model id: flag, config, last used, or the provider's default.
    pub fn model_name(&self, cli: &Cli, provider: &ResolvedProvider) -> Result<String> {
        if let Some(model) = cli
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            return Ok(model.to_string());
        }
        if let Some(model) = self
            .config
            .defaults
            .model
            .as_deref()
            .filter(|m| !m.trim().is_empty())
        {
            return Ok(model.to_string());
        }
        if let Some(model) = self.setting("last_model")? {
            return Ok(model);
        }
        provider.default_model.clone().ok_or_else(|| {
            Error::config(format!(
                "no model selected for `{}`. Pass --model, set defaults.model, or run `jai models {}`",
                provider.name, provider.name
            ))
        })
    }

    /// Merge presets, config, environment and CLI overrides.
    pub fn resolved_provider(&self, cli: &Cli, name: &str) -> Result<ResolvedProvider> {
        self.resolved_provider_raw(name, cli.base_url.as_deref(), cli.api_key.as_deref())
    }

    /// Override-aware provider resolution, used when switching provider at
    /// runtime (interactive mode).
    pub fn resolved_provider_raw(
        &self,
        name: &str,
        base_url: Option<&str>,
        api_key: Option<&str>,
    ) -> Result<ResolvedProvider> {
        self.config
            .resolve_provider(name, ProviderOverrides { base_url, api_key })
            .map_err(|err| Error::config(err.to_string()))
    }

    /// Build the HTTP provider.
    pub fn provider(&self, resolved: &ResolvedProvider) -> Result<Arc<dyn Provider>> {
        let provider = OpenAiCompatProvider::new(
            resolved.name.clone(),
            resolved.base_url.clone(),
            resolved.api_key.clone(),
        )?
        .with_headers(&resolved.headers)?;
        Ok(Arc::new(provider))
    }

    /// Build the tools, honouring `[tools.*]` config and `--no-tools`.
    pub fn tools(&self) -> Result<Option<Arc<Tools>>> {
        if self.no_tools || !self.config.agent.tools_enabled {
            return Ok(None);
        }
        let config = ToolsConfig::new(self.workspace.clone())
            .with_allowed_paths(self.allowed_paths())
            .with_deny_paths(self.config.permissions.deny_paths.clone())
            .with_tools(
                self.config.tools.filesystem.enabled,
                self.config.tools.shell.enabled,
                self.config.tools.network.enabled,
            )
            .with_max_output_bytes(self.config.permissions.max_tool_output_bytes);
        Ok(Some(Arc::new(Tools::new(config)?)))
    }

    fn allowed_paths(&self) -> Vec<PathBuf> {
        self.config
            .permissions
            .allowed_paths
            .iter()
            .map(|raw| {
                let expanded = jai_policy::expand(PathBuf::from(raw), home_dir().as_deref());
                if expanded.is_absolute() {
                    expanded
                } else {
                    self.workspace.join(expanded)
                }
            })
            .collect()
    }

    /// Policy engine for the given mode.
    pub fn policy(&self, mode: PermissionMode) -> Arc<ToolPolicy> {
        Arc::new(ToolPolicy::new(
            PolicyConfig::new(self.workspace.clone())
                .with_mode(mode)
                .with_allowed_paths(self.allowed_paths())
                .with_deny_paths(self.config.permissions.deny_paths.clone())
                .with_auto_approve_read_only(self.config.agent.auto_approve_read_only)
                .with_tools(
                    self.config.tools.filesystem.enabled,
                    self.config.tools.shell.enabled,
                    self.config.tools.network.enabled,
                ),
        ))
    }

    /// Assemble the agent engine.
    pub fn engine(
        &self,
        provider: Arc<dyn Provider>,
        provider_name: &str,
        model: &str,
        mode: PermissionMode,
        approver: Arc<dyn Approver>,
    ) -> Result<Arc<Engine>> {
        let tools: Option<Arc<dyn ToolExecutor>> = match self.tools()? {
            Some(tools) => Some(Arc::new(ToolRegistry::new(tools))),
            None => None,
        };
        let settings = EngineSettings {
            provider: provider_name.to_string(),
            model: model.to_string(),
            system_prompt: self.system_prompt.clone(),
            max_tool_rounds: self.config.agent.max_tool_rounds,
            tools_enabled: tools.is_some(),
            temperature: self.temperature,
            max_tokens: self.max_tokens,
            extra: self.extra.clone(),
            mode,
            redact: self.config.permissions.redact_secrets,
            store_tool_results: self.config.persistence.store_tool_results,
            store_reasoning: self.config.persistence.store_reasoning,
            context_window: self.context_window(model),
            max_tool_output_bytes: self.config.permissions.max_tool_output_bytes,
        };
        Ok(Arc::new(Engine::new(
            provider,
            Arc::clone(&self.store),
            tools,
            self.policy(mode),
            approver,
            settings,
        )))
    }

    /// Context window used for the `Cxt:` figure: explicit configuration
    /// (`agent.context_window`), then the built-in table for the model id, then
    /// a documented default.
    pub fn context_window(&self, model: &str) -> u64 {
        jai_core::tokens::resolve_context_window(model, self.config.agent.context_window)
    }

    /// Pick the session to work in, reporting *why* it was chosen.
    ///
    /// Order: an explicit `--session`, then `--continue`, then — when the caller
    /// allows it — the last session that was used in this same workspace, so
    /// `cd ~/projects/x && jai -a` picks up where that directory left off.
    pub fn open_session_with_origin(
        &self,
        requested: Option<&str>,
        continue_latest: bool,
        force_new: bool,
        no_resume: bool,
        provider: &str,
        model: &str,
    ) -> Result<(Session, SessionOrigin)> {
        if force_new {
            let previous = self.store.latest_session()?;
            let session = self.store.create_session(provider, model, None)?;
            let origin = match previous {
                Some(previous) => SessionOrigin::ForcedNew {
                    previous: previous.id,
                },
                None => SessionOrigin::New,
            };
            return Ok((session, origin));
        }
        if let Some(id) = requested {
            if id.eq_ignore_ascii_case("latest") {
                return Ok(match self.store.latest_session()? {
                    Some(session) => (session, SessionOrigin::Requested),
                    None => (
                        self.store.create_session(provider, model, None)?,
                        SessionOrigin::New,
                    ),
                });
            }
            let session = self.store.get_session(id)?.ok_or_else(|| {
                Error::config(format!(
                    "unknown session `{id}`; run `jai sessions` to list stored conversations"
                ))
            })?;
            return Ok((session, SessionOrigin::Requested));
        }
        if continue_latest {
            if let Some(session) = self.store.latest_session()? {
                return Ok((session, SessionOrigin::Requested));
            }
        }
        if !no_resume {
            let workspace = self.workspace.display().to_string();
            if let Some(session) = self.store.latest_session_for_workspace(&workspace)? {
                return Ok((session, SessionOrigin::Workspace { workspace }));
            }
        }
        Ok((
            self.store.create_session(provider, model, None)?,
            SessionOrigin::New,
        ))
    }

    /// Persist the provider/model selection as the new default.
    pub fn remember(&self, provider: &str, model: &str) {
        let _ = self.store.set_setting("last_provider", provider);
        let _ = self.store.set_setting("last_model", model);
    }

    pub fn setting(&self, key: &str) -> Result<Option<String>> {
        let value = self.store.setting(key)?;
        Ok(value.filter(|value| !value.trim().is_empty()))
    }
}

fn resolve_workspace(cli: &Cli, config: &Config) -> Result<PathBuf> {
    let candidate = cli
        .workspace
        .clone()
        .or_else(|| config.permissions.workspace.clone().map(PathBuf::from))
        .map(|path| jai_policy::expand(path, home_dir().as_deref()))
        .unwrap_or_else(|| std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")));
    let canonical = candidate.canonicalize().map_err(|err| {
        Error::config(format!(
            "workspace {} is not accessible: {err}",
            candidate.display()
        ))
    })?;
    if !canonical.is_dir() {
        return Err(Error::config(format!(
            "workspace {} is not a directory",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn home_dir() -> Option<PathBuf> {
    ["HOME", "USERPROFILE"]
        .iter()
        .find_map(|name| std::env::var_os(name).filter(|value| !value.is_empty()))
        .map(PathBuf::from)
}

/// Merge `--extra-json` values into one request-body map.
pub fn parse_extra_json(items: &[String]) -> Result<Map<String, Value>> {
    let mut extra = Map::new();
    for item in items {
        let value: Value = serde_json::from_str(item).map_err(|err| {
            Error::config(format!("--extra-json is not valid JSON ({err}): {item}"))
        })?;
        match value {
            Value::Object(map) => extra.extend(map),
            other => {
                return Err(Error::config(format!(
                    "--extra-json must be a JSON object, got `{other}`"
                )));
            }
        }
    }
    Ok(extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extra_json_objects_merge_in_order() {
        let merged = parse_extra_json(&[
            r#"{"thinking":{"type":"enabled"},"top_p":0.9}"#.to_string(),
            r#"{"top_p":0.5}"#.to_string(),
        ])
        .unwrap();
        assert_eq!(merged["thinking"]["type"], "enabled");
        assert_eq!(merged["top_p"], 0.5, "later values win");
    }

    #[test]
    fn extra_json_rejects_non_objects() {
        let error = parse_extra_json(&["[1,2,3]".to_string()]).unwrap_err();
        assert!(error.to_string().contains("JSON object"), "{error}");
        let error = parse_extra_json(&["not json".to_string()]).unwrap_err();
        assert!(error.to_string().contains("not valid JSON"), "{error}");
    }

    #[test]
    fn extra_json_is_empty_by_default() {
        assert!(parse_extra_json(&[]).unwrap().is_empty());
    }
}
