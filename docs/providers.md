# Providers

All providers go through the same internal types (`runtime/src/provider/types.rs`). Each adapter changes only the wire format.

| kind               | default base URL              | endpoint               | stream format | API key |
|--------------------|-------------------------------|------------------------|---------------|---------|
| `anthropic`        | `https://api.anthropic.com`   | `POST /v1/messages`    | SSE           | required (`x-api-key`) |
| `openai`           | `https://api.openai.com/v1`   | `POST /chat/completions` | SSE         | required (`Authorization: Bearer`) |
| `openaiCompatible` | none (you must set it)        | `POST /chat/completions` | SSE         | required (`Authorization: Bearer`) |
| `ollama`           | `http://127.0.0.1:11434`      | `POST /api/chat`       | NDJSON        | optional |

## Setup in VS Code

1. Run **task-x: Add LLM Provider…** (or click **+** in the Chat view title). The dialog has fields for provider type, base URL, API key, and model.
2. Click **Fetch models** to load the model list from the endpoint, or type a model id.
3. Click **Test connection**, then **Save and use**. The key goes into VS Code SecretStorage (the OS keychain). It is not written to settings. Leave the key field empty to keep a saved key.

**task-x: Select Provider and Model** and **task-x: Set API Key** stay available as quick commands.

Settings: `task-x.provider`, `task-x.<kind>.model`, `task-x.<kind>.baseUrl`, `task-x.requestTimeoutMs`, `task-x.maxTokens`, `task-x.systemPrompt`, `task-x.runtimePath`.

## Local Ollama

```sh
ollama serve
ollama pull qwen3
```

Then select **Ollama (local)** and the model. **Test Model Connection** reads `/api/show`. If the model does not list the `tools` capability, the result says "no tool calling".

## OpenAI-compatible endpoints

An API key is required. Set the base URL to the URL that comes before `/chat/completions`, normally with `/v1` (for example `http://127.0.0.1:8080/v1` for llama.cpp, vLLM or LM Studio).

## Differences between providers

- **Max tokens:** `openai` sends `max_completion_tokens` and asks for usage in the stream (`stream_options.include_usage`). `openaiCompatible` sends `max_tokens` and does not send `stream_options`, because many servers reject them.
- **Usage:** some compatible servers do not report token counts. Then `usage` fields are `null`.
- **Tool call ids:** Ollama does not always return ids. The runtime makes `call_<n>`.
- **Tool results:** OpenAI-style APIs put each result in a separate `role: tool` message. Anthropic puts it in a `tool_result` content block. The adapters convert this for you.
- **Parallel tool calls:** reported as not supported for Ollama.
- **Stream end:** some compatible servers close the stream after `finish_reason` and do not send `[DONE]`. The runtime accepts this. A stream that closes before any finish signal is a `network` error.
- **Thinking blocks:** Anthropic thinking blocks are ignored in this version.

## Security

- The runtime does not log API keys, authorization headers, or prompt text. `ProviderConfig`'s `Debug` output hides the key, and auth headers are marked sensitive.
- The runtime does not send an API key over plain `http` unless the host is loopback.
- The extension starts the runtime with a small allowlist of environment variables (path, locale, proxy, and TLS settings).
