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
