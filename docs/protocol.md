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

### `provider/models` (request)

Params: `{ "provider": ProviderConfig }`. Result: `{ "models": ModelInfo[] }` (sorted by id, max 500), where `ModelInfo` is `{ id, contextWindow?, capabilities, details? }`:

- `contextWindow`: the input window the endpoint reports (Anthropic `max_input_tokens`; OpenAI-compatible `context_length`, `max_context_length`, `context_window`, `max_model_len` (vLLM), or `top_provider.context_length` (OpenRouter); Ollama `/api/show`), else the built-in estimate for Anthropic and OpenAI. Absent when unknown.
- `capabilities`: any of `tools`, `vision`, `thinking`, `embedding`, in that order. Ollama reports them in `/api/show`; OpenRouter in `supported_parameters` and `architecture.input_modalities`. For Anthropic and the official OpenAI API they come from the model family. Empty when unknown.
- `details`: size and quantization of an Ollama model, such as `8.2B · Q4_K_M`.

For Ollama, the runtime asks `/api/show` about each installed model, 8 at a time, for the first 100 models.

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

Params: `{ level?: "file"|"module", focus?: string, depth?: number, maxNodes?: number, importsOnly?: boolean }`. Result: `{ nodes, edges, total, stats, skippedDirs, skippedDirCount }`.

- `skippedDirs` is `[{ path, reason }]` (at most 200, sorted by path): the folders the index does not walk into because they are build output, dependencies, or caches. A folder is skipped for its name (`node_modules`, `target`, `dist`, `build`, `out`, …), for a marker file that tools write into their output (`CACHEDIR.TAG`, `.rustc_info.json`, `CMakeCache.txt`, `pyvenv.cfg`, `.nojekyll`, a `_nuxt`/`_next`/`_astro` folder, …), or because a `tsconfig.json` next to it names it as `outDir`. A folder that a build makes after the scan is found on its first change notice. `skippedDirCount` is the full count. The workspace `stats` carry the same `skippedDirs` and `skippedDirCount`.

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
| `agent/run` | request (streams `agent/event`) | `{ prompt, images?, provider, chatMode: "ask"\|"plan"\|"agent"\|"team", taskId?, editor?, policy?, limits?, routes?, fallbacks?, roleProviders?, prices?, helper? }`. `helper` is a `ProviderConfig` for a small, cheap model: `summarize_file` uses it, and so do research subagents when `roleProviders` has no research entry; unset, both use the task's model. `images` is `ImageInput[]` (see [Images](#images)) → summary `{ taskId, status, text, changedFiles, operations, validations, validationRan, skippedChecks, usage, metrics, model, iterations, toolCalls, error }` |
| `agent/event` | notification (runtime → client) | `{ taskId, event }`. Event types: `started`, `model`, `iteration`, `textDelta`, `assistantMessage`, `retry`, `modelSwitched`, `compacted`, `toolStarted`, `toolFinished`, `approvalRequested`, `approvalResolved`, `fileChanged`, `steered`, `hook`, `subagent`, `subagentStarted`, `subagentFinished`, `planStarted`, `stepSkipped`, `merged`, `bestOfDone`, `usage`, `warning`, `error`, `done`, `wakeupScheduled`, `wakeupFired`, `wakeupCancelled`, `sleeping`, `awake`, `timelineSnapshot`. See [Team mode](#team-mode), [Wake-ups and waiting](#wake-ups-and-waiting), and [Timeline and export](#timeline-and-export). A `run_command` call with `background: true` gets two `toolFinished` events: one at once, with the result `{ background: true, command, message }`, then one with `background: true` and the real output when the command ends. The model gets that output as a user message before its next step, and the task does not end while background commands run. `agent/cancelTool` stops a background command too. A `run_command` call that is such a code search (see the shell-search hook below) does not start a process: the runtime answers it from the index in the usual command result (`stdout` in grep form), and adds `answeredBy: { tool, input }` and a `note` that name the search tool call to use next time. |
| `agent/cancelTool` | request | `{ taskId, callId }` (from `toolStarted`; a subagent's own task id for its calls) → `true` when a running built-in tool call (or a Claude Code `Bash` call) was stopped. Only that call stops: its process is ended, the model gets "Task has been stopped by user. Continue with the next step…" (for Claude Code, as a user message after the failed result), and the task goes on. |
| `agent/cancelSubagent` | request | `{ taskId }` (a subagent's task id from `subagentStarted`) → `true` when a running subagent was stopped |
| `agent/steer` | request | `{ taskId, text }` → `true` when a running top-level task (`agent/run` or `claude/run`) accepts the message, `false` when no such task is running or its queue (10 messages) is full. The message joins the conversation before the task's next model turn, or before it would end. Each accepted message is reported by a `steered` event `{ text }` when it joins. Text is 1..=100 000 characters after trimming; longer or empty text is an invalid-params error. |
| `agent/bestOf` | request (streams `agent/event`) | `{ prompt, providers: ProviderConfig[2..4], taskId?, editor?, policy?, limits?, prices? }` → `{ taskId, candidates, costUsd }`. See [Best of N](#best-of-n). |
| `worktrees/apply` / `worktrees/discard` | request | `{ name }` → the merge result `{ applied, files, changed, operation }` / `{ discarded }`. Apply goes through the edit log, so the change is pending review and can be undone. |
| `agent/requestApproval` | **request runtime → client** | `{ taskId, callId, tool, permission, input, preview }` → `{ decision: "allow"\|"allowSession"\|"deny" }`. No answer, or an error, counts as deny. |
| `docs/add` | request (sends `docs/progress`) | `{ name, url, maxPages?, embeddings? }` → `{ name, url, pages, chunks, semantic, skipped }`. Crawls the pages under `url` (same host, same folder; default 40 pages, at most 300; public addresses only, as for `web_fetch`), reduces them to text, and saves passages under `<dataDir>/docs/<name>.json`. With `embeddings` (a `ProviderConfig` for an Ollama or OpenAI-compatible embedding model) the passages get vectors; without it, search uses keywords. `docs/progress` is `{ name, pages, total }`. |
| `docs/list` / `docs/remove` | request | → `[{ name, url, pages, chunks, indexedMs, semantic }]` / `{ name }` → `{ removed }`. |
| `provider/complete` | request | `{ provider, system?, prompt, maxTokens? }` → `{ text, usage }`. One model call with no tools (the Source Control commit message). `maxTokens` defaults to 1024 (16 to 8192). |
| `mcp/catalog` | request | → `{ prompts: [{ server, name, description, arguments }], resources: [{ server, uri, name, description, mimeType }] }`: the prompt templates and resources of the running MCP servers (read at start when the server has those capabilities). `arguments` lists required ones first. |
| `mcp/getPrompt` / `mcp/readResource` | request | `{ server, name, arguments? }` / `{ server, uri }` → `{ text }` (at most 60,000 characters; binary resource contents are named, not included). |
| `mcp/elicit` | **request runtime → client** | `{ server, params: { message, requestedSchema } }` → `{ action: "accept", content }` \| `{ action: "decline" }` \| `{ action: "cancel" }`. Sent when an MCP server sends `elicitation/create` while it works (stdio, HTTP, and SSE). The runtime offers the `elicitation` capability to servers; no answer or an error counts as `cancel`. |
| `config/sandbox` | request | `{ enabled }` → `{ enabled, available }`. Turns the OS sandbox for `run_command` and the validation tools on or off from the next command (`initialize.sandbox` sets the start value). `available` says whether this machine can apply it. `claude/run` takes `sandbox` (default true) for Claude Code's `sandbox.enabled`. |
| `agent/editSteer` | request | `{ taskId, old, text }` → `true` when a message sent with `agent/steer` was still waiting: the first one equal to `old` becomes `text`, or is removed when `text` is empty. `false` when the task already read it or is not running. |
| `agent/askUser` | **request runtime → client** | `{ taskId, callId, questions: [{ id, text, kind: "radio"\|"checkbox", options }] }` → `{ answers: { [questionId]: string[] } \| null, other?: { [questionId]: string } }`. `other` holds the text the user typed for a question; it is trimmed and cut to 1000 characters, and a radio question can have `other` in place of a selected option. Sent when the model calls `ask_user` (top-level tasks only; 1 to 4 questions, 2 to 8 distinct options each), and when Claude Code (`claude/run`) calls `AskUserQuestion`. Claude Code gets the answers as `updatedInput.answers` (question text → chosen labels, then typed text, joined by ", "), or a denial when the user dismisses them; this question never asks for approval. Radio answers keep one option. `answers: null` means the user dismissed the questions. No answer, or an error, counts as dismissed. Options outside the offered list are dropped. The tool result for the model is `{ answered, answers: [{ question, selected, other? }] }`. Old clients that do not handle the request see a dismissal, so the protocol version is unchanged. |
| `agent/tasks` / `agent/task` / `agent/deleteTask` | request | the saved task list / `{ taskId }` → `{ summary, messages, title }` / `{ taskId }` → bool |
| `tools/list` / `tools/call` | request | the registry with schemas and permission classes / `{ name, input }` → tool result (error code `-32005`, `data` = `{ kind, message }`) |
| `tools/approvals` | request | `{ policy }` → `{ tools, servers }`, for an approvals screen. `tools` is every tool, task-x's own first and then each running MCP server's: `{ name, server?, tool, permission, decision: "allow"\|"ask"\|"deny", why: "denied"\|"allowed"\|"session"\|"mode", rule? }`. `rule` is the `allow` or `deny` entry that decided (`why` `denied` or `allowed`). `session` means the user picked "allow for this session" when it asked, and `mode` means the mode and edit mode decided. MCP tools are checked as network calls, as when they run. `servers` is `{ name, pattern, tools, allowed, asks, denied, allowListed, denyListed }` per server, where `pattern` (`mcp__<server>__*`) is the entry that covers all of its tools. Send the same `policy` as `agent/run`. |
| `tools/setApproval` | request | `{ policy, tool, choice: "allow"\|"ask"\|"deny" }` → `{ allow, deny, coveredBy? }`: the lists after setting `tool` (a tool name or a pattern such as a server's `pattern`). Its own entries leave both lists, and `allow` or `deny` gets it back. Other entries stay. `coveredBy` names another entry that still decides for it: a `deny` pattern over a tool set to allow, or any pattern over a tool set back to ask. The runtime keeps no settings, so the client saves `allow` and `deny` (for example in `task-x.agent.allowTools` and `denyTools`) and sends them in later `policy` objects. A missing `tool` or `choice`, an empty name, or a bare `*` is `-32602`. |
| `edits/list` / `edits/pending` | request | the operation log with diffs / the net unsettled change for each file `{ path, original, current, added, removed, operations }` |
| `edits/keep` / `edits/revert` | request | `{ paths? }` → `{ kept }` / `{ reverted, conflicts }`. A file that changed after the agent's edit is never reverted. |
| `edits/undo` | request | `{ operation }` → `{ restored, conflicts }` |
| `config/hooks` | request | `{ hooks, workspaceTrusted }` → `{ hooks, workspaceHooks }` |
| `mcp/configure` / `mcp/status` | request | `{ servers, workspaceTrusted, import?, disabled? }` → server status / server status. A server is `{ type?: "stdio"\|"local"\|"http"\|"sse", command?, args?, env?, cwd?, url?, headers?, timeoutMs?, disabled? }` (`local` is Copilot CLI's word for `stdio`); unknown fields (from Claude or VS Code files) are ignored. Status entries are `{ name, command, disabled, running, tools, error }`, where `command` is the URL for a remote server. `import` (any of `"claude"`, `"vscode"`, `"copilotCli"`, `"cursor"`; others are `-32602`) also starts the servers the user set up for those tools. The files, from the most general to the most specific (a later one wins for a name): Claude Desktop's `claude_desktop_config.json`, `~/.claude.json` (user servers, then this project's), `~/.copilot/mcp-config.json`, `~/.cursor/mcp.json`, VS Code's and VS Code Insiders' user `mcp.json` and their `mcp.servers` setting, then, with `workspaceTrusted` only, the workspace's `.mcp.json`, `.vscode/mcp.json`, and `.cursor/mcp.json`. After them come `.task-x/mcp.json` (trusted) and then `servers`. Comments and trailing commas are allowed. `${workspaceFolder}`, `${userHome}`, `${env:NAME}`, `${NAME}`, and `${NAME:-default}` are filled in. An entry that needs `${input:…}` is skipped. A registry-style name such as `io.github.ChromeDevTools/chrome-devtools-mcp` becomes `chrome-devtools`. At most 50 servers are imported. `disabled` names are not started, whatever file has them. With `import`, the status also has `imported: { origins, notes }`: where each server came from, and what was renamed or skipped and why. |
| `mcp/discover` | request | the params of `mcp/configure` → `{ servers: [{ name, origin, type, command, args, url, disabled }], notes }`: what `mcp/configure` would start, and from which file, without starting anything. `env` values and `headers` are left out because they hold tokens. |
| `skills/list` / `worktrees/list` | request | the skills found / the task-x worktree names. A skill entry is `{ name, description, kind, triggers, patterns, tools, params, argumentHint?, source, onDemand }`; `kind` is `skill`, `command` (Claude Code slash commands from `commands/` folders), or `prompt` (Copilot prompt files). Plugin skills and commands are named `<plugin>:<name>`. |
| `skills/resolve` | request | `{ prompt, paths? }` → `{ active: [{ name, reason, source, text }] }`: the skills that apply (a leading `/name`, a trigger word, or a file pattern), with `{{param}}` values filled in. For a command, `$ARGUMENTS` and `$1`…`$9` are filled from the text after `/name`. `/name` also finds a plugin's `<plugin>:name` when only one plugin has it. Clients use it to give skills to another agent, such as the Claude Code CLI. |
| `agents/list` | request | → `[{ name, description, tools, writes, model, maxIterations, source }]`: the custom agents (see [Custom agents](#custom-agents)), workspace agents first, then the user's, then plugin agents (`<plugin>:<name>`). |
| `plugins/list` | request | → `[{ name, id, version, description, path, enabled, source, skills, agents, commands, mcpServers? }]`. `source` is `claude` (installed by Claude Code, enabled per `enabledPlugins` in `~/.claude/settings.json`), `user` (`~/.task-x/plugins/`), `workspace` (`.task-x/plugins/`, turned off by `.task-x/plugins.json` `{ disabled }`), or `client`. |
| `plugins/install` / `plugins/uninstall` / `plugins/update` | request | `{ spec, scope?, force? }` / `{ name, scope? }` / `{ name, scope? }`. `spec` is `name@marketplace`, an https or ssh git URL, `owner/repo`, or an absolute folder; `scope` is `user` (default, `~/.task-x/plugins/`) or `workspace` (`.task-x/plugins/`). install → `{ name, path, scope, version, marketplace }`. |
| `plugins/marketplaces` / `plugins/marketplaceAdd` / `plugins/marketplaceUpdate` / `plugins/marketplaceRemove` | request | → `[{ name, path, owner: "task-x"\|"claude", plugins: [{ name, description, version, source }] }]` / `{ source }` → the marketplace / `{ name }` → the marketplace / `{ name }` → `{ removed }`. Only task-x's own marketplaces can be updated or removed. |
| `agent/checkpoints` | request | `{ taskId }` → `[{ index, prompt, messages, operations, createdMs }]`: one per user prompt. `agent/task` also returns `checkpoints` and `forkedFrom`. |
| `agent/rewind` | request | `{ taskId, checkpoint, restoreCode? }` → `{ taskId, checkpoint, messages, undone, conflicts, text }`. Cuts the conversation back to before that prompt and, unless `restoreCode` is false, undoes the edits made since (a file changed after the agent's edit is a conflict and stays). Waits for a running task on the same id. |
| `agent/fork` | request | `{ taskId, checkpoint? }` → `{ taskId, title, messages, forkedFrom }`: a copy under a new id, cut before `checkpoint` when given. The fork owns no edits. |
| `agent/compact` / `agent/cost` | request | `{ taskId }` → `{ pruned, folded, droppedRetrieval, beforeTokens, afterTokens }` / `{ usage, costUsd, unpricedTokens, billedTokens, localTokens, modelCalls, modelMs, toolMs, toolCalls, iterations, prompts, subagents, subagentCostUsd, subagentTokens, compactions }`. |
| `memory/search` / `memory/forget` | request | `{ query, limit? }` → `{ query, entries, results: [{ id, kind, title, text, files, createdMs, score }] }` / `{ id }` → `{ removed }`. See [Memory](#memory-and-the-repository-map). |
| `mcp/authorize` | request | `{ server }` → MCP status. Runs the OAuth sign-in for a remote server (see below). |
| `mcp/openUrl` | **request runtime → client** | `{ server, url }` → `{ opened }`. The client opens the sign-in page. |
| `mcp/authorizeUrl` / `mcp/tokens` | notification (runtime → client) | `{ server, url }` / `{ server, tokens: { accessToken, refreshToken?, expiresAtMs?, clientId, clientSecret?, tokenEndpoint, resource } }`. The client keeps the tokens (SecretStorage) and sends them back as `oauth` on the server in `mcp/configure`. `mcp/status` entries have `authRequired` and `signedIn`. |
| `plugins/configure` | request | `{ dirs?, disabled?, workspaceTrusted? }` → `{ plugins, enabled }`. A workspace plugin's hooks run only with `workspaceTrusted`. `dirs` are extra plugin folders (absolute paths, else `-32602`); `disabled` are plugin names or ids to turn off. The setting applies to every later `agent/run`, `skills/*`, and `agents/list`. |

`policy` is `{ mode, allow, deny, allowAll?, editMode? }`. `allow` and `deny` hold tool names; an entry ending in `*` covers every tool whose name starts with the rest, so `mcp__chrome-devtools__*` approves (or refuses) all of one MCP server's tools. A bare `*` matches nothing (use `editMode: "auto"` or `allowAll` for that). MCP tools are named `mcp__<server>__<tool>`. With `allowAll: true`, every tool call runs with no `agent/requestApproval`, destructive tools included; only `deny` refuses a tool. MCP tools are then offered in every mode, and a read-only mode still refuses other writes. Commands stay in the sandbox.

`editMode` (`"auto"`, `"safe"`, or `"ask"`) replaces `allowAll` and the destructive opt-in in every mode except `readOnly`. `deny` still refuses, and `allow` and session grants still skip the question. `auto` runs every tool with no question. `safe` runs reads, writes, deletes, and commands with no question when the OS sandbox is on (all of them stay inside the workspace, and edits and deletes go to the edit log), and asks (`agent/requestApproval`) before anything that can write or delete beyond the workspace: commands when the sandbox is off, installs, git writes, network calls, files from outside the workspace (`external`), and MCP tools. `ask` asks before every call that is not a read. A client that leaves it out keeps the earlier behaviour. A runtime older than this field rejects it as unknown; the extension ships with its own runtime, so the protocol version is unchanged.

`initialize` also accepts `dataDir` (an absolute path where task files are saved) and `sandbox` (a boolean, default true).

### Token savers

These keep each turn small. All of them are additive: a client that sends nothing new gets the earlier behaviour, except the read defaults and the shortened output, which apply to every client.

- **Prompt caching.** Anthropic requests mark the last tool, the system prompt, and the newest message as cache breakpoints (`cache_control: ephemeral`), so the next turn reads the repeated prefix from the cache. OpenAI caches long repeated prefixes by itself. `Usage` has `cachedTokens` (prompt tokens read from the cache); `inputTokens` counts all prompt tokens, cached ones included. Cost estimates price cache reads at a tenth of the input price on Anthropic and half elsewhere.
- **Shortened command output.** When a command result (`stdout`/`stderr`, also inside `result`) passes 8,000 characters, the model gets the first lines, the error lines with two lines after each, and the last 40 lines, with `… n lines left out …` markers, plus `shortened: true`, `log`, and `note`. The full output is saved for a day under the system temp folder (`task-x-logs/`), and `read_log { log, start?, end?, grep? }` reads it. The `toolFinished` event still carries the whole output (up to its own limit), so the chat shows it all. The MCP server shortens the same way.
- **Repeated reads.** A `read_file`, `read_range`, `read_symbol`, `get_file_outline`, `read_multiple_files`, or `read_log` call whose result equals an earlier call with the same input, while that earlier result is still in the conversation, returns `{ unchanged: true, sameAs, note }` instead of the content.
- **Read defaults.** `read_file` without `offset`, `limit`, or `end` returns the first 300 lines; for a longer file it adds `outline` (`L12-40 kind name` per symbol, at most 80) and a `note`. An explicit `limit` still reads up to 2,000 lines.
- **Batch tools.** `multi_search { searches: [{ pattern, literal?, glob?, caseSensitive? }], limit? }` runs up to 10 searches in one call; `read_symbol` takes `names` (up to 10) besides `name`. A failed search or missing symbol fails alone.
- **Lazy tool list.** With `limits.lazyTools: true`, the model first gets the core tools (reads, searches, edits, commands, `git_diff`, `read_log`, `remember`, `todo_write`), the agent tools (subagents, `ask_user`, skills, `summarize_file`), and the tools an active skill needs, plus `find_tool { names?, query? }`, whose description names every tool not offered yet. `find_tool` loads matching tools for the next step; calling a deferred tool by name loads and runs it at once. MCP tools are deferred too.
- **`render_svg { path, size?, background?, image? }`.** Renders an SVG file locally (resvg) to a PNG and returns a report: `viewBox`, `elements` (count per tag), `colors`, `contentBox` (the drawn area in % of the canvas), `svgSize`, `rendered`, and `warnings` (no viewBox, clipped at the edge, empty, too small, more than 6 colours, `<text>`, raster `<image>`, external links, scripts, `<foreignObject>`). `size` is the long side in pixels (default 512, 16 to 1024); `background` is `white` (default), `black`, `transparent`, or `#rrggbb`. Unless `image` is false, the PNG reaches the model as an `image` content part, like an image read with `read_file`; the chat events carry only the report. The SVG never loads files or URLs. Permission class `read`. The built-in on-demand skill `vector-design` tells the model to write the SVG with the edit tools and check it with `render_svg`, so any provider can make a logo or icon; a model without vision uses `image: false` and the report.
- **`export_png { path, sizes?, background?, out?, overwrite? }`.** Writes `<name>-<size>.png` for each size (default `[512]`, at most 12, each 16 to 4096; the long side) next to the SVG or in `out` (inside the workspace), with a transparent background unless `background` says otherwise. An existing PNG fails with `conflict` unless `overwrite` is true. The files join the edit log as one operation, as binary files: no diff, and undo removes them. Returns `{ changed, operation, from, files: [{ path, size, bytes }] }`. Permission class `write`. The chat's **Export PNG…** button calls it through `tools/call`, and the SVG preview calls `render_svg` the same way. The built-in custom agent `designer` uses both tools (see `agents/list`).
- **`web_search { query, count?, site? }`.** Searches the web (DuckDuckGo's HTML results; no key needed) and returns `{ query, results: [{ title, url, snippet }] }`, at most 20. Permission class `network`, like `web_fetch`.
- **`semantic_search { query, limit?, glob? }`.** Offered when `agent/run` has `embeddings` (a `ProviderConfig` for an embedding model; Anthropic has none). The workspace's text files are cut into 60-line chunks (10 lines overlap) and embedded; vectors are kept by chunk hash under `<dataDir>/semantic/`, so later calls embed only the chunks that changed, and another model starts over. Returns `{ query, chunks, results: [{ path, startLine, endLine, score, text }] }`.
- **`docs_search { query, source?, limit? }`.** Offered when docs were indexed (`docs/add`). Searches one docs set or all of them, by embedding when the set has vectors from the same model, else by keywords. Returns `{ query, results: [{ source, url, title, score, text }] }`. In the chat, `@docs:<name>` tells the model to use it.
- **`summarize_file { path, question? }`.** The helper model (else the task's model) reads up to 120,000 characters of the file with line numbers and returns `{ path, lines, summary, model, partial }`; the file itself never enters the conversation. Its tokens and cost count with the subagents'.
- **Project memory.** `remember { note }` adds one fact to `.task-x/memory.md`, and `remember { content }` rewrites it (at most 6,000 characters in all). Every task (and every Claude Code run) starts with the memory in a `<project_memory>` block.
- **Tokens per tool.** `toolFinished` has `tokens`: about how many tokens the result adds to the conversation (characters ÷ 4).

With a `dataDir`, the edit log (checkpoints) is saved per workspace in `dataDir/checkpoints/` after every change and loaded by `workspace/open`. After a restart, `edits/pending`, `edits/undo`, `edits/keep`, and `edits/revert` work on the earlier edits. When the log has more than 500 operations, the contents of old settled operations are dropped; `edits/undo` on one of them fails with "too old to undo".

### Team mode

`chatMode: "team"` gives the lead agent the edit tools, `run_subagent`, `worktree_apply`, `worktree_discard`, and `run_plan`. It needs a git repository with a commit; otherwise a `warning` event says so and the task runs as one agent.

`run_plan` takes `{ steps: [{ id, role, objective, context?, dependsOn? }], merge? }` (1 to 12 steps, roles `research`, `implementation`, `testing`, `review`, `documentation`). A role is matched case-insensitively and accepts the common aliases a model writes (`Research`, `code`, `docs`, `qa`, `reviewer`); anything else fails the call with `-32602`-style `invalidInput` naming the five roles. The runtime starts every step whose dependencies are done, up to `limits.maxParallel` (default 3, at most 8) at a time. A step gets the reports of its direct dependencies, and a writing step starts from the patches of all its writing ancestors (committed in its worktree, so its own patch holds only its changes). A step whose dependency did not complete is skipped. When all steps end, writing steps are merged into the working tree in dependency order through the edit log. A step that conflicts keeps its worktree.

A step can name a custom `agent` in place of `role` (`{ id, agent, objective }`); the agent's mode decides whether it gets a worktree. `roleProviders: [{ role, provider }]` sets the model for subagents of one role. Events:

| type | fields |
|---|---|
| `planStarted` | `callId`, `maxParallel`, `steps: [{ id, role, agent, objective, dependsOn }]` |
| `subagentStarted` | `name` (worktree name), `taskId`, `role`, `agent` (custom agent name or null), `objective`, `kind`, `model`, `dir` (worktree folder or null), `step` |
| `subagent` | `name`, `taskId`, `event` (the subagent's own event) |
| `subagentFinished` | `name`, `taskId`, `role`, `agent`, `status`, `error`, `costUsd`, `unpricedTokens`, `tokens`, `files`, `added`, `removed`, `worktree` |
| `stepSkipped` | `step`, `reason` |
| `merged` | `step`, `name`, `ok`, `files`, `error` |

### Custom agents

A custom agent is a Markdown file in Claude Code's agent format: `.claude/agents/`, `.github/agents/`, and `.task-x/agents/` in the workspace, `~/.claude/agents/`, `~/.copilot/agents/`, and `~/.task-x/agents/`, and the `agents/` folder of each enabled plugin. Front matter: `name`, `description`, `tools` (Claude Code names such as `Read, Grep, Edit, Bash` map to task-x tools), `mode` (`read` or `write`; without it, an agent whose tools only read is read-only), `model` (a model id, or `sonnet`/`opus`/`haiku` on Anthropic; `inherit` keeps the caller's), and `maxIterations` (1 to 60).

- With agents present, a top-level task in Agent or Team mode gets an `<available_agents>` list, and `run_subagent` and `run_plan` steps take `agent` in place of `role`. An unknown agent fails the call with `invalidInput` naming the agents.
- The subagent gets the agent's instructions in an `<agent>` block, only its tools (plus every read tool), its model, and its turn limit. A writing agent works in its own worktree, as a writing role does.
- A prompt that starts with `@name` (an agent's name) runs the whole task as that agent; `@name` is removed from the request, and the `started` event has `agent`. A read-only agent makes the task read-only.

These fields are additive, so the protocol version is unchanged.

### Subagents in the background and nested subagents

- `run_subagent` takes `background: true`: the call returns at once with `{ background, name, taskId }`, and the subagent's result reaches the lead as a message when it ends. The lead does not finish while background work runs.
- `limits.maxDepth` (default 2, at most 3) sets the levels of subagents: at 2, a writing subagent gets `run_subagent` too and can hand a part of its work to its own subagent, in a worktree of its worktree.

### Memory and the repository map

- A completed top-level task adds `{ id, title, text, files }` to `<dataDir>/memory/<workspace hash>.jsonl` (the newest 1,000 are kept). The `recall { query, limit? }` tool searches it with BM25, with the notes of `.task-x/memory.md`.
- Each top-level task starts with a `<repo_map>` block of at most `limits.repoMapTokens` (default 1,000; 0 = none): the files ranked by PageRank over the project graph (leaning towards the files of the request), each with its main definitions and their lines. The `repo_map { focus?, budgetTokens? }` tool gives a larger one.
- The project graph is saved in `<dataDir>/graphs/<workspace hash>.json` with the engine version and a fingerprint of the indexed files (paths, sizes, times). `workspace/open` reads it back while they match. `graph/project` stats have `cached`.
- `workspace/open` takes `watch: true`: the runtime then looks for changed, new, and deleted files every 2 seconds (one `stat` per known file and folder, and a listing only of folders whose time moved), indexes them once a burst settles, and sends `workspace/progress { phase: "updated", changed, changedCount, by: "watcher" }`.

### Hook events

`config/hooks` accepts the events `userPromptSubmit`, `stop`, `subagentStop`, `preCompact`, and `notification` besides the earlier ones, and Claude Code's names (`PreToolUse`, `Stop`, …). Hooks from `config/hooks` are trusted (they may approve a call); workspace hooks are not. See [extensibility.md](extensibility.md#hooks).

### Best of N

`agent/bestOf` runs the same task as an implementation subagent on each provider at the same time, each in its own worktree. Nothing is merged. Each candidate is `{ name, kind, model, status, report, files, added, removed, validations, costUsd, tokens, dir, patch }` (`patch` is cut at 60000 characters), or `{ name: null, kind, model, status: "failed", error }`. The client applies one with `worktrees/apply` and removes the others with `worktrees/discard`.

### Context compression

`limits.contextWindowTokens` sets the model's window. Without it the runtime asks the provider for the model's window and caches the answer for 5 minutes: Ollama from `/api/show` (`model_info.*.context_length`), Anthropic from `/v1/models` (`max_input_tokens`), OpenAI-compatible endpoints from `/v1/models` (`context_length`, `context_window`, or `max_token_size`). When the endpoint does not report one, it falls back to the built-in estimate for the model (Anthropic: 1M for Claude 4.6 and later, otherwise 200K; OpenAI: 1M for gpt-4.1, 272K for gpt-5, otherwise 128K; Ollama and OpenAI-compatible: 32768). `ChatRequest.contextWindow` passes the window to APIs that take one (Ollama `num_ctx`).

`limits.effort` (`"low"`, `"medium"`, `"high"`, `"xhigh"`, or `"max"`; unset is each model's default) sets the reasoning effort of every model call in the task (`ChatRequest.effort`). Each provider sends it only to models that take it: Anthropic as `output_config.effort` (Claude 4.6 and later, and Opus 4.5; a level the model lacks is lowered, `xhigh` to `high` on the 4.6 models, anything above `high` on Opus 4.5), OpenAI-compatible endpoints as `reasoning_effort` for `gpt-5*` and `o<digit>*` models, and Ollama as `think` for `gpt-oss`. The two upper levels become `high` where the API has only three. Other models ignore it.

Before each model call, the runtime estimates the prompt size (characters ÷ 4, calibrated by the real `inputTokens` of the last response). Above `limits.compactPercent` (default 85, 30 to 95) of the window it compresses in stages and stops as soon as the prompt fits. Repository excerpts from the first request are removed last, and only while the prompt is still above that limit:

1. **Prune** old tool results (all but the last 6 messages; errors are kept): a file read that a later read or edit of the same file replaced, and a result repeated later, become short stubs; an output over 2000 characters keeps its first 1200 and last 400 characters.
2. **Fold** the middle of the history into a progress summary (tool calls, files changed, notes, recent errors).
3. Above 90% of the window, **drop** the repository excerpts added to the first request.

Compressed content starts with `[context compressed]`. Events: `compacted` `{ folded, pruned, droppedRetrieval, beforeTokens, afterTokens, windowTokens }`; and after every model response, `context` `{ usedTokens, windowTokens }`.

### Claude Code engine and the task-x tools

`claude/run` also takes `approveAll` (default false; when true, every tool Claude Code asks about, MCP tools included, is allowed with no `agent/requestApproval`), `images` (`ImageInput[]`, sent with the prompt as image content blocks), `mcpServers` (more servers in Claude Code's `mcpServers` format, added to `--mcp-config`; the name `task-x` is reserved), `effort` (the same levels as `limits.effort`, passed as `--effort`), `subagentModel` (the model of Claude Code's subagents, such as `sonnet` or `haiku`, passed as the `CLAUDE_CODE_SUBAGENT_MODEL` environment variable; unset: the main model), and `disabledMcp` (server names the user turned off; Claude Code gets them as `permissions.deny` rules `mcp__<name>` and as `disabledMcpjsonServers`, so its own copies from `~/.claude.json`, `.mcp.json`, and plugins do not run either).

Claude Code always runs with `sandbox.allowUnsandboxedCommands: false`, so a command cannot leave the sandbox. A `Bash` call with `dangerouslyDisableSandbox: true` is denied at once, without asking the user.

`claude/run` takes `useTaskTools` (default true). The runtime then starts Claude Code with:

- `--mcp-config` that launches the runtime itself as an MCP server: `task-x-runtime --mcp-server --root <workspace>`. It serves the same tool registry (`tools/list`, `tools/call`) over stdio, so Claude's tools are `mcp__task-x__read_file`, `mcp__task-x__replace_text`, and so on, with the same workspace, stale-edit, and sandbox checks.
- `--disallowedTools` for Claude Code's own file tools (Read, Edit, MultiEdit, Write, NotebookEdit, Grep, Glob, LS) and for shell commands that rewrite files (`sed -i`, `perl -i`, `cat >`, `cat <<`, `tee`).
- A `PreToolUse` hook (`task-x-runtime --hook-shell-edit`) that blocks the same rewrites anywhere in a command (after `&&`, in a pipe, in `$(…)`). Inline scripts (`node -e`, `python3 -c`, `python3 - <<EOF`, and the like) are blocked only when they call a file-write API (`writeFile`, `open(…, "w")`, `write_text`, `shutil.*`, `os.remove`, …), so read-only checks run. The same hook blocks code searches that start a command (`grep`, `egrep`, `fgrep`, `rg`, `find`, after an optional `cd DIR &&`, piped only to `sort`, `uniq`, `head`, or `wc -l`); its message names the `search_regex`, `search_files_by_content`, or `find_files` call that does the same search. Searches it cannot read (context lines, `-v`, redirects, `-exec`, a filter on another command's output) run as before. It also blocks `sleep` run as a command (at the start, after `;`, `&&`, `|`, or `&`, in `$(…)`, or in a `do`/`then`/`else` body); its message tells Claude to wait with `mcp__task-x__sleep` (under 2 minutes), `mcp__task-x__wait_until` (until a command succeeds), or `mcp__task-x__schedule_wakeup` (come back later).
- `--allowedTools` for the task-x read tools; the write tools too when the permission mode is `acceptEdits`; the command tools too when Bash is allowed.
- `--append-system-prompt` telling Claude to work on files only through these tools.
- `--agents` with the task-x built-in agents (`designer`), so Claude's `Agent` tool can start them. Their tools are the `mcp__task-x__` versions of the agent's tools plus `read_file`, `read_range`, `find_files`, and `list_files`, and the built-in skills they load (`vector-design`) are written into their prompt. A built-in agent that an agent file replaces is not sent.
- `--permission-prompt-tool mcp__task-x__approve`. When a tool call needs permission, Claude Code calls this MCP tool. The MCP server sends the request to the runtime over a loopback TCP connection (address and a random token in the server's `TASK_X_APPROVAL` environment variable). The runtime asks the client with `agent/requestApproval` (`permission` is the task-x class of the tool) and reports `approvalRequested` and `approvalResolved` events. `allowSession` covers later calls of the same tool in the run. No answer, an error, or a wrong token counts as deny. With `useTaskTools: false`, the MCP server starts with `--no-tools` and serves only this tool.

The first prompt of a session starts with a `<repository_context>` block: the workspace summary and up to 2,500 tokens of index excerpts that match the prompt. A resumed session (`sessionId`) gets excerpts only when the prompt names a file of the index or an identifier it defines (snake_case, camelCase, PascalCase with an inner capital, or in backticks), and no summary: the excerpts stay in the session's history, so every later turn pays for them.

The prompt goes to Claude Code's stdin as a stream-json user message (`--input-format stream-json`). Stdin stays open while the run takes `agent/steer` messages: waiting messages are sent as one more user message, and each is reported by a `steered` event. Stdin also stays open while the run has wake-ups pending (see [Wake-ups and waiting](#wake-ups-and-waiting)): each one that fires is sent as the next user message. When Claude Code has answered every message, none waits, and no wake-up is pending, the runtime closes stdin and the run ends. The summary has the text of the last answer and the summed token counts.

A `Task` or `Agent` call of Claude Code is reported as `subagentStarted` (`taskId` is the tool-use id, `kind` is `claudeCode`, `agent` is the `subagent_type`, `role` is the closest task-x role, `dir` and `step` are null), not as `toolStarted`. Lines with a `parent_tool_use_id` are that subagent's steps: their text and tool calls come as `subagent` events. The call's `tool_result` sends `subagentFinished`. A background subagent (`run_in_background`) and any subagent still open end with the next `result` line.

The MCP server can also be used by any MCP client: `task-x-runtime --mcp-server [--root DIR]`.

### Wake-ups and waiting

Five tools let the model wait without spending a turn on every check:

| Tool | Permission | What it does |
|---|---|---|
| `sleep` | read | `{ seconds, reason? }`: pauses 0.1 to 120 seconds, then returns `{ slept, seconds, reason }`. The turn waits; Stop (`agent/cancelTool`) ends it. A registry tool, so the MCP server serves it too. |
| `schedule_wakeup` | read | `{ delaySeconds: 10..86400, prompt, reason? }` → `{ scheduled, id, dueMs, in, message }` at once. When it is due, the model gets `prompt` as a user message (`<wakeup id="w1">…</wakeup>`). Top-level tasks only. |
| `wait_until` | execute | `{ command, cwd?, match?, intervalSecs?: 2..600 (15), timeoutSecs?: 5..3600 (600), reason? }`. Runs `command` (argv, sandboxed, as `run_command`) every `intervalSecs` until it exits 0, or until its stdout or stderr matches the regular expression `match`, or until `timeoutSecs`. In a task it returns `{ background: true, id, command, givesUpAtMs, message }` at once, and the result `{ met, attempts, elapsedSecs, command, reason, last: { exitCode, timedOut, output }, message }` comes as a user message. Called through the MCP server without a runtime (`--mcp-server` alone), the call waits for that result. |
| `list_wakeups` | read | `{}` → `{ count, pending: [{ id, kind: "wakeup"\|"watch", reason, prompt, createdMs, dueMs, dueInSecs }] }`, soonest first. For a `watch` (a `wait_until` check), `prompt` is the command and `dueMs` the time it gives up. |
| `cancel_wakeup` | read | `{ id }` (`"all"` for every one) → `{ cancelled, pending }`. |

`run_command` refuses `sleep` used to wait, for every provider and MCP client: `sleep` at the start of the command, after `;`, `&&`, `|`, or `&`, in `$(…)`, in a `do`/`then`/`else` body, or in `sh -c '…'`. The call fails with a `Forbidden` error (the agent refuses it before it starts, in the background too) whose message names `sleep`, `wait_until`, and `schedule_wakeup`. A word `sleep` in arguments (`echo sleep`, `cargo test sleep_x`) is not refused. A `wait_until` check command may still contain `sleep`. The Claude Code Bash hook refuses the same commands (see above).

A task has at most 10 wake-ups and checks pending. They live in memory only: they end with the task (cancel, limit, or finish) and when the runtime stops. At startup, a record left `sleeping` is marked `failed` like one left `running`.

When the model ends its turn (no tool calls) while wake-ups are pending and no background command runs, the task does not end: its record gets the status **`sleeping`** and the runtime sends `sleeping`. The task wakes when a wake-up fires or an `agent/steer` message arrives (both join the conversation), sends `awake`, goes back to `running`, and takes another step. When the last pending one is cancelled, the task wakes and ends as usual. Time spent sleeping does not count towards `limits.deadlineSecs`, so a task can wait 30 minutes or more under the default 30-minute deadline.

| Event | Fields |
|---|---|
| `wakeupScheduled` | `wakeup: { id, kind, reason, prompt, createdMs, dueMs }` |
| `wakeupFired` | `id`, `kind`, `reason`; `met` (bool) for a `wait_until` check |
| `wakeupCancelled` | `id`, `by`: `model` (`cancel_wakeup`) or `taskEnded` |
| `sleeping` | `untilMs` (the next one due), `pending` (as in `list_wakeups`) |
| `awake` | `by`: `wakeup`, `message`, `nothingPending`, or `cancelled`; `sleptMs`, `pending` |

Clients show a sleeping task with its own status. The terminal client (`task-x-runtime chat`/`exec`) shows a sleeping task as one magenta line redrawn in place every second: the moon goes through its phases (🌑🌒🌓🌔🌕🌖🌗🌘) and the countdown ticks down, as in `🌒 Sleeping · next wake-up in 29m 58s: <reason> (1 pending)`. Any other output clears the line first. On a stream that is not a terminal (logs, pipes, `TERM=dumb`) it prints one static line, `☾ Sleeping · next wake-up in 30m: …`, instead. The wake-up lines use ⏰ in yellow. There is no colour when `NO_COLOR` is set.

With the Claude Code engine (`claude/run`, `useTaskTools` and the runtime's approval channel), the MCP server also lists `schedule_wakeup`, `list_wakeups`, and `cancel_wakeup`, and forwards these and `wait_until` to the runtime over the approval connection (a line `{ token, op: "tool", name, input }`, answered `{ ok }` or `{ error: { kind, message } }`). The three scheduling tools are in `--allowedTools`; `wait_until` follows the command tools. The system prompt tells Claude to use these tools instead of `sleep` in Bash. When Claude Code has answered every message and wake-ups are pending, the runtime sends `sleeping` and keeps stdin open; a fired wake-up is written to stdin as the next user message (`awake` with `by: "wakeup"`), and an `agent/steer` message wakes it too.

The `sleeping` status and these events are additive: older clients ignore unknown event types, so the protocol version is unchanged.

### Timeline and export

Every top-level task (`agent/run` and `claude/run`) leaves a **timeline** in the project: snapshots of the whole workspace in a hidden git repository, `<workspace>/.task-x/timeline.git`, separate from the project's own `.git`. Snapshots are taken before each prompt (`Before: <prompt>`), after each step whose tool calls may change files (`After: <tools>`; the agent's read-only tools do not count), and at the end (`End: <status>`); a step that changed nothing adds no snapshot. Claude Code runs get the before and end snapshots. Because snapshots read the files themselves, they catch changes from edit tools, commands, Claude Code, and the user alike.

- **What stays out:** files the project's `.gitignore` ignores, dependency and cache folders (`node_modules/`, `target/`, `.venv/`, `.next/`, and similar), files over 5 MB, and `.task-x/timeline.git` and `.task-x/exports` themselves. A restore leaves all of these alone. The repository holds a `.gitignore` of `*`, so the project's git never sees it, and the code index skips it. The user's global git settings (hooks, filters, signing) do not apply to it.
- **Only the last 10 tasks:** when a new task takes its first snapshot, the timelines of all but the 10 most recently active tasks are deleted and their data is removed (`git gc --prune=now`). The task records themselves stay.
- **Failures never stop a task:** if git is missing or a snapshot fails, the task goes on and the first failure is a `warning` event.

| Method | Kind | Params → result |
|---|---|---|
| `timeline/tasks` | request | → `{ tasks: [{ taskId, snapshots, lastMs, lastLabel }], keep: 10 }`, most recently active first. |
| `timeline/list` | request | `{ taskId }` → `{ taskId, snapshots: Snapshot[], keep }`, oldest first. Empty for a task whose timeline was deleted. |
| `timeline/diff` | request | `{ taskId, snapshot, against? }` → `{ snapshot, against, diff, truncated }`: the unified diff (with `--stat`) of that snapshot against the one before it, or against `against`. Cut at 400,000 characters. The first snapshot has an empty diff. |
| `timeline/restore` | request | `{ taskId, snapshot }` → `{ taskId, restored, step, backup, files }`. Puts the workspace back to that snapshot: its files are written, and files added since are removed. The current state is saved first: as a new snapshot (`Before restoring step <n>`) when it differs from the newest one, else the newest one stands for it. Its id is `backup`, and restoring `backup` undoes the restore. Refused (`-32600`) while any task runs. The workspace is reindexed afterwards, and files must be read again before they are edited. |
| `agent/turns` | request | `{ taskId }` → `{ taskId, turns: Turn[] }`: the turns an export can start and end at. `Turn` is `{ n, prompt, createdMs }`, `n` counting from 1. A turn is one user prompt and everything up to the next one (the task's checkpoints). A record without checkpoints has one turn. |
| `agent/export` | request | `{ taskId, path?, from?, to? }` → `{ taskId, path, bytes, from, to, turns }`. Writes turns `from` to `to` (inclusive; default the first and the last) as one HTML file: a header with status and stat cards, a contents list of the turns, the prompts (context blocks folded away), the answers (Markdown, with framed and aligned tables), every tool call with its input and result, a table of the files those turns changed (lines added and removed) with their diffs, and the timeline snapshots taken during those turns. A range outside `1..turns` is `-32602`. The page has no scripts and loads nothing (`Content-Security-Policy: default-src 'none'`), so it opens anywhere, offline; it follows the system's light or dark theme, and a CSS-only switch picks Auto, Light, or Dark. `path` must end in `.html`; a relative one is inside the workspace. Default: `<workspace>/.task-x/exports/<date>-<title>-<taskId>.html`, with `-turns-<from>-<to>` (or `-turn-<n>`) before `.html` for a part of the task. Token and cost figures are always the whole task's. Only a saved task record (`agent/run`) can be exported. |

`Snapshot` is `{ id, taskId, step, label, timeMs, base, files: [{ path, status: "added"|"modified"|"deleted"|"typeChanged" }] }`. `step` 0 is the first snapshot (`base: true`, the state before the task, with no `files`); `files` of a later one are its changes since the snapshot before. The `timelineSnapshot` event `{ snapshot }` reports each new snapshot while a task runs.

In the terminal client: `/timeline` lists the current task's snapshots, `/restore <step>` restores one, and `/export [turns] [file.html]` writes the HTML page and prints its path. Without a range it lists the turns and asks where to start and where to end (Enter keeps the first and the last); a range is `3`, `2-5`, `2-`, `-4`, or `all`. With no terminal it exports every turn. `/approvals` lists the MCP servers with each tool's decision, and task-x's own tools that ask or are refused. `/allow`, `/ask`, and `/deny <name>` change the session's lists: a server name stands for all of its tools, and a tool's short name stands for its full name when only one server has it.

These methods and the event are additive; the protocol version is unchanged.

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

`ImageInput` is `{ mediaType, data }`. `mediaType` is `image/png`, `image/jpeg`, `image/gif`, or `image/webp`; `data` is base64 without a `data:` prefix, at most 7 000 000 characters. A prompt takes at most 8 images; anything else fails with `-32602`. The images join the user message as `image` content parts. Anthropic gets `image` blocks, OpenAI-compatible endpoints get `image_url` data URLs, and Ollama gets the message's `images` list. For context compression an image counts as about 1600 tokens. A tool result that carries an image (`read_file` on an image, `render_svg`) keeps its image data whole: the 60,000-character result limit applies to the rest of the result only.

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
