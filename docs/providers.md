# Providers

jAI speaks the OpenAI-compatible HTTP API only: `POST {base_url}/chat/completions`
with `stream: true`, and `GET {base_url}/models`. Anything that implements those
two endpoints works, including self-hosted gateways.

## Built-in presets

| Provider | Base URL | API key env | Notes |
|----------|----------|-------------|-------|
| `deepseek` | `https://api.deepseek.com` | `DEEPSEEK_API_KEY` | `https://api.deepseek.com/v1` also works |
| `openai` | `https://api.openai.com/v1` | `OPENAI_API_KEY` | |
| `ollama` | `http://127.0.0.1:11434/v1` | — (not required) | local, no key needed |
| `lmstudio` | `http://127.0.0.1:1234/v1` | — (not required) | local, no key needed |
| `openrouter` | `https://openrouter.ai/api/v1` | `OPENROUTER_API_KEY` | model ids are `vendor/model` |
| `groq` | `https://api.groq.com/openai/v1` | `GROQ_API_KEY` | |
| `moonshot` | `https://api.moonshot.cn/v1` | `MOONSHOT_API_KEY` | `api.moonshot.ai` for the international endpoint |
| `mistral` | `https://api.mistral.ai/v1` | `MISTRAL_API_KEY` | |
| `together` | `https://api.together.xyz/v1` | `TOGETHER_API_KEY` | |
| `xai` | `https://api.x.ai/v1` | `XAI_API_KEY` | |
| `anthropic` | `https://api.anthropic.com/v1` | `ANTHROPIC_API_KEY` | Anthropic's OpenAI-compatibility layer; some fields are unsupported |
| `custom` | — | `JAI_API_KEY` | set `base_url` in config or pass `--base-url` |

Those base URLs are conventional defaults, not verified contracts. Confirm the
one you care about with:

```sh
jai models <provider>       # live GET /models
jai providers               # preset + whether its key is set
```

Model names are never hardcoded in the request path: `--model`/`defaults.model`
select one, and the authoritative list comes from `GET /models`. The `models`
field in a preset exists only for documentation and error hints.

## Adding a provider

Any OpenAI-compatible endpoint can be added without a code change:

```toml
[providers.my-gateway]
base_url = "https://gateway.internal/v1"
api_key_env = "GATEWAY_API_KEY"
default_model = "internal-chat"
models = ["internal-chat"]

[providers.my-gateway.headers]
X-Tenant = "me"
```

Then `jai my-gateway -m internal-chat "hello"`. `base_url` may also be given per
run with `--base-url` (and a key with `--api-key`, though the environment is
preferred because flags are visible in the process list and shell history).

Values are resolved in this order: preset → `config.toml` → environment
(`JAI_BASE_URL`, `JAI_API_KEY`, and the provider's own key variable) → flags.

## Request shape

```json
{
  "model": "deepseek-chat",
  "messages": [{"role": "user", "content": "hi"}],
  "stream": true,
  "tools": [{"type": "function", "function": {"name": "...", "description": "...", "parameters": {}}}],
  "tool_choice": "auto"
}
```

* `temperature` / `max_tokens` are only sent when configured.
* Tool results are sent as `{"role": "tool", "tool_call_id": "...", "name": "..."}`;
  assistant turns that only call tools send `"content": null`.
* `--extra-json '{"thinking":{"type":"enabled"}}'` merges arbitrary fields into
  the body (it wins over built-in fields) — the escape hatch for provider
  extensions such as DeepSeek's thinking mode. If a provider rejects such a
  field, remove it: jAI does not pre-validate provider-specific parameters.

## Streaming

Requests send `Accept: text/event-stream` and `Accept-Encoding: identity` so
proxies do not buffer or compress the stream. Frames are decoded only once
complete, so a multi-byte character split across two TCP chunks is never
mangled; `[DONE]` is optional and a server that closes the stream without it
still finishes cleanly.

`reasoning_content` (DeepSeek reasoner and similar) is currently ignored: only
`delta.content` and `delta.tool_calls` are surfaced.

## Errors

| Situation | Error | Exit code |
|-----------|-------|-----------|
| `401`/`403` | authentication failed | 3 |
| DNS/TLS/timeout/connection refused | network error | 4 |
| `402` | insufficient balance or credits | 5 |
| `429` | rate limited | 5 |
| `400`/`404`/`422`/`5xx` | provider API error (message passed through) | 5 |
| Unparsable stream frame | provider API error | 5 |
| Cancelled with Ctrl-C | cancelled | 130 |

Provider error messages (`{"error":{"message": ...}}`) are extracted and shown;
bodies that are not JSON are truncated on a character boundary.

## Local models

```sh
ollama pull llama3.2
jai ollama -m llama3.2 -a
```

Ollama and LM Studio need no API key. If a local server requires one, set
`OLLAMA_API_KEY`/`LMSTUDIO_API_KEY` or `[providers.ollama].api_key_env`.
