//! Built-in OpenAI-compatible provider presets.
//!
//! Only `base_url`, the API-key environment variable, and a few model hints
//! live here. Model lists are hints for completion only: the authoritative
//! list always comes from the provider's `GET /models` (see `jai models`).

/// A ready-made provider definition shipped with jAI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProviderPreset {
    /// Provider name used on the command line and in `config.toml`.
    pub name: &'static str,
    /// Default API root; `/chat/completions` and `/models` are appended.
    pub base_url: &'static str,
    /// Environment variable holding the API key.
    pub api_key_env: &'static str,
    /// Whether an API key is required (local servers need none).
    pub requires_key: bool,
    /// Model used when neither `--model` nor config selects one.
    pub default_model: &'static str,
    /// Non-authoritative model hints for shell completion and docs.
    pub models: &'static [&'static str],
    /// Free-form caveat shown by `jai providers`.
    pub note: &'static str,
}

const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        name: "deepseek",
        base_url: "https://api.deepseek.com",
        api_key_env: "DEEPSEEK_API_KEY",
        requires_key: true,
        default_model: "deepseek-chat",
        models: &["deepseek-chat", "deepseek-reasoner"],
        note: "https://api.deepseek.com/v1 also works for OpenAI SDK compatibility",
    },
    ProviderPreset {
        name: "openai",
        base_url: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
        requires_key: true,
        default_model: "gpt-4o-mini",
        models: &["gpt-4o", "gpt-4o-mini", "gpt-4.1", "o4-mini"],
        note: "authoritative model list: jai models openai",
    },
    ProviderPreset {
        name: "ollama",
        base_url: "http://127.0.0.1:11434/v1",
        api_key_env: "OLLAMA_API_KEY",
        requires_key: false,
        default_model: "llama3.2",
        models: &["llama3.2", "qwen2.5", "mistral"],
        note: "local server; any non-empty API key value is ignored by Ollama",
    },
    ProviderPreset {
        name: "lmstudio",
        base_url: "http://127.0.0.1:1234/v1",
        api_key_env: "LMSTUDIO_API_KEY",
        requires_key: false,
        default_model: "local-model",
        models: &["local-model"],
        note: "LM Studio local server",
    },
    ProviderPreset {
        name: "openrouter",
        base_url: "https://openrouter.ai/api/v1",
        api_key_env: "OPENROUTER_API_KEY",
        requires_key: true,
        default_model: "openai/gpt-4o-mini",
        models: &[],
        note: "model ids are vendor-prefixed (vendor/model)",
    },
    ProviderPreset {
        name: "groq",
        base_url: "https://api.groq.com/openai/v1",
        api_key_env: "GROQ_API_KEY",
        requires_key: true,
        default_model: "llama-3.3-70b-versatile",
        models: &[],
        note: "",
    },
    ProviderPreset {
        name: "moonshot",
        base_url: "https://api.moonshot.cn/v1",
        api_key_env: "MOONSHOT_API_KEY",
        requires_key: true,
        default_model: "moonshot-v1-8k",
        models: &[],
        note: "Kimi / Moonshot; use api.moonshot.ai for the international endpoint",
    },
    ProviderPreset {
        name: "mistral",
        base_url: "https://api.mistral.ai/v1",
        api_key_env: "MISTRAL_API_KEY",
        requires_key: true,
        default_model: "mistral-large-latest",
        models: &[],
        note: "",
    },
    ProviderPreset {
        name: "together",
        base_url: "https://api.together.xyz/v1",
        api_key_env: "TOGETHER_API_KEY",
        requires_key: true,
        default_model: "meta-llama/Llama-3.3-70B-Instruct-Turbo",
        models: &[],
        note: "",
    },
    ProviderPreset {
        name: "xai",
        base_url: "https://api.x.ai/v1",
        api_key_env: "XAI_API_KEY",
        requires_key: true,
        default_model: "grok-2-latest",
        models: &[],
        note: "",
    },
    ProviderPreset {
        name: "anthropic",
        base_url: "https://api.anthropic.com/v1",
        api_key_env: "ANTHROPIC_API_KEY",
        requires_key: true,
        default_model: "claude-3-5-haiku-latest",
        models: &[],
        note: "uses Anthropic's OpenAI-compatibility layer; some fields are unsupported",
    },
    ProviderPreset {
        name: "custom",
        base_url: "",
        api_key_env: "JAI_API_KEY",
        requires_key: false,
        default_model: "",
        models: &[],
        note: "any OpenAI-compatible endpoint; set base_url in config.toml or pass --base-url",
    },
];

/// All built-in presets.
pub fn presets() -> &'static [ProviderPreset] {
    PRESETS
}

/// Look up a preset by name (case-insensitive).
pub fn preset(name: &str) -> Option<&'static ProviderPreset> {
    let name = name.to_ascii_lowercase();
    PRESETS.iter().find(|p| p.name == name)
}

/// A provider definition after merging presets, config, CLI flags and env.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProvider {
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub api_key_env: String,
    pub requires_key: bool,
    pub default_model: Option<String>,
    pub models: Vec<String>,
    pub headers: Vec<(String, String)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preset_lookup_is_case_insensitive() {
        assert_eq!(preset("DeepSeek").unwrap().name, "deepseek");
        assert!(preset("nope").is_none());
    }

    #[test]
    fn every_preset_but_custom_has_a_base_url() {
        for p in presets() {
            if p.name == "custom" {
                assert!(p.base_url.is_empty());
            } else {
                assert!(p.base_url.starts_with("http"), "{}", p.name);
            }
            assert!(!p.api_key_env.is_empty(), "{}", p.name);
        }
    }

    #[test]
    fn local_presets_do_not_require_keys() {
        assert!(!preset("ollama").unwrap().requires_key);
        assert!(!preset("lmstudio").unwrap().requires_key);
        assert!(preset("deepseek").unwrap().requires_key);
    }
}
