# jAI — Expanded Development Plan

## 1. Core goals

jAI should become a local-first AI agent with:

```text
jAI
├── Multi-provider / multi-model
├── OpenAI-compatible APIs
├── Environment-based API keys
├── One-shot CLI
├── Interactive CLI agent
├── Web interface
├── Persistent sessions
├── Persistent messages
├── Tool execution
├── Safe / yolo execution modes
├── Strong local data protection
└── macOS / Linux / Windows
```

The design principle should be:

> **Safe by default. Persistent by default. Explicit opt-in for actions without confirmation.**

---

# 2. Persistence with SQLite

Use SQLite as jAI's local state database.

A good directory structure is:

```text
~/.jai/
├── config.toml
├── jai.db
├── logs/
├── sessions/
└── cache/
```

For Windows, a native equivalent can be supported, but keeping a `.jai` directory under the user's profile is also a reasonable simple first implementation.

I would make the database:

```text
~/.jai/jai.db
```

and never place it inside the source/project directory by default.

---

# 3. What should be stored

At minimum:

```text
sessions
messages
tool_calls
tool_results
provider/model information
usage information
timestamps
```

A practical schema:

```sql
CREATE TABLE sessions (
    id              TEXT PRIMARY KEY,
    title           TEXT,
    provider        TEXT NOT NULL,
    model           TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    updated_at      INTEGER NOT NULL
);

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    role            TEXT NOT NULL,
    content         TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES sessions(id)
);

CREATE TABLE tool_calls (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id      TEXT NOT NULL,
    message_id      INTEGER,
    tool_name       TEXT NOT NULL,
    arguments       TEXT,
    mode            TEXT NOT NULL,
    status          TEXT NOT NULL,
    created_at      INTEGER NOT NULL,
    FOREIGN KEY(session_id) REFERENCES sessions(id)
);

CREATE TABLE tool_results (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    tool_call_id    INTEGER NOT NULL,
    result          TEXT,
    created_at      INTEGER NOT NULL,
    FOREIGN KEY(tool_call_id) REFERENCES tool_calls(id)
);
```

I'd also add indexes on `session_id` and timestamps.

---

# 4. Sessions become a first-class concept

Instead of treating every CLI invocation as unrelated, jAI should have sessions.

For example:

```bash
jai --agent
```

Needs user choose provider and models at first time, and loads the last used if exists. 


Then it could create (for example):

```text
Session: 20261008-001
Provider: deepseek
Model: deepseek-flash
```

Then:

```text
You> Explain Rust ownership
jAI> ...

You> Give me an example
jAI> ...
```

gets saved automatically.

Later:

```bash
jai sessions
```

could show:

```text
ID                 Title                    Updated
────────────────────────────────────────────────────
a83f...             Rust ownership           2 min ago
b712...             Project analysis         yesterday
d921...             Python debugging         Oct 6
```

and:

```bash
jai session a83f...
```

reopens it.

---

# 5. Useful persistence commands

I'd plan for:

```text
/sessions
/session
/new
/load <id>
/rename <name>
/clear
/delete
/export
```

CLI equivalents could eventually be:

```bash
jai sessions
jai session <id>
jai session delete <id>
jai session export <id>
```

A particularly important command is:

```bash
jai session delete <id>
```

because users need an explicit way to remove sensitive conversation history.

Also useful:

```bash
jai purge
```

to remove all locally stored sessions after an explicit confirmation.

---

# 6. Data-safety model

Because jAI may contain private prompts, source code, documents, credentials, business information, and tool output, I would treat the SQLite database as **sensitive data**.

The database should be considered equivalent to a user's private files.

### Directory protection

On Unix:

```text
~/.jai          → owner-only
config.toml     → owner-only
jai.db          → owner-only
```

Target permissions:

```text
directory: 0700
files:     0600
```

On Windows, use user-only filesystem ACLs rather than relying on Unix permission semantics.

jAI should verify or establish these protections when it creates the directory.

---

# 7. Do not store API keys in SQLite

Your original environment-variable approach should remain.

For example:

```bash
DEEPSEEK_API_KEY
OPENAI_API_KEY
ANTHROPIC_API_KEY
```

The database should contain:

```text
provider = deepseek
model = deepseek-flash
```

but never:

```text
api_key = sk-...
```

Likewise, **never write API keys to logs**.

---

# 8. Consider encryption

No encryption now. 

---

# 9. Tool execution modes

Your two modes are a good foundation:

```text
safe
yolo
```

with:

```text
safe = confirmation required
yolo = no confirmation
```

I would make **safe the default**.

For example:

```bash
jai --agent
```

means:

```text
tool mode: safe
```

while:

```bash
jai --agent --mode yolo
```

means:

```text
tool mode: yolo
```

---

# 10. Safe mode

In safe mode, the model can request a tool, but jAI does not automatically execute it. 

Example:

```text
You> Find all Rust files in this project.

jAI wants to execute:

filesystem_list
path: ./src

Allow? [y/N]
```

For shell commands:

```text
jAI wants to execute:

cargo test

Working directory:
~/projects/jai

Allow? [y/N]
```

For destructive operations:

```text
jAI wants to execute:

rm target/debug/app

This operation may delete data.

Allow? [y/N]
```

The confirmation UI should clearly display:

```text
Tool
Arguments
Working directory
Files affected
Risk level
```

---

# 11. yolo mode

`yolo` means:

```text
no interactive confirmation
```

Example:

```bash
jai --agent --permission yolo
```

Then:

```text
model
  ↓
tool request
  ↓
policy check
  ↓
execute
  ↓
result
  ↓
model
```

However, I would **not equate “no confirmation” with “no safety controls.”**

That's an important distinction.

yolo can mean:

> "Do not ask the user for confirmation."

It does not have to mean:

> "Anything the model requests is automatically allowed."

For example, you may still want hard safeguards against:

```text
reading private credential stores
exposing API keys
accessing another user's files
writing outside permitted directories
dangerous system modifications
```

That gives you:

```text
safe
    confirmation + policy

yolo
    no confirmation + policy
```

rather than:

```text
yolo
    execute absolutely anything
```

---

# 12. Tool policy engine

I would introduce a dedicated policy layer:

```text
                   ┌─────────────┐
                   │ AI Model    │
                   └──────┬──────┘
                          │
                     Tool Request
                          │
                   ┌──────▼──────┐
                   │ Tool Policy │
                   └──────┬──────┘
                          │
                ┌─────────┴─────────┐
                │                   │
             safe                yolo
                │                   │
          confirmation        auto execute
                │                   │
                └─────────┬─────────┘
                          │
                    Tool Executor
```

This is much safer than putting confirmation logic directly inside individual tools.

---

# 13. Tool risk levels

Give every tool metadata describing its risk.

For example:

```rust
pub enum RiskLevel {
    ReadOnly,
    Write,
    Execute,
    Destructive,
    Network,
}
```

A tool might declare:

```text
filesystem_read
    ReadOnly

filesystem_write
    Write

shell_execute
    Execute

filesystem_delete
    Destructive

http.request
    Network
```

Then the policy engine decides what to do.

---

# 14. Tool permissions

A better long-term configuration could look like:

```toml
[agent]
tool_mode = "safe"

[tools.filesystem]
enabled = true

[tools.shell]
enabled = true

[tools.network]
enabled = false
```

Later:

```toml
[permissions]
allowed_paths = [
    "~/projects",
    "~/Documents/work"
]
```

This would allow jAI to operate inside selected directories without granting it your entire machine.

---

# 15. Workspace sandboxing

This is one of the strongest safety improvements I'd make.

When jAI is working on a project:

```bash
cd ~/projects/myapp
jai --agent
```

jAI could treat:

```text
~/projects/myapp
```

as its workspace.

Then tools can be restricted to:

```text
workspace/
```

rather than:

```text
/
```

For example:

```text
filesystem_read
    ✓ ./src/main.rs
    ✓ ./Cargo.toml

    ✗ ~/.ssh/id_ed25519
    ✗ ~/.aws/credentials
    ✗ /etc/passwd
```

This dramatically reduces accidental exposure.

---

# 16. Sensitive-file protection

I would add a denylist for obviously sensitive files and directories.

Examples:

```text
~/.ssh
~/.aws
~/.config/gcloud
.env
.env.*
*.pem
*.key
credentials.*
secrets.*
```

This should not necessarily be an absolute permanent denylist—advanced users may need access—but it should trigger the strongest protection and/or require explicit policy changes.

---

# 17. Never expose secrets to the model unintentionally

An especially important rule:

```text
Tool → secret → model
```

should be treated as a security boundary.

For example, if a shell command outputs:

```text
OPENAI_API_KEY=...
```

jAI should have the ability to redact it before putting the output into:

```text
messages
```

or sending it back to the provider.

A pipeline like this is useful:

```text
Tool output
    ↓
Secret detector / redactor
    ↓
Persistence
    ↓
Model context
```

This is especially important for:

```text
environment variables
shell output
configuration files
HTTP responses
logs
```

---

# 18. Persistence should be policy-aware

Not everything necessarily needs to be stored forever.

I'd separate:

```text
Conversation data
Tool execution data
Operational logs
Secrets
```

and establish different rules.

For example:

```text
Messages
    persistent

Tool calls
    persistent

Tool results
    persistent, configurable

Debug logs
    optional / rotating

API keys
    never persistent in jAI DB

Authentication tokens
    never persistent in jAI DB
```

A useful configuration could eventually be:

```toml
[persistence]
enabled = true
store_tool_results = true
store_debug_logs = false
```

---

# 19. Audit trail

Because jAI can execute local actions, SQLite should also provide a lightweight audit history.

For example:

```text
2026-10-08 12:35
session=a83f...
tool=shell_execute
mode=safe
command="cargo test"
decision=approved
```

or:

```text
tool=filesystem_write
path=src/main.rs
decision=approved
```

This lets the user answer:

> What did jAI actually do?

without needing verbose debug logging.

---

# 20. Separate audit information from model conversation

This is worth doing explicitly.

Don't make tool execution records just another assistant message.

Instead:

```text
Conversation
├── user
├── assistant
├── user
└── assistant

Execution history
├── tool request
├── policy decision
├── execution result
└── execution status
```

Then the UI can show:

```text
Assistant
I will update the Cargo configuration.

Tool
filesystem_write
src/Cargo.toml

Approved ✓
```

This makes the system easier to audit and debug.

---

# 21. Database architecture

I'd organize the persistence layer independently:

```text
jai-core
    Agent
    Message
    Tool
    Provider

jai-db
    Database
    SessionRepository
    MessageRepository
    ToolRepository
    MigrationManager
```

Conceptually:

```rust
pub trait SessionRepository {
    fn create(...);
    fn load(...);
    fn list(...);
    fn delete(...);
}

pub trait MessageRepository {
    fn append(...);
    fn list(...);
}
```

This prevents SQLite details from leaking throughout the agent code.

---

# 22. Database reliability

SQLite should be initialized automatically:

```text
jai
 ↓
create ~/.jai
 ↓
open jai.db
 ↓
run migrations
 ↓
start agent
```

Use migrations rather than changing tables manually.

For example:

```text
migrations/
├── 001_initial.sql
├── 002_add_tool_calls.sql
├── 003_add_usage.sql
└── ...
```

Also consider:

```text
WAL mode
foreign keys
busy timeout
transactions
integrity checks
```

A crash should not destroy a conversation.

---

# 23. Session recovery

One nice property of SQLite persistence is crash recovery.

Suppose:

```text
You> Analyze this project.
jAI> ...
```

and the process crashes during a tool execution.

On restart:

```bash
jai --agent
```

jAI should be able to detect an unfinished operation:

```text
Previous session found.

Last operation:
shell_execute
status: interrupted
```

Then let the user decide whether to resume.

This is much safer than blindly repeating a command.

---

# 24. Updated architecture

With persistence and security included, the overall architecture becomes:

```text
                         ┌─────────────────────┐
                         │        jAI          │
                         └──────────┬──────────┘
                                    │
                     ┌──────────────┼──────────────┐
                     │              │              │
                    CLI            Web           Future
                     │              │
                     └───────┬──────┴──────────────┘
                             │
                       ┌─────▼─────┐
                       │   Agent   │
                       └─────┬─────┘
                             │
              ┌──────────────┼──────────────┐
              │              │              │
          Context         Tool Policy     Session
              │              │              │
              │       ┌──────┴──────┐       │
              │       │             │       │
              │      safe          yolo     │
              │       │             │       │
              │   confirmation   automatic  │
              │       │             │       │
              └───────┴─────────────┴───────┘
                             │
                      ┌──────▼──────┐
                      │   Tool     │
                      │  Executor  │
                      └──────┬──────┘
                             │
                  ┌──────────┼──────────┐
                  │          │          │
             filesystem    shell     network
                  │          │          │
                  └──────────┼──────────┘
                             │
                      ┌──────▼──────┐
                      │   SQLite   │
                      │ ~/.jai/... │
                      └─────────────┘
                             │
                      persistence
                      + audit data
```

Provider architecture remains underneath the agent:

```text
Agent
  │
  ▼
Provider Interface
  │
  ├── OpenAI-compatible
  ├── DeepSeek
  ├── OpenAI
  ├── OpenRouter
  └── Custom endpoint
```

---

# 25. Revised project structure

I'd now structure the project approximately like this:

```text
jai/
├── Cargo.toml
├── README.md
│
├── crates/
│   ├── jai-cli/
│   ├── jai-core/
│   ├── jai-provider/
│   ├── jai-db/
│   ├── jai-tools/
│   ├── jai-policy/
│   ├── jai-config/
│   └── jai-web/
│
├── migrations/
│   ├── 001_initial.sql
│   ├── 002_tool_calls.sql
│   └── 003_audit.sql
│
├── web/
│
├── tests/
│   ├── provider/
│   ├── agent/
│   ├── database/
│   ├── policy/
│   └── tools/
│
└── docs/
    ├── architecture.md
    ├── security.md
    ├── providers.md
    └── tools.md
```

The important new crate is:

```text
jai-policy
```

because security policy should not be mixed into the tools themselves.

---

# 26. Revised development phases

### Phase 0 — Security architecture

Before building many features:

```text
✓ threat model
✓ data classification
✓ filesystem layout
✓ SQLite strategy
✓ secret-handling rules
✓ safe mode definition
✓ yolo mode definition
✓ tool policy architecture
```

This phase prevents expensive redesign later.

### Phase 1 — Basic CLI + provider

```text
✓ Rust CLI
✓ OpenAI-compatible client
✓ environment API keys
✓ provider/model selection
✓ one-shot prompts
```

Example:

```bash
jai deepseek -m deepseek-flash "Hello"
```

### Phase 2 — SQLite persistence

```text
✓ ~/.jai
✓ SQLite
✓ sessions
✓ messages
✓ migrations
✓ automatic session saving
✓ resume sessions
✓ delete/export
```

### Phase 3 — Interactive agent

```text
✓ --agent
✓ conversation history
✓ streaming
✓ slash commands
✓ session management
```

### Phase 4 — Tools + safe policy

```text
✓ tool abstraction
✓ tool registry
✓ policy engine
✓ safe mode
✓ confirmation UI
✓ audit trail
✓ workspace restrictions
```

### Phase 5 — yolo

```text
✓ --mode yolo
✓ no confirmation prompts
✓ same policy engine
✓ explicit user opt-in
✓ strong audit trail
```

### Phase 6 — Multiple providers

```text
✓ provider configuration
✓ multiple endpoints
✓ model management
✓ generic OpenAI-compatible providers
```

### Phase 7 — Web UI

```text
✓ local server
✓ browser interface
✓ session browser
✓ provider/model selector
✓ streaming
✓ tool approval UI
```

### Phase 8 — Hardened release

```text
✓ database protection
✓ optional/enforced encryption at rest
✓ secret redaction
✓ filesystem permission checks
✓ security tests
✓ crash recovery
✓ cross-platform packaging
✓ macOS
✓ Linux
✓ Windows
```

---

# 27. Recommended initial commands

I would make these the core user experience:

```bash
# One-shot
jai deepseek -m deepseek-flash "Explain Rust ownership"

# Interactive
jai deepseek -m deepseek-flash --agent

# Interactive, explicitly confirmation-free
jai deepseek -m deepseek-flash --agent --mode yolo

# Use defaults
jai --agent

# Session management
jai sessions
jai session <id>

# Start web UI
jai --web
```

And inside the agent:

```text
/help
/model
/provider
/session
/sessions
/new
/clear
/delete
/exit
```

---

## 28. The security rule I'd make non-negotiable

For jAI, I'd establish this early:

```text
                 DEFAULT
                    │
                    ▼
              ┌───────────┐
              │   safe    │
              └─────┬─────┘
                    │
             tool requested
                    │
                    ▼
              policy check
                    │
                    ▼
              user confirms
                    │
                    ▼
                 execute
```

And:

```text
                 EXPLICIT OPT-IN
                       │
                       ▼
                  ┌─────────┐
                  │  yolo   │
                  └────┬────┘
                       │
                policy check
                       │
                       ▼
                    execute
```

So **safe is the default, yolo is an explicit choice, and both pass through the same policy engine**.

That gives you a much stronger foundation for jAI than simply adding `--yes` to shell execution.