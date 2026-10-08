# jAI architecture

## Crate map

| Crate | Responsibility | Key types |
|-------|----------------|-----------|
| `jai-core` | domain layer, no I/O policy | `Message`, `ChatRequest`, `Engine`, `EngineSettings`, `EngineEvent`, `Provider`, `Policy`, `ToolExecutor`, `SessionStore`, `Approver`, `Redactor` |
| `jai-config` | settings, `~/.jai` layout, provider presets | `Config`, `Paths`, `presets()`, `ResolvedProvider` |
| `jai-provider` | OpenAI-compatible HTTP + SSE | `OpenAiCompatProvider`, `SseDecoder`, wire types |
| `jai-db` | SQLite storage and migrations | `SqliteStore`, `MIGRATIONS` |
| `jai-policy` | security decisions | `ToolPolicy`, `PolicyConfig`, `Sandbox`, `check_path_access`, `classify_shell_command` |
| `jai-tools` | tool implementations | `Tools`, `ToolRegistry`, `PathGuard`, `fs`, `shell`, `http` |
| `jai-web` | local web UI | `WebBackend`, `build_router`, `PendingApprovals` |
| `jai-cli` | the binary and all wiring | `args`, `app::App`, `one_shot`, `repl`, `commands`, `web_mode` |

Dependency direction is one-way: `core` ← (`config`, `provider`, `db`,
`policy`) ← `tools` ← front ends (`cli`, `web`). `jai-cli` is the only crate that
knows about every other one; `jai-web` only depends on `jai-core` and receives a
`WebBackend` implementation from the CLI.

## Turn flow

```text
user input
   │
   ▼
Engine::start_turn ──► task ──► TurnHandle { events, cancel }
   │                                  │
   │                                  └─► EngineEvent stream (delta, tool_*,
   │                                      approval_requested, usage, finished)
   ▼
append user message (SessionStore)
   │
   ▼
loop up to agent.max_tool_rounds:
   build ChatRequest (system prompt + stored history + tool schemas)
        │
        ▼
   Provider::chat_stream ──► SSE frames ──► Accumulator ──► AssistantTurn
        │                                                   │
        ▼                                                   ▼
   EngineEvent::Delta                             persist assistant message
        │
        ▼
   tool calls? ── no ──► done
        │ yes
        ▼
   ToolExecutor::describe ──► ToolRequest (risk, summary, paths, cwd)
        │
        ▼
   Policy::evaluate ──► Allow | Confirm | Deny
        │                        │
        │                        └─► Approver::approve (terminal prompt or browser)
        ▼
   ToolExecutor::execute ──► ToolOutcome ──► redact ──► truncate
        │
        ▼
   audit row (status, decision) + tool result message ──► next round
```

Properties this buys us:

* **One implementation of the agent.** Streaming, tool loops, approvals and
  audit apply identically to CLI, web and tests; the test suite drives the same
  `Engine` with scripted providers.
* **Front ends cannot skip the policy.** `ToolExecutor::execute` is only called
  from the engine, after `Policy::evaluate`.
* **Providers are dumb.** They build JSON, stream SSE, and map HTTP errors to
  `jai_core::Error` with an exit code. They never print or persist anything.
* **Cancellation is cooperative.** `TurnHandle::cancel`/`TurnCanceller` set an
  `AtomicBool` that the provider's pump checks between chunks, so Ctrl-C stops
  the request instead of the process.

## Data model

`~/.jai/jai.db`, migrations in `migrations/`:

| Table | Contents |
|-------|----------|
| `sessions` | id, title, provider, model, timestamps |
| `messages` | role, content, `tool_calls` JSON, `tool_call_id`, `tool_name` |
| `tool_calls` | audit row per tool request: name, arguments, mode, risk, status, decision, summary |
| `tool_results` | output of a tool call (separate table, so the audit survives message cleanup) |
| `usage` | token accounting per response |
| `settings` | last used provider/model, local UI defaults |
| `schema_migrations` | applied migration versions |

Details:

* `PRAGMA journal_mode = WAL`, `foreign_keys = ON`, `busy_timeout = 5000`,
  `synchronous = NORMAL`, `trusted_schema = OFF`.
* Deleting a session cascades to its messages, audit rows and usage.
* `SessionStore::reconcile_interrupted` marks tool calls that were still
  `running` at startup as `interrupted` and returns them, which is how the REPL
  warns about a crash without blindly repeating a command.
* `SqliteStore` is `Send + Sync` (`Mutex<Connection>`); calls are short and
  local, and the engine calls them synchronously.

## Configuration resolution

```text
built-in preset  ──►  config.toml  ──►  environment  ──►  CLI flags
     (name,             (base_url,        (JAI_* and      (--base-url,
      base_url,          api_key_env,      provider        --api-key,
      key env,           models,          key vars)        --model)
      default model)     headers)
```

`jai providers` prints the result per provider, `jai config` prints the
effective files, and `jai doctor` re-checks the installation the way a release
would.

## Testing strategy

| Layer | How it is tested |
|-------|------------------|
| domain/streaming | unit tests in `jai-core` (accumulator, redaction, sandbox, time) |
| provider | `crates/jai-provider/tests/mock_provider.rs`: axum mock, raw SSE bytes split at every offset, status → exit-code mapping, cancellation, timeouts |
| storage | `crates/jai-db/tests/file_store.rs`: permissions, migration idempotency, crash recovery, two handles on one file |
| tools | unit tests per tool plus `PathGuard` symlink-escape tests |
| web | `crates/jai-web/tests/server.rs`: token guard, `Host` check, session CRUD, SSE streaming, approval round trip |
| CLI | `crates/jai-cli/tests/cli.rs`: runs the real binary against a mock provider with a temporary `JAI_HOME` |

## Deliberate limitations

* A single jAI process per session is assumed for concurrent writes; WAL allows
  readers, but two writers can still interleave turns.
* Debug logging to `~/.jai/logs` is not implemented (`[persistence]
  store_debug_logs` is accepted but unused).
* No encryption at rest.
* Only OpenAI-compatible HTTP providers (no vendor-specific SDKs).
