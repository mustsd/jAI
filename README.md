# jAI

A local-first AI agent written in Rust: OpenAI-compatible providers, a SQLite
conversation store, tool execution inside a workspace, and both CLI and web
interaction modes.

```
jai deepseek -m deepseek-chat "Explain Rust ownership"   # one-shot
jai -a                                                   # interactive agent
jai -a --mode yolo                                       # ... no confirmation prompts
jai --web                                                # local web UI
```

Design rule: **safe by default, persistent by default, explicit opt-in for
actions without confirmation.**

## Status

Everything below is implemented, built and covered by tests on macOS (the
platform this repository was developed on, `rustc 1.95.0`):

| Area | State |
|------|-------|
| OpenAI-compatible providers, streaming, `/models` discovery | done |
| Environment-based API keys (never written to disk) | done |
| One-shot CLI, stdin piping, exit-code taxonomy | done |
| Interactive agent (`-a`) with slash commands, Ctrl-C cancellation | done |
| SQLite persistence: sessions, messages, tool audit, usage, migrations | done |
| Crash recovery for interrupted tool calls | done |
| Workspace sandbox, sensitive-path denylist, safe/yolo policy engine | done |
| Tools: filesystem, shell (scrubbed environment), network (opt-in) | done |
| Secret redaction before storage/display/model context | done |
| Web: session browser, streaming chat, tool approval | done |
| Web: switching provider/model from the UI | not implemented (choose them on the command line; the UI lists the available models) |
| Windows/Linux specifics | code paths exist (`cfg`), **not tested on this machine**; `.github/workflows/ci.yml` runs the suite and builds artifacts on Linux and Windows |
| Encryption at rest | not implemented (see `PLAN.md` §8) |
| OpenAI-compatible `/v1/chat/completions` gateway endpoint | not implemented |
| Debug logs under `~/.jai/logs` | flag exists, writing is not implemented |
| Packaging (Homebrew, winget, .msi, .deb) | not implemented; CI builds and uploads per-platform binaries |

## Install

```sh
cargo build --release            # target/release/jai
# or
cargo install --path crates/jai-cli
```

Rust 1.85 or newer. `rusqlite` is built with the bundled SQLite, so no system
library is required.

## Quick start

```sh
export DEEPSEEK_API_KEY=sk-...     # or OPENAI_API_KEY, OPENROUTER_API_KEY, ...

jai deepseek -m deepseek-chat "Explain Rust ownership in three sentences"
jai deepseek -a                    # interactive; /help lists the commands
jai models deepseek                # what the provider actually offers
jai sessions                       # stored conversations
jai -a -c                          # resume the most recent session
jai doctor                         # check paths, database, keys, workspace
```

Local models need no key:

```sh
jai ollama -m llama3.2 -a
jai lmstudio -a
```

## Command line

```
jai [OPTIONS] [PROVIDER] [PROMPT]...
jai <COMMAND>
```

If the first word is not a known provider it is treated as part of the prompt,
so `jai "Explain Rust ownership"` uses your configured default provider.

| Flag | Meaning |
|------|---------|
| `-m, --model <MODEL>` | model id |
| `-a, --agent` | interactive mode |
| `--mode safe\|yolo` | tool confirmation mode (default `safe`) |
| `-c, --continue` | resume the most recent session |
| `--session <ID>` | resume a specific session (`latest` is accepted) |
| `--new` | force a new session |
| `--workspace <DIR>` | directory tools may touch (default: current directory) |
| `--no-tools` | offer no tools to the model |
| `--no-persist` | keep nothing on disk (in-memory session) |
| `--system <TEXT>` `--temperature <F>` `--max-tokens <N>` | request shaping |
| `--extra-json '<JSON>'` | extra request-body fields, e.g. `{"thinking":{"type":"enabled"}}` |
| `--base-url <URL>` `--api-key <KEY>` | override the provider endpoint/key |
| `--web` `--bind <ADDR>` `--port <PORT>` | local web UI (loopback only by default) |
| `-v, --verbose` | also print tool activity and token usage |

Subcommands: `sessions`, `session <ID> [--export] [--audit] [--rename T] [--delete]`,
`models [PROVIDER]`, `providers`, `config [--init]`, `doctor`, `purge [--yes]`.

Inside the interactive agent: `/help /model /provider /mode /system /tools
/sessions /session /new /load /rename /clear /delete /export /tokens /exit`.

Exit codes: `0` success · `1` internal/storage/IO · `2` usage or configuration ·
`3` authentication · `4` network · `5` provider API (400/402/422/429/5xx) ·
`6` tool denied by policy or tool failure · `130` cancelled with Ctrl-C.

## Configuration

Everything lives in `~/.jai` (override with `JAI_HOME`):

```
~/.jai/config.toml    settings
~/.jai/jai.db         sessions, messages, tool audit, usage
~/.jai/history.txt    interactive input history
```

Create a commented starting point with `jai config init` (see
[`config.example.toml`](config.example.toml)); inspect the effective settings
with `jai config`.

API keys are read from the environment only:

```
DEEPSEEK_API_KEY  OPENAI_API_KEY  OPENROUTER_API_KEY  GROQ_API_KEY
MOONSHOT_API_KEY  MISTRAL_API_KEY TOGETHER_API_KEY    XAI_API_KEY
ANTHROPIC_API_KEY JAI_API_KEY (any provider)
```

Also recognised: `JAI_HOME`, `JAI_CONFIG`, `JAI_PROVIDER`, `JAI_MODEL`,
`JAI_MODE`, `JAI_BASE_URL`, `JAI_DEBUG_LOGS`.

## Security defaults

* API keys never leave the environment: they are not written to `config.toml`,
  the database, tool output, or logs.
* `~/.jai` is created `0700`, `jai.db` `0600` (Unix; Windows ACLs are not
  narrowed yet).
* Tools are confined to the workspace; escapes (`..`, absolute paths, symlinks),
  sensitive files (`.env`, `~/.ssh`, `~/.aws`, `*.pem`, `credentials.*`, ...)
  and disabled tool families are denied in **both** safe and yolo mode.
* Safe mode asks before every tool call, showing tool, arguments, risk, working
  directory and affected files; Enter means "no".
* `shell_execute` runs with a scrubbed environment, so a command cannot read
  the API keys of the jAI process.
* Tool output and stored text pass a secret redactor (API-key shapes,
  `Authorization` headers, `KEY=value`, PEM private keys).
* The web UI binds loopback only, requires a per-run token and rejects
  non-loopback `Host` headers.

Details: [`docs/security.md`](docs/security.md).

## Architecture

```
crates/jai-core      domain types, traits (Provider, Policy, ToolExecutor,
                     SessionStore, Approver), the agent engine, redaction
crates/jai-config    config.toml, ~/.jai layout, provider presets
crates/jai-provider  OpenAI-compatible HTTP client + SSE decoding
crates/jai-db        SQLite storage, migrations, audit trail
crates/jai-policy    workspace sandbox, sensitive paths, safe/yolo decisions
crates/jai-tools     filesystem / shell / network tools
crates/jai-web       local web UI (axum) + approval plumbing
crates/jai-cli       the `jai` binary (one-shot, REPL, subcommands, wiring)
```

One agent loop drives every front end; providers never print, the CLI never
builds HTTP, and the policy engine is the only place that decides whether a tool
runs. See [`docs/architecture.md`](docs/architecture.md).

## Documentation

* [`docs/architecture.md`](docs/architecture.md) — crates, engine flow, data model
* [`docs/security.md`](docs/security.md) — threat model, sandbox, secret handling
* [`docs/providers.md`](docs/providers.md) — provider setup and compatibility notes
* [`docs/tools.md`](docs/tools.md) — tool reference and policy behaviour
* [`PLAN.md`](PLAN.md) — the original plan and roadmap

## Development

```sh
cargo test --workspace     # 243 tests: unit, mock-provider, mock-web, end-to-end CLI
cargo clippy --workspace --all-targets
cargo fmt --all
```

The test suite never touches the network or your own `~/.jai`: provider and web
tests run against local mock servers, and CLI tests use temporary `JAI_HOME`
directories.

`.github/workflows/ci.yml` runs `fmt`, `clippy` and the test suite on Linux,
macOS and Windows and uploads release binaries per target. It has not been run
by the author yet (no CI in this repository when the code was written), so treat
it as a starting point.

## At session start (clock in)
1. Read PROGRESS.md for current state
2. Read DECISIONS.md for important decisions
3. Run make check to confirm repo is in consistent state
4. Continue from PROGRESS.md "Next Steps" section

## Before session end (clock out)
1. Update PROGRESS.md
2. Run make check to confirm consistent state
3. Commit all completed work

## License

MIT — see [`LICENSE`](LICENSE).
