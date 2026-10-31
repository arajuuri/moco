# Providers and models

Sources: `packages/ai/src/{llm,route,protocols,providers}`, `core/src/{provider,model,model-resolver,models-dev,credential}.ts`, `core/src/plugin/provider/*`.
Upstream is a large provider matrix (~50 provider modules, ~45 protocol adapters including image/speech/video). For a rewrite implement a **small set of wire protocols**
and let the catalog supply base URLs.

## Abstraction

```
Model { id, providerID, name, family, capabilities {tools, input: text|image|pdf|audio, output, reasoning}, limit {context, input?, output},
        cost {input, output, cache {read, write}} (USD per million tokens), variants[] (e.g. reasoning effort presets), route {protocol, baseURL, auth} }
Request { system[], messages[], tools[] (name, description, JSON schema), toolChoice, settings (temperature, topP, maxOutput, reasoning...), headers, body overlay }
Stream events: text-start/delta/end, reasoning-start/delta/end, tool-input-start/delta/end, tool-call {id,name,input,providerExecuted?},
               step-finish {reason, usage}, provider-error {code, retryable, retryAfterMs?, contextOverflow?}
Usage { input, output, reasoning, cacheRead, cacheWrite } -> cost via model.cost
```
One interface: `stream(request) -> Stream<event>`; each protocol adapter converts the neutral request to its wire JSON and parses SSE chunks into the neutral events.
Per-model/agent overlays: `request.settings`, extra `headers`, and a raw `body` object merged into the request JSON (config `providers.<id>.models.<id>`, `agents.<id>.request`).

## Wire protocols to implement, in priority order

1. **Anthropic Messages** (`protocols/anthropic-messages.ts`): `POST {baseURL=https://api.anthropic.com/v1}/messages`, headers `x-api-key`, `anthropic-version`, SSE streaming; content blocks text/thinking/tool_use/tool_result/image/document; prompt caching via `cache_control` markers (see `cache-policy.ts`); extended thinking with budget.
2. **OpenAI-compatible Chat Completions** (`protocols/openai-chat.ts`, `openai-compatible-chat.ts`): `POST {baseURL=https://api.openai.com/v1}/chat/completions`, SSE `data:` chunks, `tool_calls` deltas. Covers OpenRouter, Groq, DeepSeek, Together, Fireworks, Ollama, LM Studio, vLLM, etc.: only base URL and key differ.
3. **OpenAI Responses** (`protocols/openai-responses.ts`): `POST /responses`, needed for newer OpenAI models and reasoning items, with encrypted reasoning replay.
4. **Gemini** (`protocols/gemini.ts`) and Vertex variants. Later.
5. Bedrock Converse + event-stream framing. Later (needs SigV4).

I did not read the adapters line by line. When implementing each, use the vendor docs as primary reference and the adapter as a cross-check for edge cases (tool-call id handling, empty-content workarounds, reasoning replay, error classification).

## Error classification (needed by retry)

Map HTTP/transport failures to: `Auth`, `RateLimit(retryAfterMs)`, `ProviderInternal(retryAfterMs)`, `InvalidRequest`, `ContextOverflow`, `ContentFilter`, `Transport(code: Timeout|ECONNRESET|...)`. Retryable: RateLimit, ProviderInternal, Transport. `ContextOverflow` is detected from provider-specific error strings/codes and triggers compaction, not retry (`isContextOverflowFailure`).

## Catalog

Upstream ships a snapshot of the **models.dev** catalog and refreshes it from the network (`core/src/models-dev.ts`, `models-dev/snapshot.txt`). Entries give provider id, env var names for keys, npm-style package (selects the protocol), models with limits/cost/capabilities. In your app: embed a trimmed JSON snapshot, refresh from `https://models.dev/api.json` (verify the URL and schema version yourself; the code accepts v1 `/api.json` and a newer v2 shape), and let `opencode.json`-style config add custom providers/models.

## Selecting the model

Order (`model-resolver.ts`): session-selected model -> agent `model` -> config `model` -> last used / first connected provider default (`model.default`). Model refs look like `provider/model` in config strings, optionally with a variant (`provider/model#variant`; check `schema/src/config/model.ts` parse for the exact separator).

## Credentials

Sources of keys, in order: stored credential (UI "connect"), environment variable named by the catalog entry, config `providers.<id>`. Stored in `auth.json` (mode `0600`, see `02`) in the rewrite, not the Keychain. OAuth flows exist upstream for some providers (ChatGPT, GitHub Copilot, GitLab); skip initially, use API keys. For MCP OAuth see `08`.

## Cost and usage

After each step record usage, compute cost = sum(tokens x price / 1e6) with tier pricing by context size, accumulate on the session (`tokens_*`, `cost`), publish `session.usage.updated`. Sidebar shows context usage vs limit.

## Warming (optional)
Keep cache hot with periodic tiny requests (`config.warming`). Skip.

## moco implementation notes (Phase 2, providers slice)

Code: `catalog.rs`, `provider/{anthropic,openai_chat,registry}.rs`, `auth.rs`.
- **Protocols implemented**: Anthropic Messages and OpenAI-compatible Chat Completions (`POST {base}/chat/completions`, `stream_options.include_usage`, tool calls assembled from deltas, `reasoning_content` shown but not stored, cached tokens read from `prompt_tokens_details`). OpenAI Responses and Gemini are not implemented; catalog providers that need them are listed as "unsupported protocol". Chat Completions is used for `api.openai.com` too, with `max_completion_tokens`.
- **Catalog**: an embedded snapshot (`catalog_snapshot.json`, 10 providers, regenerate with `scripts/update-catalog-snapshot.py`), replaced by `<app dir>/cache/models.json` after `moco models --refresh` (full models.dev download filtered to usable providers and tool-calling models, about 7,000 models). Only the v1 `api.json` shape is parsed. Providers whose `npm` package is one of the AI SDK OpenAI-compatible families use Chat Completions; a small table supplies base URLs for those without an `api` field.
- **Model refs** are `provider/model` (the first `/` splits, so `openrouter/anthropic/claude-x` works). Resolution: session choice (`/model`, `--model`, `MOCO_MODEL`), then the agent's `model`, then config `model`, then `anthropic/claude-sonnet-5-5`. There is no "last used / first connected" fallback; a missing provider fails with the env var and `moco auth` hint.
- **Credentials**: an environment variable named by the catalog entry (or `<PROVIDER>_API_KEY`) wins over `auth.json`; `moco auth <provider>` reads the key from stdin (echo off on a terminal) and writes `auth.json` with mode 0600, keeping other entries.
- **Config `providers.<id>`** adds or overrides a provider: `base_url`, `env`, `protocol` (`openai-chat` default for new ones, or `anthropic`), `headers`, `models.<id>` (`name`, `limit{context,output}`, `cost{input,output,cache_read,cache_write}`), `disabled`. Providers defined this way need no key (Ollama, LM Studio, vLLM):
```jsonc
{ "providers": { "ollama": { "base_url": "http://localhost:11434/v1",
    "models": { "qwen3-coder": { "limit": { "context": 32768, "output": 8192 } } } } },
  "model": "ollama/qwen3-coder" }
```
- **Cost**: tokens times catalog prices per million, including context tiers (a tier applies when input plus cached tokens exceed its size). Totals are stored on the session (`cost`, `tokens`); the terminal prints tokens, cost, session cost and context fill after each turn. Models without a price show "cost n/a".
- **Not done**: variants (reasoning effort presets), per-model request overlays (`request.settings/headers/body`), OAuth flows, usage limits, warming.
- **Prompt caching (Anthropic protocol)**: `cache_control: {"type":"ephemeral"}` markers on the last tool definition, the system prompt, the last block of the final message and the last block of the message that ended the previous request of the agent loop (four, the API maximum; fewer for short requests). The fourth marker means a step that adds many content blocks still hits the previous step's cache entry exactly instead of relying on the API's lookback window. Default 5-minute lifetime; the 1-hour option is not used. Turn it off per provider with `providers.<id>.prompt_cache: false`. This only pays off if every request extends the previous one byte for byte, which is why the Plan agent's reminder lives in the system prompt (not appended to the last message) and why `every_request_of_a_turn_extends_the_previous_one` exists as a test. Things that do break the prefix and are expected: compaction, a changed `AGENTS.md`, switching model or agent, and the tools-disabled final step. Measured on macOS 10.14 against the live API: a two-step turn cost $0.0105 instead of about $0.016, and follow-up turns in the same session within five minutes cost $0.0006 instead of about $0.015 (the usage line shows `cached` and `cache written` counts).
- **Other protocols**: OpenAI-compatible endpoints cache automatically on the server side and report `cached_tokens`, which is used for pricing; no markers are sent. OpenRouter's Anthropic-backed models would need explicit markers in content parts, which are not implemented.

