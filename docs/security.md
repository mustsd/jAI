# jAI security model

jAI runs commands on your machine and stores your conversations. This document
describes what is protected, how, and what is explicitly *not* protected yet.

## Data classification

| Data | Location | Protection |
|------|----------|------------|
| API keys | process environment only | never written to disk, never logged, scrubbed from child processes, redacted from text |
| Conversations, tool output | `~/.jai/jai.db` | owner-only file, WAL sidecars owner-only, filtered by the redactor before storage |
| Audit trail (tool calls, decisions) | `jai.db` tables `tool_calls` / `tool_results` | same |
| Configuration | `~/.jai/config.toml` | owner-only file; contains no credentials |
| Input history | `~/.jai/history.txt` | inside the owner-only directory |
| Debug logs | not implemented | — |

Unix permissions: `~/.jai` is created `0700`, `config.toml` and `jai.db` `0600`;
`jai doctor` reports a loosened directory, and the interactive agent warns on
exit. **Windows ACLs are not narrowed** — the directory lives under the user
profile, which is user-scoped by default, but jAI does not verify it.

## Threat model

In scope:

* A model (or a prompt-injected document/web page) trying to read credentials,
  escape the workspace, or run something unexpected.
* A crash mid-tool-call leaving an unclear state.
* Secrets leaking into stored data, the terminal, or a provider request.
* Another local process or web page driving your agent.

Out of scope (documented, not mitigated):

* A local attacker with your user account (they can read the database).
* Malicious code you explicitly approve and run.
* Encryption at rest.
* Prompt injection that stays *inside* the granted permissions: jAI limits the
  blast radius, it does not judge intent.

## Layers

1. **Workspace sandbox** (`jai-policy::Sandbox`). Every path a tool resolves is
   normalised lexically and must sit inside the workspace (the process working
   directory, `--workspace`, or `[permissions].workspace`) or an explicitly
   allowed path. `..` escapes, absolute paths outside, and prefix lookalikes
   (`/work` vs `/workspace-other`) are rejected. Existing paths are then
   canonicalised so symlinks cannot escape either.
2. **Sensitive-path denylist** (`jai-policy::denylist`). `.env*`, `*.pem`,
   `*.key`, `*.p12`, `*.jks`, `id_rsa*`, `credentials*`, `secrets*`, `~/.ssh`,
   `~/.aws`, `~/.gnupg`, `~/.kube`, `~/.config/gcloud`, `~/.jai` (jAI's own
   state, so a tool cannot read your conversations), `/etc/shadow`,
   `/etc/sudoers`, `/etc/passwd`. Users can add globs via
   `[permissions].deny_paths`.
3. **Tool families.** `[tools.filesystem]`, `[tools.shell]` and
   `[tools.network]` gate whole families; `--no-tools` disables everything.
   Network access is **off** by default. Unknown tool names are denied.
4. **Permission modes.**
   * `safe` (default): every tool call is confirmed. The prompt shows tool,
     arguments, risk level, working directory and files; the default answer is
     "no" (Enter refuses).
   * `yolo` (`--mode yolo`, or `permission_mode = "yolo"`): no prompts, but the
     same policy engine still denies disabled tools, out-of-workspace paths and
     sensitive files. Yolo removes the *question*, not the *rules*.
5. **Shell hardening.** `shell_execute` runs `sh -c` (Unix) or `cmd /C`
   (Windows) with `env_clear()` plus an allow-list (`PATH`, `HOME`, `LANG`,
   `TERM`, `TMPDIR`, `SystemRoot`, ...) and `JAI_WORKSPACE`; output is captured,
   truncated to `permissions.max_tool_output_bytes` and redacted. A visual
   classification marks obviously destructive commands (`rm -rf`, `mkfs`,
   `dd of=`, `git reset --hard`, `… | sh`, `sudo …`) as *destructive* risk.
6. **Secret redaction** (`jai-core::Redactor`). Applied to tool output, stored
   messages and model context: values of `*_API_KEY`/`*_TOKEN`/`*_SECRET`
   environment variables, `sk-`/`ghp_`/`xoxb-`/`AKIA`/`AIza` token shapes,
   `Authorization` headers, `KEY=value` assignments (skipping counters such as
   `max_tokens`), and PEM private-key blocks. It is a safety net, not a proof:
   an unknown key format can still pass through.
7. **Audit trail.** Every tool request is stored *before* it runs (status
   `running`) and updated with the outcome and the policy decision, separately
   from the conversation, so `jai session <id> --audit` answers "what did jAI
   actually do?".
8. **Crash recovery.** On startup, tool calls left `running` are marked
   `interrupted` and reported; jAI never replays them automatically.
9. **Web mode.** Loopback bind by default (`web.local_only = true` refuses other
   addresses), every request needs the per-run token printed at startup
   (`?token=…` or `X-JAI-Token`), and non-loopback `Host` headers are rejected
   (DNS-rebinding/CSRF defence). Approval prompts require an explicit click;
   an unanswered prompt is denied after 5 minutes, and a disconnected browser
   cancels the turn.

## Data retention

`jai session <id> --delete` removes a conversation, its messages, its audit rows
and its usage; `jai purge` removes everything after an explicit confirmation
(`--yes` skips it for scripts). `--no-persist` keeps a run entirely in memory.
Removing the database is a normal way to erase history: nothing is replicated
elsewhere.

## Reporting

This is a young project with no external audit. If you find a weakness, please
open an issue with a minimal reproduction, and do not include real credentials.
