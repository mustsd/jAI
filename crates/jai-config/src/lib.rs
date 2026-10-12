//! jAI configuration: `~/.jai/config.toml`, environment overrides, and the
//! provider registry.
//!
//! Precedence, lowest to highest:
//! 1. built-in provider presets ([`providers`])
//! 2. `config.toml` values
//! 3. environment variables (`JAI_PROVIDER`, `JAI_MODEL`, `JAI_MODE`,
//!    `JAI_BASE_URL`, `JAI_API_KEY`)
//! 4. command-line flags
//!
//! API keys are read from the environment only and are never written to disk.

pub mod paths;
pub mod providers;

pub use paths::Paths;
pub use providers::{ProviderPreset, ResolvedProvider, preset, presets};

use jai_core::PermissionMode;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

/// Configuration failures, all of which map to exit code 2 (usage/config).
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("could not read {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("could not write {path}: {source}")]
    Write {
        path: String,
        source: std::io::Error,
    },
    #[error("{path} is not valid TOML: {source}")]
    Parse {
        path: String,
        source: toml::de::Error,
    },
    #[error("could not serialize configuration: {0}")]
    Serialize(String),
    #[error("unknown provider `{0}` (known providers: {1})")]
    UnknownProvider(String, String),
    #[error("provider `{0}` has no base URL; set [providers.{0}].base_url or pass --base-url")]
    MissingBaseUrl(String),
    #[error("no API key for provider `{provider}`: set ${env_var}")]
    MissingApiKey { provider: String, env_var: String },
    #[error("invalid configuration: {0}")]
    Invalid(String),
}

/// Defaults applied when no `--provider`/`--model` is given.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    pub provider: Option<String>,
    pub model: Option<String>,
}

/// Agent behaviour.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AgentConfig {
    /// System prompt prepended to every session.
    pub system_prompt: Option<String>,
    /// Tool permission mode: `safe` (ask) or `yolo` (no prompts).
    pub permission_mode: PermissionMode,
    /// Maximum assistant/tool round trips per user turn.
    ///
    /// A reasoning model that keeps calling tools without converging would
    /// otherwise loop forever: this is the hard stop. When the budget is spent
    /// the turn ends with a notice rather than silently. At least 1 (validated).
    pub max_tool_rounds: u32,
    /// Send tool definitions to the model at all.
    pub tools_enabled: bool,
    /// Auto-approve read-only tools in safe mode (off by default: the plan's
    /// safe mode confirms every tool call).
    pub auto_approve_read_only: bool,
    /// Sampling temperature; omitted when unset.
    pub temperature: Option<f32>,
    /// `max_tokens` sent to the provider; omitted when unset.
    pub max_tokens: Option<u32>,
    /// Model context window for the `Cxt:` figure. When unset, jAI uses its
    /// built-in table for well-known model ids.
    pub context_window: Option<u64>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            permission_mode: PermissionMode::Safe,
            max_tool_rounds: 8,
            tools_enabled: true,
            auto_approve_read_only: false,
            temperature: None,
            max_tokens: None,
            context_window: None,
        }
    }
}

/// What gets written to the local database.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PersistenceConfig {
    pub enabled: bool,
    pub store_tool_results: bool,
    /// Persist the model's reasoning text. It is never replayed to the provider,
    /// so storing it only affects what you can read back later.
    pub store_reasoning: bool,
    pub store_debug_logs: bool,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            store_tool_results: true,
            store_reasoning: true,
            store_debug_logs: false,
        }
    }
}

/// Filesystem and secret-handling policy.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct PermissionsConfig {
    /// Extra directories tools may touch, beyond the workspace root.
    pub allowed_paths: Vec<String>,
    /// Extra glob-ish deny patterns, on top of the built-in sensitive list.
    pub deny_paths: Vec<String>,
    /// Root the agent is allowed to work in; defaults to the process cwd.
    pub workspace: Option<String>,
    /// Redact secrets from tool output, logs and persisted text.
    pub redact_secrets: bool,
    /// Per-tool-result byte cap kept in context and in the database.
    pub max_tool_output_bytes: usize,
}

impl Default for PermissionsConfig {
    fn default() -> Self {
        Self {
            allowed_paths: Vec::new(),
            deny_paths: Vec::new(),
            workspace: None,
            redact_secrets: true,
            max_tool_output_bytes: 32 * 1024,
        }
    }
}

/// Per-tool enable flags. Filesystem and shell tools are on; network tools are
/// off until the user opts in.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolsConfig {
    pub filesystem: ToolToggle,
    pub shell: ToolToggle,
    pub network: ToolToggle,
}

impl Default for ToolsConfig {
    fn default() -> Self {
        Self {
            filesystem: ToolToggle::enabled(),
            shell: ToolToggle::enabled(),
            network: ToolToggle::disabled(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ToolToggle {
    pub enabled: bool,
}

impl Default for ToolToggle {
    fn default() -> Self {
        Self::disabled()
    }
}

impl ToolToggle {
    pub fn enabled() -> Self {
        Self { enabled: true }
    }

    pub fn disabled() -> Self {
        Self { enabled: false }
    }
}

/// Local web mode settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct WebConfig {
    pub bind: String,
    pub port: u16,
    /// Refuse to bind anything but loopback addresses.
    pub local_only: bool,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            bind: "127.0.0.1".to_string(),
            port: 7699,
            local_only: true,
        }
    }
}

/// A provider entry in `config.toml`; overrides the built-in preset.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub default_model: Option<String>,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
}

/// The whole `config.toml`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub defaults: Defaults,
    pub agent: AgentConfig,
    pub persistence: PersistenceConfig,
    pub permissions: PermissionsConfig,
    pub tools: ToolsConfig,
    pub web: WebConfig,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub providers: BTreeMap<String, ProviderConfig>,
}

/// Per-invocation overrides, usually from CLI flags.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProviderOverrides<'a> {
    pub base_url: Option<&'a str>,
    pub api_key: Option<&'a str>,
}

impl Config {
    /// Load `path`, returning defaults when the file does not exist.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::from_toml(&text, path),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(ConfigError::Read {
                path: path.display().to_string(),
                source,
            }),
        }
    }

    /// Parse configuration text; `path` is only used for error messages.
    pub fn from_toml(text: &str, path: &Path) -> Result<Self, ConfigError> {
        let config: Config = toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        config.validate()?;
        Ok(config)
    }

    /// Serialize to TOML (used by `jai config init`).
    pub fn to_toml(&self) -> Result<String, ConfigError> {
        toml::to_string_pretty(self).map_err(|e| ConfigError::Serialize(e.to_string()))
    }

    /// Write the configuration to `path` with owner-only permissions.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            paths::ensure_private_dir(parent).map_err(|source| ConfigError::Write {
                path: parent.display().to_string(),
                source,
            })?;
        }
        let text = self.to_toml()?;
        std::fs::write(path, text).map_err(|source| ConfigError::Write {
            path: path.display().to_string(),
            source,
        })?;
        paths::restrict_file_to_owner(path).map_err(|source| ConfigError::Write {
            path: path.display().to_string(),
            source,
        })?;
        Ok(())
    }

    /// Apply environment overrides in place.
    pub fn apply_env(&mut self) {
        if let Some(value) = env_non_empty("JAI_PROVIDER") {
            self.defaults.provider = Some(value);
        }
        if let Some(value) = env_non_empty("JAI_MODEL") {
            self.defaults.model = Some(value);
        }
        if let Some(value) = env_non_empty("JAI_MODE") {
            if let Ok(mode) = value.parse::<PermissionMode>() {
                self.agent.permission_mode = mode;
            }
        }
        if env_non_empty("JAI_DEBUG_LOGS").is_some() {
            self.persistence.store_debug_logs = true;
        }
    }

    fn validate(&self) -> Result<(), ConfigError> {
        if self.agent.max_tool_rounds == 0 {
            return Err(ConfigError::Invalid(
                "agent.max_tool_rounds must be at least 1".to_string(),
            ));
        }
        if let Some(window) = self.agent.context_window {
            if window < 1_000 {
                return Err(ConfigError::Invalid(
                    "agent.context_window must be at least 1000 tokens".to_string(),
                ));
            }
        }
        if self.permissions.max_tool_output_bytes == 0 {
            return Err(ConfigError::Invalid(
                "permissions.max_tool_output_bytes must be greater than 0".to_string(),
            ));
        }
        for (name, provider) in &self.providers {
            if let Some(url) = &provider.base_url {
                if !url.starts_with("http://") && !url.starts_with("https://") {
                    return Err(ConfigError::Invalid(format!(
                        "providers.{name}.base_url must start with http:// or https://"
                    )));
                }
            }
        }
        if !self.web.bind.parse::<std::net::IpAddr>().is_ok_and(|ip| {
            if self.web.local_only {
                ip.is_loopback()
            } else {
                true
            }
        }) {
            return Err(ConfigError::Invalid(format!(
                "web.bind `{}` is not a loopback address (local_only is on by default)",
                self.web.bind
            )));
        }
        Ok(())
    }

    /// Tool permission mode after config and env resolution.
    pub fn permission_mode(&self) -> PermissionMode {
        self.agent.permission_mode
    }

    /// Merge preset + config + env/flags into a concrete provider.
    pub fn resolve_provider(
        &self,
        name: &str,
        overrides: ProviderOverrides<'_>,
    ) -> Result<ResolvedProvider, ConfigError> {
        let name = name.trim().to_ascii_lowercase();
        let preset = providers::preset(&name);
        let entry = self.providers.get(&name);

        if preset.is_none() && entry.is_none() {
            let known = self.known_provider_names().join(", ");
            return Err(ConfigError::UnknownProvider(name, known));
        }

        let base_url = overrides
            .base_url
            .map(str::to_string)
            .or_else(|| env_non_empty("JAI_BASE_URL"))
            .or_else(|| entry.and_then(|e| e.base_url.clone()))
            .or_else(|| preset.map(|p| p.base_url.to_string()))
            .filter(|url| !url.is_empty())
            .ok_or_else(|| ConfigError::MissingBaseUrl(name.clone()))?;

        let api_key_env = entry
            .and_then(|e| e.api_key_env.clone())
            .or_else(|| preset.map(|p| p.api_key_env.to_string()))
            .unwrap_or_else(|| format!("{}_API_KEY", name.to_ascii_uppercase()));

        let requires_key = preset.map(|p| p.requires_key).unwrap_or(true);
        let api_key = overrides
            .api_key
            .map(str::to_string)
            .or_else(|| env_non_empty(&api_key_env))
            .or_else(|| env_non_empty("JAI_API_KEY"));
        if api_key.is_none() && requires_key {
            return Err(ConfigError::MissingApiKey {
                provider: name,
                env_var: api_key_env,
            });
        }

        let mut models: Vec<String> = entry.map(|e| e.models.clone()).unwrap_or_default();
        if models.is_empty() {
            if let Some(preset) = preset {
                models = preset.models.iter().map(|m| m.to_string()).collect();
            }
        }
        let default_model = entry
            .and_then(|e| e.default_model.clone())
            .or_else(|| preset.map(|p| p.default_model.to_string()))
            .filter(|m| !m.is_empty());

        Ok(ResolvedProvider {
            name,
            base_url,
            api_key,
            api_key_env,
            requires_key,
            default_model,
            models,
            headers: entry
                .map(|e| {
                    e.headers
                        .iter()
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                })
                .unwrap_or_default(),
        })
    }

    /// Provider names usable right now: presets plus config-defined ones.
    pub fn known_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = providers::presets()
            .iter()
            .map(|p| p.name.to_string())
            .collect();
        for name in self.providers.keys() {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }
}

fn env_non_empty(name: &str) -> Option<String> {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => Some(value),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Config {
        Config::from_toml(text, Path::new("config.toml")).expect("valid config")
    }

    #[test]
    fn defaults_are_safe_by_default() {
        let config = Config::default();
        assert_eq!(config.agent.permission_mode, PermissionMode::Safe);
        assert!(config.persistence.enabled);
        assert!(config.persistence.store_tool_results);
        assert!(!config.persistence.store_debug_logs);
        assert!(config.permissions.redact_secrets);
        assert!(config.tools.filesystem.enabled);
        assert!(config.tools.shell.enabled);
        assert!(!config.tools.network.enabled, "network tools are opt-in");
        assert!(config.web.local_only);
        assert_eq!(config.web.bind, "127.0.0.1");
    }

    #[test]
    fn round_trips_through_toml() {
        let text = r#"
[defaults]
provider = "deepseek"
model = "deepseek-chat"

[agent]
permission_mode = "yolo"
max_tool_rounds = 3

[permissions]
allowed_paths = ["~/projects"]

[providers.deepseek]
base_url = "https://example.test/v1"
"#;
        let config = parse(text);
        assert_eq!(config.defaults.provider.as_deref(), Some("deepseek"));
        assert_eq!(config.agent.permission_mode, PermissionMode::Yolo);
        assert_eq!(config.agent.max_tool_rounds, 3);
        assert_eq!(config.permissions.allowed_paths, vec!["~/projects"]);
        let serialized = config.to_toml().unwrap();
        assert_eq!(parse(&serialized), config);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = Config::from_toml("[agent]\nnope = 1\n", Path::new("config.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Parse { .. }), "{err:?}");
    }

    #[test]
    fn non_loopback_bind_is_rejected_while_local_only() {
        let err =
            Config::from_toml("[web]\nbind = \"0.0.0.0\"\n", Path::new("config.toml")).unwrap_err();
        assert!(err.to_string().contains("loopback"), "{err}");
        let ok = parse("[web]\nbind = \"0.0.0.0\"\nlocal_only = false\n");
        assert_eq!(ok.web.bind, "0.0.0.0");
    }

    #[test]
    fn resolve_provider_merges_config_over_preset() {
        let config = parse("[providers.deepseek]\nbase_url = \"https://proxy.test/v1\"\n");
        let resolved = config
            .resolve_provider(
                "deepseek",
                ProviderOverrides {
                    base_url: None,
                    api_key: Some("test-key"),
                },
            )
            .unwrap();
        assert_eq!(resolved.base_url, "https://proxy.test/v1");
        assert_eq!(resolved.api_key_env, "DEEPSEEK_API_KEY");
        assert_eq!(resolved.name, "deepseek");
    }

    #[test]
    fn resolve_provider_accepts_custom_endpoint_from_config() {
        let config = parse(
            "[providers.acme]\nbase_url = \"http://127.0.0.1:9000/v1\"\napi_key_env = \"ACME_KEY\"\ndefault_model = \"acme-1\"\n",
        );
        let resolved = config
            .resolve_provider(
                "acme",
                ProviderOverrides {
                    base_url: None,
                    api_key: Some("test-key"),
                },
            )
            .unwrap();
        assert_eq!(resolved.base_url, "http://127.0.0.1:9000/v1");
        assert_eq!(resolved.api_key_env, "ACME_KEY");
        assert_eq!(resolved.default_model.as_deref(), Some("acme-1"));
    }

    #[test]
    fn local_providers_work_without_keys() {
        let resolved = Config::default()
            .resolve_provider("ollama", ProviderOverrides::default())
            .unwrap();
        assert!(resolved.api_key.is_none());
        assert!(!resolved.requires_key);
    }

    #[test]
    fn missing_key_and_unknown_provider_are_clear() {
        let config = Config::default();
        // The real environment may already define the key; only assert on the
        // unknown-provider path plus the error text of the missing-key path.
        let err = config
            .resolve_provider("does-not-exist", ProviderOverrides::default())
            .unwrap_err();
        assert!(matches!(err, ConfigError::UnknownProvider(..)), "{err:?}");
        match config.resolve_provider(
            "deepseek",
            ProviderOverrides {
                base_url: None,
                api_key: None,
            },
        ) {
            Ok(resolved) => assert!(resolved.api_key.is_some()),
            Err(err) => assert!(err.to_string().contains("DEEPSEEK_API_KEY"), "{err}"),
        }
    }

    #[test]
    fn cli_overrides_win() {
        let resolved = Config::default()
            .resolve_provider(
                "custom",
                ProviderOverrides {
                    base_url: Some("http://127.0.0.1:8080/v1"),
                    api_key: Some("test-key"),
                },
            )
            .unwrap();
        assert_eq!(resolved.base_url, "http://127.0.0.1:8080/v1");
        assert_eq!(resolved.api_key.as_deref(), Some("test-key"));
    }

    #[test]
    fn custom_provider_without_base_url_is_rejected() {
        let err = Config::default()
            .resolve_provider("custom", ProviderOverrides::default())
            .unwrap_err();
        assert!(matches!(err, ConfigError::MissingBaseUrl(_)), "{err:?}");
    }
}
