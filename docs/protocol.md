# Runtime protocol (v1)

The VS Code extension and `task-x-runtime` talk JSON-RPC 2.0 over the runtime's stdio.

## Framing

- One JSON object per line, UTF-8, terminated by `\n`. No `Content-Length` headers.
- Maximum message size is 32 MiB. A larger line ends the session.
- stdout carries protocol messages only. stderr carries diagnostics (set `TASK_X_LOG` to `error`, `warn`, `info`, `debug`, or `trace`; the default is `warn`). At `info`, each model call logs its token counts, each tool call logs its outcome, class, and duration, and each retry logs its attempt. Prompt and output text are never logged.
- Request ids must be strings or integers. `1` and `"1"` are different ids.

## Lifecycle

1. The client sends `initialize`. Every other request before it fails with `-32003`.
2. The client sends requests and notifications.
3. The client sends `shutdown`. The runtime cancels in-flight work and rejects new requests.
4. The client sends the `exit` notification. The runtime waits up to 5 s for open replies, then exits with code 0.

When stdin closes, the runtime does the same as `exit`.

## Methods

### `initialize` (request)

```json
{ "protocolVersion": 1, "clientInfo": { "name": "task-x-vscode", "version": "0.1.0" } }
```

Result:

```json
{
  "protocolVersion": 1,
  "serverInfo": { "name": "task-x-runtime", "version": "0.1.0" },
  "providers": [{ "kind": "anthropic", "capabilities": { ... }, "requiresApiKey": true }]
}
```

A different `protocolVersion` fails with `-32002`. `error.data.serverProtocolVersion` gives the supported version.

`initialize` also marks any task file still saved as `running` as `failed` (with `error` set): nothing can be running before the first request, so such a record belongs to a runtime that was killed mid-task. Clients therefore never list phantom running work.

### `provider/capabilities` (request)

No params. The result is the `providers` array from `initialize`.

### `provider/listModels` (request)

Params: `{ "provider": ProviderConfig }`. Result: `{ "models": string[] }` (sorted, max 500).

### `provider/contextWindow` (request)

Params: `{ "provider": ProviderConfig }`. Result: `{ "tokens": number }`, the context window of `provider.model`. The runtime asks the provider first (the same lookup and 5-minute cache as `limits.contextWindowTokens`). If the provider does not say, the result is the built-in estimate.

### `provider/test` (request)

Params: `{ "provider": ProviderConfig }`. The runtime lists models to check connection and credentials. For Ollama, it also reads `/api/show` to find tool support. For an OpenAI-compatible endpoint without `/models`, it sends a one-token chat.

```json
{ "latencyMs": 84, "models": ["qwen3:latest"], "modelAvailable": true, "capabilities": { ... } }
```

`modelAvailable` is `null` when the endpoint cannot list models.

### `chat/send` (request, streaming)

Params: `{ "provider": ProviderConfig, "request": ChatRequest }`.

While the request runs, the runtime sends `chat/event` notifications:

```json
{ "jsonrpc": "2.0", "method": "chat/event", "params": { "requestId": 7, "event": { "type": "textDelta", "text": "Hel" } } }
```

Event types:

| type        | fields                         | meaning                                     |
|-------------|--------------------------------|---------------------------------------------|
| `textDelta` | `text`                         | Next part of the assistant text             |
| `toolCall`  | `id`, `name`, `input` (object) | A complete tool call (arguments are parsed) |
| `usage`     | `usage.inputTokens`, `usage.outputTokens` | Token counts, when the provider reports them |
| `done`      | `stopReason`                   | The provider finished the response          |

The final result combines all events:

```json
{ "text": "Hello", "toolCalls": [], "stopReason": "endTurn", "usage": { "inputTokens": 12, "outputTokens": 3 } }
```

`stopReason` is one of `endTurn`, `maxTokens`, `toolUse`, `stopSequence`, `contentFilter`, `unknown`.

### `workspace/open` (request)

Params: `{ "root": "/abs/path", "exclude"?: string[] }`. `userInstructions` and `userPromptsDir` from older clients are accepted and ignored. The runtime scans the folder, builds the file and symbol index, detects the project, and loads instructions. It replaces any workspace that was open before. Result: a workspace summary (`project`, `stats`, `instructions`, `excludes`). See [context.md](context.md).

### `workspace/progress` (notification, runtime → client)

Sent while `workspace/open` and `workspace/refresh` run, and after `workspace/filesChanged` updates the index:

| phase | fields |
|---|---|
| `indexing` | `files` (seen so far), `parsed`, `elapsedMs`. At most every 150 ms. |
| `graph` | `files`, `elapsedMs`: the project graph is being built |
| `ready` | `stats` (as in the workspace summary), `graph` (see `graph/project` stats), `elapsedMs` |
| `updated` | `stats`: changed files were re-indexed; the graph is rebuilt on its next use |
| `failed` | `error` |

### `graph/project` (request)

Params: `{ level?: "file"|"module", focus?: string, depth?: number, maxNodes?: number, importsOnly?: boolean }`. Result: `{ nodes, edges, total, stats }`.

- The graph comes from the index. **Import** edges come from resolved import statements (Rust `mod`/`use crate|super|self`, TypeScript/JavaScript relative `import`/`require`, Python `import`/`from`, Go package imports). **Reference** edges link a file to the file that defines a symbol it uses (any tree-sitter language). A name defined in more than 3 files is not linked.
- `level: "module"` rolls files up into their directories. Without `focus`, the `maxNodes` (default 250, at most 600) most connected nodes are returned; with `focus` (a file or directory), the nodes within `depth` (default 1, at most 4) steps.
- A node is `{ id, language, symbols, files, degree, inCycle }`. An edge is `{ from, to, weight, kind }`, where `from` and `to` are indexes into `nodes` and `weight` counts linked symbols.
- `stats` is `{ files, edges, importEdges, referenceEdges, modules, cycles, orphans, hubs: [{ path, dependents }], importCycles: string[][], buildMs, version }`. `version` is the graph engine version. Import cycles are strongly connected components of the import edges.

### `graph/file` (request)

Params: `{ path }`. Result: `{ path, dependsOn, usedBy, importCycle }`; each list entry is `{ path, kind, symbols }`. Fails with `-32602` when the file is not indexed. The agent has the same data through the read-only tool `get_dependencies`.

### `workspace/refresh` (request)

No params. Scans again. Files with the same size and modification time are not parsed again. Result: a workspace summary.

### `workspace/filesChanged` (notification)

Params: `{ "paths": string[] }` (absolute, or relative to the root). The runtime updates only these paths. Deleted paths leave the index.

### `context/inspect` (request)

No params. Result: the current workspace summary. Fails with `-32004` when no workspace is open.

### `context/retrieve` (request)

Params: `{ "query": string, "budgetTokens"?: number, "focusPaths"?: string[] }`. Result:

```json
{
  "retrieval": { "terms": ["..."], "snippets": [{ "path": "src/a.rs", "startLine": 3, "endLine": 9, "text": "...", "score": 21.5, "reason": "defines function `parse`" }], "filesConsidered": 4, "budgetTokens": 8000, "usedTokens": 120 },
  "instructions": [{ "source": "CLAUDE.md", "scope": "workspace", "dir": "", "content": "...", "truncated": false }]
}
```

### `prompt/refine` (request)

Params: `{ "prompt": string, "provider"?: ProviderConfig, "routes"?: Route[], "focusPaths"?: string[] }`. `Route` is `{ kinds, provider }`, as in `agent/run`. The runtime classifies the prompt, picks the route for its task kind (else `provider`), and asks that model once to rewrite the prompt so a coding agent can act on it. When a workspace is open, repository excerpts for the prompt (about 3000 tokens, `focusPaths` first) go with it. Result:

```json
{ "text": "Rewritten prompt", "kind": "codeModification", "model": "qwen3:latest", "routed": false, "usage": { "inputTokens": 900, "outputTokens": 120 } }
```

The prompt is 1..=100 000 characters. With no route and no `provider`, the request fails with `-32602`. Reasoning (`<think>`), code fences, and echoed `<request>` tags are removed from the text.

### Agent, tools, and extensions

| Method | Kind | Params → result |
|---|---|---|
| `agent/run` | request (streams `agent/event`) | `{ prompt, images?, provider, chatMode: "ask"\|"plan"\|"agent"\|"team", taskId?, editor?, policy?, limits?, routes?, fallbacks?, roleProviders?, prices? }`. `images` is `ImageInput[]` (see [Images](#images)) → summary `{ taskId, status, text, changedFiles, operations, validations, validationRan, skippedChecks, usage, metrics, model, iterations, toolCalls, error }` |
| `agent/event` | notification (runtime → client) | `{ taskId, event }`. Event types: `started`, `model`, `iteration`, `textDelta`, `assistantMessage`, `retry`, `modelSwitched`, `compacted`, `toolStarted`, `toolFinished`, `approvalRequested`, `approvalResolved`, `fileChanged`, `steered`, `hook`, `subagent`, `subagentStarted`, `subagentFinished`, `planStarted`, `stepSkipped`, `merged`, `bestOfDone`, `usage`, `warning`, `error`, `done`. See [Team mode](#team-mode). A `run_command` call with `background: true` gets two `toolFinished` events: one at once, with the result `{ background: true, command, message }`, then one with `background: true` and the real output when the command ends. The model gets that output as a user message before its next step, and the task does not end while background commands run. `agent/cancelTool` stops a background command too. |
| `agent/cancelTool` | request | `{ taskId, callId }` (from `toolStarted`; a subagent's own task id for its calls) → `true` when a running built-in tool call (or a Claude Code `Bash` call) was stopped. Only that call stops: its process is ended, the model gets "Task has been stopped by user. Continue with the next step…" (for Claude Code, as a user message after the failed result), and the task goes on. |
| `agent/cancelSubagent` | request | `{ taskId }` (a subagent's task id from `subagentStarted`) → `true` when a running subagent was stopped |
| `agent/steer` | request | `{ taskId, text }` → `true` when a running top-level task (`agent/run` or `claude/run`) accepts the message, `false` when no such task is running or its queue (10 messages) is full. The message joins the conversation before the task's next model turn, or before it would end. Each accepted message is reported by a `steered` event `{ text }` when it joins. Text is 1..=100 000 characters after trimming; longer or empty text is an invalid-params error. |
| `agent/bestOf` | request (streams `agent/event`) | `{ prompt, providers: ProviderConfig[2..4], taskId?, editor?, policy?, limits?, prices? }` → `{ taskId, candidates, costUsd }`. See [Best of N](#best-of-n). |
| `worktrees/apply` / `worktrees/discard` | request | `{ name }` → the merge result `{ applied, files, changed, operation }` / `{ discarded }`. Apply goes through the edit log, so the change is pending review and can be undone. |
| `agent/requestApproval` | **request runtime → client** | `{ taskId, callId, tool, permission, input, preview }` → `{ decision: "allow"\|"allowSession"\|"deny" }`. No answer, or an error, counts as deny. |
| `agent/askUser` | **request runtime → client** | `{ taskId, callId, questions: [{ id, text, kind: "radio"\|"checkbox", options }] }` → `{ answers: { [questionId]: string[] } \| null }`. Sent when the model calls `ask_user` (top-level tasks only; 1 to 4 questions, 2 to 8 distinct options each), and when Claude Code (`claude/run`) calls `AskUserQuestion`. Claude Code gets the answers as `updatedInput.answers` (question text → chosen labels joined by ", "), or a denial when the user dismisses them; this question never asks for approval. Radio answers keep one option. `answers: null` means the user dismissed the questions. No answer, or an error, counts as dismissed. Options outside the offered list are dropped. The tool result for the model is `{ answered, answers: [{ question, selected }] }`. Old clients that do not handle the request see a dismissal, so the protocol version is unchanged. |
| `agent/tasks` / `agent/task` / `agent/deleteTask` | request | the saved task list / `{ taskId }` → `{ summary, messages, title }` / `{ taskId }` → bool |
| `tools/list` / `tools/call` | request | the registry with schemas and permission classes / `{ name, input }` → tool result (error code `-32005`, `data` = `{ kind, message }`) |
| `edits/list` / `edits/pending` | request | the operation log with diffs / the net unsettled change for each file `{ path, original, current, added, removed, operations }` |
| `edits/keep` / `edits/revert` | request | `{ paths? }` → `{ kept }` / `{ reverted, conflicts }`. A file that changed after the agent's edit is never reverted. |
| `edits/undo` | request | `{ operation }` → `{ restored, conflicts }` |
| `config/hooks` | request | `{ hooks, workspaceTrusted }` → `{ hooks, workspaceHooks }` |
| `mcp/configure` / `mcp/status` | request | `{ servers, workspaceTrusted }` → server status / server status. A server is `{ type?: "stdio"\|"http"\|"sse", command?, args?, env?, cwd?, url?, headers?, timeoutMs?, disabled? }`; unknown fields (from Claude or VS Code files) are ignored. Status entries are `{ name, command, disabled, running, tools, error }`, where `command` is the URL for a remote server. |
| `skills/list` / `worktrees/list` | request | the skills found / the task-x worktree names |
| `skills/resolve` | request | `{ prompt, paths? }` → `{ active: [{ name, reason, source, text }] }`: the skills that apply (a leading `/name`, a trigger word, or a file pattern), with `{{param}}` values filled in. Clients use it to give skills to another agent, such as the Claude Code CLI. |

`initialize` also accepts `dataDir` (an absolute path where task files are saved) and `sandbox` (a boolean, default true).

With a `dataDir`, the edit log (checkpoints) is saved per workspace in `dataDir/checkpoints/` after every change and loaded by `workspace/open`. After a restart, `edits/pending`, `edits/undo`, `edits/keep`, and `edits/revert` work on the earlier edits. When the log has more than 500 operations, the contents of old settled operations are dropped; `edits/undo` on one of them fails with "too old to undo".

### Team mode

`chatMode: "team"` gives the lead agent the edit tools, `run_subagent`, `worktree_apply`, `worktree_discard`, and `run_plan`. It needs a git repository with a commit; otherwise a `warning` event says so and the task runs as one agent.

`run_plan` takes `{ steps: [{ id, role, objective, context?, dependsOn? }], merge? }` (1 to 12 steps, roles `research`, `implementation`, `testing`, `review`, `documentation`). A role is matched case-insensitively and accepts the common aliases a model writes (`Research`, `code`, `docs`, `qa`, `reviewer`); anything else fails the call with `-32602`-style `invalidInput` naming the five roles. The runtime starts every step whose dependencies are done, up to `limits.maxParallel` (default 3, at most 8) at a time. A step gets the reports of its direct dependencies, and a writing step starts from the patches of all its writing ancestors (committed in its worktree, so its own patch holds only its changes). A step whose dependency did not complete is skipped. When all steps end, writing steps are merged into the working tree in dependency order through the edit log. A step that conflicts keeps its worktree.

`roleProviders: [{ role, provider }]` sets the model for subagents of one role. Events:

| type | fields |
|---|---|
| `planStarted` | `callId`, `maxParallel`, `steps: [{ id, role, objective, dependsOn }]` |
| `subagentStarted` | `name` (worktree name), `taskId`, `role`, `objective`, `kind`, `model`, `dir` (worktree folder or null), `step` |
| `subagent` | `name`, `taskId`, `event` (the subagent's own event) |
| `subagentFinished` | `name`, `taskId`, `role`, `status`, `error`, `costUsd`, `unpricedTokens`, `tokens`, `files`, `added`, `removed`, `worktree` |
| `stepSkipped` | `step`, `reason` |
| `merged` | `step`, `name`, `ok`, `files`, `error` |

### Best of N

`agent/bestOf` runs the same task as an implementation subagent on each provider at the same time, each in its own worktree. Nothing is merged. Each candidate is `{ name, kind, model, status, report, files, added, removed, validations, costUsd, tokens, dir, patch }` (`patch` is cut at 60000 characters), or `{ name: null, kind, model, status: "failed", error }`. The client applies one with `worktrees/apply` and removes the others with `worktrees/discard`.

### Context compression

`limits.contextWindowTokens` sets the model's window. Without it the runtime asks the provider for the model's window and caches the answer for 5 minutes: Ollama from `/api/show` (`model_info.*.context_length`), Anthropic from `/v1/models` (`max_input_tokens`), OpenAI-compatible endpoints from `/v1/models` (`context_length`, `context_window`, or `max_token_size`). When the endpoint does not report one, it falls back to the built-in estimate for the model (Anthropic: 1M for Claude 4.6 and later, otherwise 200K; OpenAI: 1M for gpt-4.1, 272K for gpt-5, otherwise 128K; Ollama and OpenAI-compatible: 32768). `ChatRequest.contextWindow` passes the window to APIs that take one (Ollama `num_ctx`).

Before each model call, the runtime estimates the prompt size (characters ÷ 4, calibrated by the real `inputTokens` of the last response). Above `limits.compactPercent` (default 85, 30 to 95) of the window it compresses in stages and stops as soon as the prompt fits. Repository excerpts from the first request are removed last, and only while the prompt is still above that limit:

1. **Prune** old tool results (all but the last 6 messages; errors are kept): a file read that a later read or edit of the same file replaced, and a result repeated later, become short stubs; an output over 2000 characters keeps its first 1200 and last 400 characters.
2. **Fold** the middle of the history into a progress summary (tool calls, files changed, notes, recent errors).
3. Above 90% of the window, **drop** the repository excerpts added to the first request.

Compressed content starts with `[context compressed]`. Events: `compacted` `{ folded, pruned, droppedRetrieval, beforeTokens, afterTokens, windowTokens }`; and after every model response, `context` `{ usedTokens, windowTokens }`.

### Claude Code engine and the task-x tools

`claude/run` also takes `images` (`ImageInput[]`, sent with the prompt as image content blocks) and `mcpServers` (more servers in Claude Code's `mcpServers` format, added to `--mcp-config`; the name `task-x` is reserved).

Claude Code always runs with `sandbox.allowUnsandboxedCommands: false`, so a command cannot leave the sandbox. A `Bash` call with `dangerouslyDisableSandbox: true` is denied at once, without asking the user.

`claude/run` takes `useTaskTools` (default true). The runtime then starts Claude Code with:

- `--mcp-config` that launches the runtime itself as an MCP server: `task-x-runtime --mcp-server --root <workspace>`. It serves the same tool registry (`tools/list`, `tools/call`) over stdio, so Claude's tools are `mcp__task-x__read_file`, `mcp__task-x__replace_text`, and so on, with the same workspace, stale-edit, and sandbox checks.
- `--disallowedTools` for Claude Code's own file tools (Read, Edit, MultiEdit, Write, NotebookEdit, Grep, Glob, LS) and for shell commands that rewrite files (`sed -i`, `perl -i`, `cat >`, `cat <<`, `tee`).
- A `PreToolUse` hook (`task-x-runtime --hook-shell-edit`) that blocks the same rewrites anywhere in a command (after `&&`, in a pipe, in `$(…)`). Inline scripts (`node -e`, `python3 -c`, `python3 - <<EOF`, and the like) are blocked only when they call a file-write API (`writeFile`, `open(…, "w")`, `write_text`, `shutil.*`, `os.remove`, …), so read-only checks run.
- `--allowedTools` for the task-x read tools; the write tools too when the permission mode is `acceptEdits`; the command tools too when Bash is allowed.
- `--append-system-prompt` telling Claude to work on files only through these tools.
- `--permission-prompt-tool mcp__task-x__approve`. When a tool call needs permission, Claude Code calls this MCP tool. The MCP server sends the request to the runtime over a loopback TCP connection (address and a random token in the server's `TASK_X_APPROVAL` environment variable). The runtime asks the client with `agent/requestApproval` (`permission` is the task-x class of the tool) and reports `approvalRequested` and `approvalResolved` events. `allowSession` covers later calls of the same tool in the run. No answer, an error, or a wrong token counts as deny. With `useTaskTools: false`, the MCP server starts with `--no-tools` and serves only this tool.

The prompt goes to Claude Code's stdin as a stream-json user message (`--input-format stream-json`). Stdin stays open while the run takes `agent/steer` messages: waiting messages are sent as one more user message, and each is reported by a `steered` event. When Claude Code has answered every message and none waits, the runtime closes stdin and the run ends. The summary has the text of the last answer and the summed token counts.

The MCP server can also be used by any MCP client: `task-x-runtime --mcp-server [--root DIR]`.

### Costs

`metrics` in a summary has `costUsd` (subagents included), `unpricedTokens` (billed tokens of models with no price), `subagents`, `subagentCostUsd`, and `subagentTokens`. Prices are per million tokens and match a model id by prefix: `prices: [{ model, inputPerMtok, outputPerMtok }]` first, then built-in Anthropic first-party rates. Local providers cost nothing. The `usage` event also carries `costUsd`.

### Review findings

A review answer can end with a fenced code block whose language is `task-x-findings`: a JSON array of `{ path, line, severity: "error"|"warning"|"info", message }`. The extension shows these in the Problems panel. The review subagent role asks for this block.

### `$/cancelRequest` (notification)

Params: `{ "id": <request id> }`. The cancelled request replies with error `-32800` and `data.kind = "cancelled"`. An unknown id is ignored.

### `shutdown` (request), `exit` (notification)

See [Lifecycle](#lifecycle).

## Types

### ProviderConfig

| field       | type   | notes |
|-------------|--------|-------|
| `kind`      | `"anthropic" \| "openai" \| "openaiCompatible" \| "ollama"` | required |
| `model`     | string | required, 1–256 characters, no control characters |
| `baseUrl`   | string | optional, except for `openaiCompatible`. Must be `http` or `https` and contain no credentials |
| `apiKey`    | string | required for `anthropic`, `openai`, and `openaiCompatible`; optional for `ollama`. Never logged. Rejected over plain `http` unless the host is loopback |
| `timeoutMs` | number | default 120000, clamped to 1000–600000. Applies to the wait for headers and to each idle gap in a stream |

Unknown fields are rejected.

### EditorContext

`agent/run` takes `editor: { activeFile?, selection?: { path, startLine, endLine, text }, openFiles, diagnostics, mentions? }`. `mentions` holds files and folders the user referenced with `@path` (at most 10). The runtime attaches each file's indexed text (40000 characters each, 120000 in total) or each folder's file list. Excluded files are not in the index, so they are never attached.

### ChatRequest

| field         | type          | notes |
|---------------|---------------|-------|
| `system`      | string        | optional |
| `messages`    | `Message[]`   | required, not empty, first role must be `user` |
| `tools`       | `ToolSpec[]`  | optional. `inputSchema` must be a JSON Schema object |
| `maxTokens`   | number        | default 4096 |
| `temperature` | number        | 0–2 |

`Message` is `{ "role": "user" | "assistant", "content": ContentPart[] }`. A `ContentPart` is one of:

- `{ "type": "text", "text": "..." }`
- `{ "type": "toolUse", "id": "...", "name": "...", "input": { ... } }`
- `{ "type": "toolResult", "toolUseId": "...", "toolName": "...", "content": "...", "isError": false }`
- `{ "type": "image", "mediaType": "image/png", "data": "<base64>" }`

### Images

`ImageInput` is `{ mediaType, data }`. `mediaType` is `image/png`, `image/jpeg`, `image/gif`, or `image/webp`; `data` is base64 without a `data:` prefix, at most 7 000 000 characters. A prompt takes at most 8 images; anything else fails with `-32602`. The images join the user message as `image` content parts. Anthropic gets `image` blocks, OpenAI-compatible endpoints get `image_url` data URLs, and Ollama gets the message's `images` list. For context compression an image counts as about 1600 tokens.

## Errors

| code     | meaning |
|----------|---------|
| `-32700` | Parse error (the reply id is `null`) |
| `-32600` | Invalid request (bad `jsonrpc`, bad id, duplicate in-flight id, or the runtime is shutting down) |
| `-32601` | Unknown method |
| `-32602` | Invalid params |
| `-32603` | Internal error |
| `-32001` | Provider error. `data` is a `ProviderError` |
| `-32002` | Protocol version mismatch |
| `-32003` | Not initialized |
| `-32004` | No workspace is open |
| `-32005` | Tool error |
| `-32800` | Request cancelled |

`ProviderError`:

```json
{ "kind": "rateLimited", "message": "...", "status": 429, "retryable": true }
```

`kind` is one of `invalidConfig`, `invalidRequest`, `auth`, `permissionDenied`, `notFound`, `rateLimited`, `overloaded`, `server`, `network`, `timeout`, `cancelled`, `protocol`. `retryable` is `true` for `rateLimited`, `overloaded`, `server`, `network`, and `timeout`. The runtime does not retry a stream by itself.

## Compatibility

The client and runtime must agree on `protocolVersion`. Additive changes (new optional fields, new methods, new event types) do not change the version. Clients must ignore unknown event types and unknown result fields.
