# Tools

jAI offers tools to the model only when `[tools.*]` allows them and
`--no-tools` is absent. Every call goes through the policy engine first
(`docs/security.md`); the descriptions below are what the model sees.

All filesystem paths are relative to the workspace unless absolute, and are
confined to it. Paths outside it, sensitive names, and symlink escapes fail with
exit code 6 and are reported to the model as `ERROR: …` so it can adapt.

Tool names must match the pattern OpenAI-compatible APIs enforce for function
names, `^[a-zA-Z0-9_-]+$` (DeepSeek rejects anything else with HTTP 400), so the
names below use `_`. Calls that arrive with the older `family.action` spelling are
still accepted and normalised to the canonical name.

## `filesystem_read`

```json
{"path": "src/main.rs", "max_bytes": 4096}
```

Reads a text file. Directories are rejected (use `filesystem_list`), files with
NUL bytes in the first 8 KiB are rejected as binary, and output is capped by
`permissions.max_tool_output_bytes` (`max_bytes` can lower it, never raise it).

Risk: **read-only**.

## `filesystem_list`

```json
{"path": "src"}
```

Lists one directory, sorted, directories marked with a trailing `/`, capped at
500 entries (the cap is reported in the output). Defaults to the workspace root.

Risk: **read-only**.

## `filesystem_write`

```json
{"path": "notes/todo.md", "content": "- ship it", "create_dirs": true, "overwrite": true}
```

Writes a text file. `create_dirs` (default `false`) creates missing parents;
`overwrite` (default `true`) must be set to `false` to refuse replacing an
existing file. Existing directories are never replaced.

Risk: **write**.

## `filesystem_delete`

```json
{"path": "target/debug/app", "recursive": false}
```

Deletes a file, or a directory when `recursive` is true (empty directories can
be removed without it). The path is canonicalised and re-checked before removal,
so deleting through a symlink cannot remove something outside the workspace.

Risk: **destructive** (the confirmation prompt warns explicitly).

## `shell_execute`

```json
{"command": "cargo test", "cwd": "crates/jai-core", "timeout_seconds": 120}
```

Runs one command through `sh -c` (Unix) or `cmd /C` (Windows):

* `cwd` must be inside the workspace (default: the workspace root);
* `timeout_seconds` defaults to 120 s and is clamped to 600 s; a timed-out
  command is killed and reported as a failure;
* the child gets a **scrubbed environment** (see `docs/security.md`), plus
  `JAI_WORKSPACE`; API keys of the jAI process are not visible to it;
* stdout and stderr are captured together with the exit code, truncated to
  `permissions.max_tool_output_bytes` and redacted before being stored.

Risk: **execute**, or **destructive** when the command matches known dangerous
patterns (`rm -rf`, `mkfs`, `dd of=`, `git reset --hard`, `… | sh`, `sudo …`).
Commands are not sandboxed beyond the working directory: use safe mode, and
review the prompt.

## `network_request`

Off by default (`[tools.network] enabled = false`).

```json
{"url": "https://example.com", "method": "GET", "headers": {"accept": "text/html"}, "body": null}
```

Only `http://` and `https://`; methods `GET`, `HEAD`, `POST`, `PUT`, `PATCH`,
`DELETE`; one request per call; 30 s timeout; response body truncated and
redacted. Non-2xx responses are returned as failures (with status and body) so
the model can reason about them.

Risk: **network**.

## Behaviour shared by every tool

* `describe()` runs before execution and produces the risk level, one-line
  summary, affected paths and working directory shown in the confirmation
  prompt.
* Arguments that are not valid JSON are treated as a **destructive** request
  (the prompt still shows exactly what was asked for) rather than silently
  running with defaults.
* Unknown tool names are refused by both the policy engine and the registry.
* `ToolOutcome.truncated` records whether the byte cap was hit.

## Enabling and disabling

```toml
[tools.filesystem]
enabled = true
[tools.shell]
enabled = true
[tools.network]
enabled = false
```

`jai -a --no-tools` sends no tool schemas at all, which is the cheapest way to
guarantee the model cannot act.

## Adding a tool
1. Implement the function in `crates/jai-tools/src/*.rs`, returning
   `ToolOutcome` and using `PathGuard` for every path.
2. Register its name in `crates/jai-tools/src/lib.rs` (name constant, spec,
   `describe`, `execute`), keeping the name inside `^[a-zA-Z0-9_-]+$`.
3. Decide its family (`filesystem`, `shell`, `network`) — the policy engine
   denies unknown families, so a new family also needs a config toggle. The
   family is the part of the name before the first `_` or `.`.
4. Cover it with a test in the same module (happy path, path escape, and any
   failure path).
