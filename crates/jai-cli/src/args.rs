//! Command-line surface.

use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

const LONG_ABOUT: &str = "\
jAI is a local-first AI agent for OpenAI-compatible providers.

It keeps conversations in a local SQLite database (~/.jai/jai.db), runs tools
(shell, filesystem, ...) strictly inside a workspace directory, and asks for
confirmation before every tool call unless you explicitly opt into yolo mode.";

const AFTER_HELP: &str = "\
EXIT CODES:
  0  success                 4  network failure (DNS, TLS, timeout)
  1  internal/storage/IO      5  provider API error (400/402/422/429/5xx)
  2  usage or configuration   6  tool denied by policy or tool failed
  3  authentication failed    130 cancelled with Ctrl-C

ENVIRONMENT:
  DEEPSEEK_API_KEY, OPENAI_API_KEY, OPENROUTER_API_KEY, GROQ_API_KEY,
  MOONSHOT_API_KEY, MISTRAL_API_KEY, TOGETHER_API_KEY, XAI_API_KEY,
  ANTHROPIC_API_KEY, JAI_API_KEY          provider credentials (env only)
  JAI_HOME      state directory (default ~/.jai)
  JAI_CONFIG    configuration file (default ~/.jai/config.toml)
  JAI_PROVIDER, JAI_MODEL, JAI_MODE, JAI_BASE_URL   overrides
  JAI_DEBUG_LOGS=1 to also persist debug logs

EXAMPLES:
  jai deepseek -m deepseek-chat \"Explain Rust ownership\"
  jai \"Explain Rust ownership\"        first word unknown => prompt wording
  jai -a                             interactive agent (safe mode)
  jai -a --mode yolo                 ... with no confirmation prompts
  jai -a -c                          ... resuming the most recent session
  jai --web                          local web UI on 127.0.0.1:7699
  jai sessions                       list stored conversations
  jai models ollama                  list the models a provider offers
  jai doctor                         check the local installation";

/// jAI command line.
#[derive(Debug, Parser)]
#[command(
    name = "jai",
    version,
    about = "jAI — local-first AI agent with OpenAI-compatible providers",
    long_about = LONG_ABOUT,
    after_help = AFTER_HELP,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    /// Provider name: deepseek, openai, ollama, openrouter, groq, ...
    #[arg(value_name = "PROVIDER")]
    pub provider: Option<String>,

    /// Prompt text. With no prompt and no --agent, jAI reads stdin.
    #[arg(value_name = "PROMPT")]
    pub prompt: Vec<String>,

    /// Model id (defaults to the provider preset or your last choice).
    #[arg(short = 'm', long, value_name = "MODEL")]
    pub model: Option<String>,

    /// Override the provider API root.
    #[arg(long, value_name = "URL")]
    pub base_url: Option<String>,

    /// API key for this run. Prefer the provider's environment variable: a key
    /// passed here is visible in your shell history and process list.
    #[arg(long, value_name = "KEY", hide_env_values = true)]
    pub api_key: Option<String>,

    /// Interactive agent mode (also: --agent).
    #[arg(short = 'a', long)]
    pub agent: bool,

    /// Tool confirmation mode.
    #[arg(long, value_enum, value_name = "MODE")]
    pub mode: Option<Mode>,

    /// Resume the most recent session.
    #[arg(short = 'c', long = "continue")]
    pub continue_session: bool,

    /// Resume a specific session id.
    #[arg(long, value_name = "ID")]
    pub session: Option<String>,

    /// Start a new session even when a previous one exists.
    #[arg(long)]
    pub new: bool,

    /// Do not write anything to the local database.
    #[arg(long)]
    pub no_persist: bool,

    /// Do not offer tools to the model.
    #[arg(long)]
    pub no_tools: bool,

    /// Workspace the agent may read and write (default: current directory).
    #[arg(long, value_name = "DIR")]
    pub workspace: Option<PathBuf>,

    /// System prompt for this run.
    #[arg(long, value_name = "TEXT")]
    pub system: Option<String>,

    /// Start the local web interface.
    #[arg(long)]
    pub web: bool,

    /// Address for --web (default 127.0.0.1).
    #[arg(long, value_name = "ADDR")]
    pub bind: Option<String>,

    /// Port for --web (default 7699).
    #[arg(short = 'p', long, value_name = "PORT")]
    pub port: Option<u16>,

    /// Sampling temperature.
    #[arg(long, value_name = "FLOAT")]
    pub temperature: Option<f32>,

    /// Maximum tokens to generate.
    #[arg(long, value_name = "TOKENS")]
    pub max_tokens: Option<u32>,

    /// Extra JSON merged into the request body (repeatable), e.g.
    /// --extra-json '{"thinking":{"type":"enabled"}}'.
    #[arg(long = "extra-json", value_name = "JSON")]
    pub extra_json: Vec<String>,

    /// Print usage, tool activity and timings.
    #[arg(short = 'v', long)]
    pub verbose: bool,
}

/// Subcommands.
#[derive(Debug, Subcommand)]
pub enum Command {
    /// List stored sessions.
    Sessions {
        /// Print machine-readable JSON.
        #[arg(long)]
        json: bool,
    },

    /// Show, export, rename or delete one session.
    Session {
        /// Session id, or `latest` for the most recent one.
        #[arg(value_name = "ID")]
        id: String,

        /// Delete the session and its messages.
        #[arg(long)]
        delete: bool,

        /// Export the session as Markdown.
        #[arg(long)]
        export: bool,

        /// Write exports to this file instead of stdout.
        #[arg(long, value_name = "FILE")]
        output: Option<PathBuf>,

        /// Set a new title.
        #[arg(long, value_name = "TITLE")]
        rename: Option<String>,

        /// Show the tool audit trail.
        #[arg(long)]
        audit: bool,
    },

    /// List the models a provider advertises.
    Models {
        #[arg(value_name = "PROVIDER")]
        provider: Option<String>,
    },

    /// List the configured providers and whether their keys are set.
    Providers,

    /// Show the configuration file, state directory and effective settings.
    Config {
        /// Write a default configuration file if none exists.
        #[arg(long)]
        init: bool,
    },

    /// Check the local installation (paths, database, keys, workspace).
    Doctor,

    /// Delete every locally stored session.
    Purge {
        /// Skip the interactive confirmation.
        #[arg(long)]
        yes: bool,
    },
}

/// `--mode` values.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Mode {
    /// Ask before every tool call (default).
    Safe,
    /// Never ask, but still enforce the policy engine.
    Yolo,
}

impl From<Mode> for jai_core::PermissionMode {
    fn from(mode: Mode) -> Self {
        match mode {
            Mode::Safe => jai_core::PermissionMode::Safe,
            Mode::Yolo => jai_core::PermissionMode::Yolo,
        }
    }
}

impl Cli {
    /// The prompt text, if one was given on the command line.
    pub fn prompt_text(&self) -> Option<String> {
        if self.prompt.is_empty() {
            None
        } else {
            Some(self.prompt.join(" "))
        }
    }

    /// True when the first positional names a provider we know about.
    pub fn first_word_is_known_provider(&self, known: &[String]) -> bool {
        match self.provider.as_deref() {
            Some(name) => known
                .iter()
                .any(|candidate| candidate.eq_ignore_ascii_case(name)),
            None => false,
        }
    }

    /// `jai Explain Rust ownership` should go to the configured default
    /// provider, not to a provider called `Explain`: an unknown first word is
    /// folded back into the prompt when the rest of the line already looks like
    /// one. Returns whether the arguments were reinterpreted.
    pub fn absorb_unknown_provider(&mut self, known: &[String]) -> bool {
        let Some(name) = self.provider.clone() else {
            return false;
        };
        if self.first_word_is_known_provider(known) || self.prompt.is_empty() {
            return false;
        }
        // With JAI_BASE_URL set, an unlisted name may be a deliberate custom endpoint.
        if std::env::var("JAI_BASE_URL")
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false)
        {
            return false;
        }
        self.prompt.insert(0, name);
        self.provider = None;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Cli {
        Cli::try_parse_from(args).expect("valid arguments")
    }

    #[test]
    fn the_readme_example_parses() {
        let cli = parse(&["jai", "deepseek", "-m", "deepseek-flash"]);
        assert_eq!(cli.provider.as_deref(), Some("deepseek"));
        assert_eq!(cli.model.as_deref(), Some("deepseek-flash"));
        assert!(cli.prompt_text().is_none());

        let cli = parse(&[
            "jai",
            "deepseek",
            "-m",
            "deepseek-chat",
            "Explain Rust ownership",
        ]);
        assert_eq!(cli.prompt_text().as_deref(), Some("Explain Rust ownership"));
    }

    #[test]
    fn the_agent_short_flag_works() {
        assert!(parse(&["jai", "-a"]).agent);
        assert!(parse(&["jai", "--agent"]).agent);
    }

    #[test]
    fn multi_word_prompts_are_joined() {
        // clap puts the first free word in PROVIDER; an unknown word followed by
        // more text is folded back into the prompt.
        let known = vec!["deepseek".to_string(), "ollama".to_string()];

        let mut cli = parse(&["jai", "Explain", "Rust", "ownership"]);
        assert!(cli.absorb_unknown_provider(&known));
        assert!(cli.provider.is_none());
        assert_eq!(cli.prompt_text().as_deref(), Some("Explain Rust ownership"));

        let mut cli = parse(&["jai", "deepseek", "Explain", "ownership"]);
        assert!(!cli.absorb_unknown_provider(&known));
        assert_eq!(cli.provider.as_deref(), Some("deepseek"));
        assert_eq!(cli.prompt_text().as_deref(), Some("Explain ownership"));

        // A lone unknown word stays a provider so the error can name it.
        let mut cli = parse(&["jai", "nope"]);
        assert!(!cli.absorb_unknown_provider(&known));
        assert_eq!(cli.provider.as_deref(), Some("nope"));

        // Provider names are matched case-insensitively.
        let mut cli = parse(&["jai", "DEEPSEEK", "hello"]);
        assert!(!cli.absorb_unknown_provider(&known));
        assert_eq!(cli.provider.as_deref(), Some("DEEPSEEK"));
        assert_eq!(cli.prompt_text().as_deref(), Some("hello"));
    }

    #[test]
    fn modes_parse_and_default_to_none() {
        assert_eq!(
            parse(&["jai", "-a", "--mode", "yolo"]).mode,
            Some(Mode::Yolo)
        );
        assert_eq!(
            parse(&["jai", "-a", "--mode", "safe"]).mode,
            Some(Mode::Safe)
        );
        assert!(parse(&["jai", "-a"]).mode.is_none());
        assert!(Cli::try_parse_from(["jai", "-a", "--mode", "maybe"]).is_err());
    }

    #[test]
    fn subcommands_parse() {
        match parse(&["jai", "sessions"]).command {
            Some(Command::Sessions { json }) => assert!(!json),
            other => panic!("unexpected {other:?}"),
        }
        match parse(&["jai", "session", "abc123", "--export", "--output", "out.md"]).command {
            Some(Command::Session {
                id, export, output, ..
            }) => {
                assert_eq!(id, "abc123");
                assert!(export);
                assert_eq!(output.unwrap().to_string_lossy(), "out.md");
            }
            other => panic!("unexpected {other:?}"),
        }
        match parse(&["jai", "models", "ollama"]).command {
            Some(Command::Models { provider }) => assert_eq!(provider.as_deref(), Some("ollama")),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            parse(&["jai", "doctor"]).command,
            Some(Command::Doctor)
        ));
        assert!(matches!(
            parse(&["jai", "purge", "--yes"]).command,
            Some(Command::Purge { yes: true })
        ));
    }

    #[test]
    fn web_and_extra_json_flags_parse() {
        let cli = parse(&[
            "jai",
            "--web",
            "--bind",
            "127.0.0.1",
            "--port",
            "8080",
            "--extra-json",
            r#"{"thinking":{"type":"enabled"}}"#,
            "--extra-json",
            r#"{"top_p":0.9}"#,
        ]);
        assert!(cli.web);
        assert_eq!(cli.port, Some(8080));
        assert_eq!(cli.extra_json.len(), 2);
    }

    #[test]
    fn session_selection_flags_do_not_conflict() {
        let cli = parse(&["jai", "-a", "--session", "abcd", "--mode", "yolo"]);
        assert_eq!(cli.session.as_deref(), Some("abcd"));
        let cli = parse(&["jai", "-a", "-c"]);
        assert!(cli.continue_session);
    }

    #[test]
    fn prompts_and_subcommands_stay_distinguishable() {
        // "sessions" is a subcommand, not a provider name.
        assert!(matches!(
            parse(&["jai", "sessions"]).command,
            Some(Command::Sessions { .. })
        ));
        // An unknown first word is treated as a provider.
        let cli = parse(&["jai", "myprovider", "hello"]);
        assert_eq!(cli.provider.as_deref(), Some("myprovider"));
        assert_eq!(cli.prompt_text().as_deref(), Some("hello"));
    }
}
