# task-x

task-x is a coding agent for VS Code that works with any model provider. A Rust runtime indexes the repository, builds context, runs tools, and drives the agent loop. A TypeScript extension gives the chat, approvals, diffs, and editor commands.

## Install

Download the extension from the [latest release](https://github.com/its-ash/task-x/releases/latest) and install it in VS Code.

**With make** (from a clone of this repository):

```sh
make add    # download task-x.vsix from the latest release and install it
```

**By hand:**

1. Download `task-x.vsix` from the [latest release](https://github.com/its-ash/task-x/releases/latest).
2. Install it:
   - In a terminal: `code --install-extension task-x.vsix`
   - Or in VS Code: open the Extensions view, click **…** (Views and More Actions), select **Install from VSIX…**, and select the file.
3. Reload the VS Code window.

The `.vsix` holds a runtime built for the machine that published it (now macOS on Apple silicon). On another platform, build it from source (see [Quick start](#quick-start)).

**To use it:**

1. Open the **task-x** view in the activity bar.
2. Click **Add provider…** in the model picker, or run **task-x: Add LLM Provider…**. Set the type, base URL, key, and model, then click **Test connection** and **Save and use**. Or sign in with Anthropic to use Claude Code (`claude auth login`).
3. Type a request and press Enter. To show the agent a screenshot or a design, paste the image into the input, drop it on the input, or click the image button.
4. Review the changed files in the panel above the input: open the diff, then click **Keep** or **Undo**.

## Features

### Chat

- **Agent mode.** The chat always edits files and runs commands in the workspace without asking. Commands always run in the OS sandbox, so they can write only inside the workspace, temp folders, and build caches. Deletes, installs, network calls, and git writes still ask, unless listed in `task-x.agent.allowTools`.
- **Night Mode** (switch in the toolbar): for free or local models that drop or delay requests. When a run does not finish (error, stall, or limit), the last request runs again with a delay that grows from 15 s to 5 min, with no limit on retries. A run with no progress for 10 minutes is stopped and started again; waiting for an approval or an answer does not count. Turning Night Mode on, or reloading the window, starts an unfinished last request again. Stop, New chat, or turning the switch off ends it.
  - While Night Mode is on, the toolbar shows only the Think and Loop models. Auto-commit is always on, so the model picker and Auto-commit hide.
  - *Think* (shown when Night Mode is on): a stronger model, or Claude Code, that reads the code and splits each request into a checklist in `todo.md` at the workspace root (at most 50 items). Unset, or if it fails: the Loop model works on the whole request.
  - *Loop*: the free model that works on the first open item of `todo.md` in each run. When a run completes, task-x checks the item off and starts the next one, until all items are done. A run that does not finish is retried on the same item. Each item is committed as `task-x: Todo n/total: <item>`. You can edit `todo.md` while the loop runs. Unset: the chat model.
- **Refine** (sparkle button beside Send): rewrites the typed prompt so the agent can act on it, with repository excerpts for the prompt, `@` mentions, and the active file. It uses the `task-x.routing` provider for the prompt's task kind, else the active provider model. The result replaces the input; Ctrl+Z (Cmd+Z) restores your text. With Claude Code as the engine, Refine needs a provider model or a routing entry.
- **Auto-commit** (checkbox in the toolbar): when a task ends, commits the files that task changed, with the message `task-x: <first line of your request>`. Other staged or unstaged work in the repository is left alone. A failed commit, such as one blocked by a pre-commit hook, is reported in the chat and does not fail the task.
- **Live edit cards:** each file edit appears in the timeline as it happens, with its `+/-` counts and an expandable diff. Keep or Undo a single edit from its card.
- **Changed-files panel** above the input: open a file's diff, then Keep or Undo it, or Keep or Undo all changes.
- **Model picker:** search, provider tabs, and a *Recent* list of the last five models you used.
- **Streaming Markdown** with syntax highlighting, copy buttons, and a tool timeline. Command output can be copied or opened in an editor.
- **Input:** `@path` mentions for files and folders, `/` for skills, file and selection chips for the active editor (toggle whether they are sent), and queued messages while a task runs.
- **Images:** paste, drop, or attach PNG, JPEG, GIF, or WebP images (up to 8, 5 MB each). They go to the model with the message: Anthropic, OpenAI, and OpenAI-compatible endpoints, Ollama vision models, and Claude Code. The model must support images.
- **Task history** per workspace; stop a running task, or continue it when it reaches the turn limit.
- **Context meter** and automatic compression for long tasks.
- **Busy indicator:** an animated border runs around the input while the agent works.

### Agent work

- **Team mode:** the lead agent plans a dependency graph. Ready steps run as parallel subagents, each writer in its own git worktree. Finished work merges in dependency order through the edit log, so it can be reviewed and undone. Each subagent has a lane in the chat with its role, model, live steps, cost, and Stop, Apply, or Discard.
- **Best of N:** two to four models do the same task in separate worktrees. The Compare panel shows the results side by side, and Keep applies one and discards the rest.
- **Background tasks:** run a task outside the chat. The status bar counts running tasks, and a notification opens the result when it finishes.
- **Review Changes:** a review of your changes, with findings listed in the Problems panel.
- **Token counts:** input and output tokens for each model turn, and the task total. No price is shown; `task-x.pricing` only feeds the internal cost figures.
- **Copy and move files:** `copy_file` and `move_file` bring a file into the workspace from anywhere on the machine (an absolute path such as `~/Downloads/logo.png`), or move files between folders inside it. Reading a file from outside the workspace asks first; the destination is always inside it.
- **Project graph and index:** a project and file dependency graph, Tree-sitter symbols, and an index status button in the toolbar.

### Providers

- **Anthropic, OpenAI, OpenAI-compatible endpoints, and Ollama.** Use **Add LLM Provider…** to set the type, base URL, key, and model, then test the connection. API keys are stored in VS Code SecretStorage.
- **Sign in with Anthropic:** run `claude auth login` in a terminal, then use your Claude account. Chat then runs through Claude Code (`task-x.engine`), and `task-x.claudeCode.useTaskXTools` gives it task-x's tools.
- **Routing and fallback:** pick a model per task kind (`task-x.routing`), and optionally fall back to other providers when one fails (`task-x.fallbackProviders`).

### Context and instructions

- The workspace index follows `.gitignore`, always excludes secrets, and retrieves snippets within a token budget. Exclusions can be added with `task-x.index.exclude`.
- task-x sends one instruction file: `CLAUDE.md`, else `.github/copilot-instructions.md` (in task-x projects they are the same file, through a symlink). It also reads Copilot and Claude skills and `.github/prompts`.

### Safety

- Workspace-only file access. Edits match exactly, detect stale files, and are written atomically with rollback.
- Undo keeps your own concurrent edits. Every agent edit is recorded in a per-workspace edit log, which survives a restart.
- A permission policy (`task-x.agent.allowTools`, `task-x.agent.denyTools`). Destructive tools run only when explicitly allowed. Repository text cannot change the policy.
- Commands always run in a sandbox.

### Extensibility

- **Skills** from `.github/skills`, `.claude/skills`, and `~/.claude/skills`, invoked with `/name`.
- **Hooks** (`task-x.hooks`) and **MCP servers** (`task-x.mcpServers`). Check MCP status with `task-x.mcpStatus`.
- **MCP servers from Claude and Copilot:** task-x also starts the servers you set up in Claude Desktop, Claude Code (`~/.claude.json`, `.mcp.json`), GitHub Copilot in VS Code (`mcp.json`, `.vscode/mcp.json`), and the Copilot CLI, such as Figma and Chrome DevTools. Local (stdio) and remote (HTTP and SSE) servers work. Choose the sources with `task-x.mcp.import`. Remote servers that need a browser sign-in (OAuth) need an `Authorization` header in `task-x.mcpServers` instead.

### Editor commands

| Command | What it does |
|---|---|
| `task-x.openChat` | Open the chat |
| `task-x.newChat`, `task-x.clearChat`, `task-x.openTask`, `task-x.showTasks` | Start, clear, reopen, or list chats |
| `task-x.askAgent` | Ask about the selection |
| `task-x.explainSelection`, `task-x.fixSelection`, `task-x.refactorSelection`, `task-x.generateTests` | Act on the selection |
| `task-x.explainError` | Explain a diagnostic |
| `task-x.reviewChanges` | Review your changes |
| `task-x.continueTask`, `task-x.cancel` | Continue or stop the current task |
| `task-x.runInBackground`, `task-x.bestOf` | Run a task in the background, or best of N |
| `task-x.undoLast`, `task-x.undoAll`, `task-x.keepAll` | Undo or keep agent edits |
| `task-x.selectProvider`, `task-x.addProvider`, `task-x.testConnection` | Choose and test providers |
| `task-x.setApiKey`, `task-x.clearApiKey`, `task-x.signIn`, `task-x.signOut` | Manage credentials |
| `task-x.inspectContext`, `task-x.previewContext`, `task-x.reindex` | Inspect the context and index |
| `task-x.showGraph`, `task-x.showFileGraph` | Show the project graph |
| `task-x.listSkills`, `task-x.mcpStatus` | List skills; show MCP status |
| `task-x.showLogs`, `task-x.clearLogs`, `task-x.restartRuntime` | Logs panel and runtime |
| `task-x.copySelection`, `task-x.clearFindings` | Copy the selection; clear review findings |

The Logs panel (`task-x Logs`) shows extension, runtime, and agent logs, filterable by level, source, and text.

### Status

The status of each phase is in [ROADMAP.md](ROADMAP.md). Some parts, such as Team mode against real models and the Logs panel inside VS Code, have been tested only with mock providers so far.

## Layout

```
runtime/     Rust runtime (task-x-runtime): JSON-RPC server, providers, context, tools, agent
extension/   VS Code extension: chat view, approvals, provider dialog, commands
docs/        Protocol, providers, context, security, extensibility
```

## Requirements

- Rust 1.85 or later
- Node.js (see `.nvmrc`)
- VS Code 1.100 or later
- git (for worktree subagents and auto-commit)

## Quick start

```sh
make run      # build the debug runtime and the extension, then open an Extension Development Host
```

In the development window:

1. Open the **task-x** view in the activity bar.
2. Click **Add provider…** in the model picker, or run **task-x: Add LLM Provider…**. Pick the type, base URL, key, and model, then **Test connection** and **Save and use**.
3. Choose **Ask**, **Plan**, or **Agent** next to the input and send a request.
4. Review the changed files in the panel above the input: open the diff, then **Keep** or **Undo**.

## Build, test, package, release

```sh
make build                                        # release runtime → extension/bin, compile extension
cargo test --manifest-path runtime/Cargo.toml     # Rust unit, protocol, and performance tests
npm --prefix extension test                       # client and end-to-end tests against the built runtime
npm --prefix extension run package                # build a .vsix (bundles the runtime in extension/bin)
make install                                      # build the .vsix and install it into VS Code
make deploy                                       # build, push main, and publish the .vsix on GitHub
make add                                          # install the .vsix of the latest GitHub release
```

`make deploy` needs the GitHub CLI (`gh auth login`). It publishes `task-x-<version>.vsix` and `task-x.vsix` on the release `v<version>`, where the version comes from `extension/package.json`. If that release exists, its files are replaced. To publish a new version, increase `version` first.

The tests use mock providers and local git repositories. They do not need API keys or network access.

## Documentation

- [Runtime protocol](docs/protocol.md)
- [Providers](docs/providers.md)
- [Workspace context](docs/context.md)
- [Permissions and security](docs/security.md)
- [Skills, instructions, hooks, and MCP](docs/extensibility.md)
