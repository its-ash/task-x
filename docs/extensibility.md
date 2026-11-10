# Skills, commands, agents, plugins, hooks, and MCP

You can extend task-x without rebuilding it. task-x reads the same instruction, skill, command, agent, and plugin files as GitHub Copilot and Claude Code, so most repositories need no changes.

## Instruction files

task-x sends one file to the model: `CLAUDE.md` at the workspace root, else `.github/copilot-instructions.md`. `CLAUDE.md` is a symlink to `.github/copilot-instructions.md` in task-x projects, so they are one file. Other instruction files are not read. See [context.md](context.md#instruction-precedence).

## Skills

A skill is a Markdown file with optional front matter:

```markdown
---
name: db-migration
description: Write a reversible database migration
triggers: migration, schema change     # optional: apply when the request mentions these
patterns: migrations/**, **/*.sql      # optional: apply when the request touches these files
tools: read_file, create_file, run_tests   # optional: limit the non-read tools while active
params: table                          # optional: values for /db-migration table=users
---
Create a migration for {{table}} with up and down steps…
```

Locations, lowest precedence first (a later file with the same name wins):

- `~/.claude/skills/`, `~/.copilot/skills/`, `~/.task-x/skills/`
- `.claude/skills/`, `.github/skills/`, `.task-x/skills/` in the workspace
- `.github/prompts/<name>.prompt.md` (Copilot prompt files)

Each skill is `<name>.md` or `<name>/SKILL.md`.

There are three ways a skill applies:

- **Explicit:** start the message with `/name` (for example `/release`).
- **Automatic:** a trigger word or a file pattern matches the request. Up to 3 skills apply at once.
- **On demand:** a skill without triggers or patterns (the usual Copilot and Claude `SKILL.md`) appears by name and description in an index. The model calls `read_skill` to load it when it fits.

Run **task-x: List Skills** to see what task-x found.

task-x comes with one on-demand skill, `vector-design`. It covers logos, icons, and other SVG graphics: the model writes the SVG, then checks it with the `render_svg` tool, which returns a PNG and a list of problems. A skill file named `vector-design` replaces it.

To write a skill from a prompt, run `/create-skill <what the skill does>`. The agent reads the project, writes `.claude/skills/<name>/SKILL.md`, and copies the skill folder to `.github/skills/<name>/`, so Claude Code and Copilot both read it.

## Commands

A command is a Claude Code slash command: a Markdown file that you run by typing `/name`. It never applies by itself, and the model does not see it in the skill index.

```markdown
---
description: Fix a GitHub issue
argument-hint: [issue-number]
allowed-tools: Bash(git:*), Read, Edit
---
Fix issue #$1. Details: $ARGUMENTS
```

- Locations: `~/.claude/commands/`, `~/.copilot/commands/`, `~/.task-x/commands/`, `.claude/commands/` and `.task-x/commands/` in the workspace, and `commands/` in a plugin.
- A subfolder makes a namespace: `commands/frontend/test.md` is `/frontend:test`.
- `$ARGUMENTS` is the text after `/name`, and `$1` to `$9` are its words.
- `allowed-tools` limits the edit and command tools while the command runs. Read tools stay available.
- In the chat, the `/` menu lists commands first, with their argument hint. **New command…** creates one.

## Custom agents

A custom agent is a specialist subagent with its own instructions, tools, and model, in Claude Code's agent format:

```markdown
---
name: code-reviewer
description: Review changed code for defects. Use after a change.
tools: Read, Grep, Glob
model: inherit
maxIterations: 20
---
You are a code reviewer. Report each defect with file:line and severity.
```

- Locations, lowest precedence first: plugin `agents/` folders (named `<plugin>:<name>`), `~/.claude/agents/`, `~/.copilot/agents/`, `~/.task-x/agents/`, then `.claude/agents/`, `.github/agents/`, and `.task-x/agents/` in the workspace.
- `tools` uses Claude Code names (`Read`, `Grep`, `Glob`, `Edit`, `Write`, `Bash`, `WebFetch`, `WebSearch`) or task-x tool names. An agent whose tools only read is read-only. `mode: read` or `mode: write` sets it directly.
- `model` is a model id for the current provider, or `sonnet`, `opus`, or `haiku` on Anthropic. Other providers ignore those aliases.
- **Delegation:** in Agent and Team mode, the lead sees the agents by name and description and calls `run_subagent { agent, objective }`. Several run at the same time, and each writing agent works in its own git worktree. In Team mode, each `run_plan` step can name an agent.
- **Direct use:** start the message with `@name` to run the whole task as that agent. The `@` menu lists agents at the start of the message, and the `/` menu lists them too.
- Each subagent has a lane in the chat with the agent's name, model, live steps, and Stop, Apply, or Discard.
- **From a prompt:** `/create-agent <what the agent does>` makes the agent read the project and write `.claude/agents/<name>.md` for Claude Code and `.github/agents/<name>.agent.md` for Copilot. Copilot's tool names (`read`, `search`, `edit`, `execute`, `web`) also work in task-x.

## Plugins

A plugin is a folder in the Claude Code plugin layout that bundles skills, commands, agents, and MCP servers:

```text
my-plugin/
  .claude-plugin/plugin.json   { "name", "version", "description" }
  skills/<name>/SKILL.md
  commands/<name>.md
  agents/<name>.md
  .mcp.json                    { "mcpServers": { … } }
```

task-x loads plugins from these places:

- the plugins that Claude Code installed (`claude plugin install`), enabled or disabled as `enabledPlugins` in `~/.claude/settings.json` says;
- `~/.task-x/plugins/<name>/` and `.task-x/plugins/<name>/` in the workspace;
- the folders in `task-x.plugins.dirs`.

A plugin's skills and commands are `/plugin:name`, and `/name` also works when no other plugin has that name. Its agents are `plugin:name`. Turn a plugin off with `task-x.plugins.disabled`, or for one workspace with `.task-x/plugins.json` (`{ "disabled": ["name"] }`). In the `/` menu, the toggle on a plugin row uses `claude plugin enable|disable` for Claude Code plugins, and `task-x.plugins.disabled` for the others.

A plugin's `hooks/hooks.json` (Claude Code's hook format) runs too. Hooks of a workspace plugin run only in a trusted workspace. Its MCP servers (`.mcp.json`) start like the others.

### Marketplaces and installs

task-x installs plugins without the Claude Code CLI. Run **task-x: Install Plugin…** or pick **Install plugin…** in the `/` menu:

- Pick a plugin from a marketplace. task-x reads its own marketplaces (`~/.task-x/marketplaces/`) and the ones Claude Code added (`~/.claude/plugins/marketplaces/`).
- Or give `name@marketplace`, a git URL, `owner/repo` on GitHub, or an absolute folder.
- Choose **For me** (`~/.task-x/plugins/`) or **For this project** (`.task-x/plugins/`, which you can commit).

**task-x: Add Plugin Marketplace…** adds a marketplace from a git URL, `owner/repo`, or a folder. A marketplace is a repository with `.claude-plugin/marketplace.json`, in Claude Code's format. The gear on a plugin row updates or uninstalls a plugin that task-x installed. git runs with an argument list, with repository hooks turned off, and https or ssh URLs only.

## Hooks

Hooks run your commands at agent events. Each hook gets a JSON object on stdin: `{ event, tool, filePath, budgetUsed, budgetLimit, severity, payload }`, plus the fields Claude Code hooks read (`hook_event_name`, `tool_name`, `tool_input`, `session_id`, `cwd`, `prompt`). `filePath` is the tool's `path` (or `from`) input. `budgetUsed` is the task's tokens so far, and `budgetLimit` its token budget (null when there is none). `severity` is set for `afterTool` when the call failed: `retryable`, `permanent`, or `blocked`. `payload` holds the event data (task id, tool input, result).

```json
{ "hooks": [
  { "event": "beforeTool", "command": ["./scripts/guard.sh"], "matcher": "^(delete_file|run_tests)$", "timeoutMs": 10000 },
  { "event": "afterEdit", "command": ["npx", "prettier", "--write", "."] }
] }
```

| Event | When | Can block |
|---|---|---|
| `userPromptSubmit` | The user's prompt, before the task starts | Yes: the task does not start |
| `beforeTask` / `afterTask` | The task starts / ends | `beforeTask` |
| `beforeTool` / `afterTool` | Around every tool call | `beforeTool` |
| `beforeEdit` / `afterEdit` | Around a call that writes or deletes | `beforeEdit` |
| `beforeValidation` / `afterValidation` | Around tests, builds, and linters | `beforeValidation` |
| `stop` / `subagentStop` | The agent (or a subagent) is about to finish | Yes: it goes on, with the reason as feedback (at most 3 times) |
| `preCompact` | The history is about to be compressed | No |
| `notification` | The agent waits for an approval or an answer | No |

Claude Code's names work too: `PreToolUse`, `PostToolUse`, `UserPromptSubmit`, `Stop`, `SubagentStop`, `PreCompact`, `Notification`, `SessionStart`, and `SessionEnd`. A `matcher` matches the task-x tool name or its Claude Code name (`Bash`, `Edit`, `Write`, `Read`, `Grep`, `Glob`).

- A blocking event's hook that exits with a non-zero code blocks. Its output goes back to the model.
- A hook can also answer with JSON on stdout:
  - `{ "decision": "block", "reason": "…" }` blocks with that reason (also `{ "continue": false, "stopReason" }`).
  - `{ "additionalContext": "…" }` gives the model text before its next step. For `userPromptSubmit` and `beforeTask`, plain stdout counts as context.
  - `{ "updatedInput": { … } }` from a `beforeTool` hook replaces the tool's input.
  - `{ "decision": "approve" }` from a `beforeTool` hook skips the approval prompt. Only a trusted hook can approve: one from `task-x.hooks` or from a plugin you installed. A workspace hook can block but never grant, and `task-x.agent.denyTools` always wins.
  - Claude Code's `hookSpecificOutput` (`permissionDecision`, `permissionDecisionReason`, `updatedInput`, `additionalContext`) is read the same way.
- Hooks run before the permission policy asks the user, so the user is only asked about a call no hook blocks.
- Hooks from settings and `.task-x/hooks.json` are argument arrays and never run through a shell. Plugin hooks are Claude Code command lines: they run with `sh -c` (`cmd /C` on Windows) with `CLAUDE_PLUGIN_ROOT` set, and only exit code 2 blocks, as in Claude Code.

Configure hooks in the `task-x.hooks` setting, or in `.task-x/hooks.json`. task-x loads `.task-x/hooks.json` only when VS Code trusts the workspace.

## MCP servers

```json
"task-x.mcpServers": {
  "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": { "GITHUB_TOKEN": "…" } }
}
```

A trusted workspace can also add servers in `.task-x/mcp.json` (`{ "servers": { … } }`).

A server is local or remote:

```json
"task-x.mcpServers": {
  "chrome-devtools": { "command": "npx", "args": ["-y", "chrome-devtools-mcp@latest"] },
  "figma": { "type": "http", "url": "http://127.0.0.1:3845/mcp" },
  "remote": { "type": "http", "url": "https://mcp.example.com/mcp", "headers": { "Authorization": "Bearer …" } }
}
```

- `type` is `stdio` (default with a `command`), `http` (streamable HTTP; default with only a `url`), or `sse` (the older HTTP+SSE transport).
- `headers` go with every request. They are never logged, and never sent over plain `http` to a host that is not loopback.
- A server that answers 401 or 403 needs sign-in. task-x shows **Sign In**, or run **task-x: Sign In to MCP Server…**:
  - task-x finds the server's authorization server (from the `WWW-Authenticate` header or the `.well-known` metadata), registers itself as a client, and opens the sign-in page in your browser. The code comes back to a one-time listener on 127.0.0.1, with PKCE.
  - The tokens are kept in VS Code's secret storage, never in settings or logs. An expired token is refreshed before the server starts.
  - **task-x: Sign Out of MCP Server…** forgets them. A server that does not support OAuth still works with a token in `headers`, which wins over a sign-in.

### Servers from Claude and Copilot

task-x also reads the MCP servers you already set up in other tools (`task-x.mcp.import`, default all three):

| Source | Files |
|---|---|
| `claude` | Claude Desktop `claude_desktop_config.json`; Claude Code `~/.claude.json` (user servers and this project's servers); the workspace `.mcp.json` |
| `vscode` | GitHub Copilot in VS Code: the user `mcp.json`, the `mcp.servers` setting, and the workspace `.vscode/mcp.json` |
| `copilotCli` | `~/.copilot/mcp-config.json` |

- Later rows of the list override earlier ones with the same server name, workspace files override user files, and `task-x.mcpServers` overrides all of them.
- Workspace files are read only when VS Code trusts the workspace.
- `${workspaceFolder}`, `${userHome}`, `${env:NAME}`, and Claude's `${NAME}` and `${NAME:-default}` are filled in. A server that uses a VS Code `${input:…}` prompt is skipped; copy it into `task-x.mcpServers` with the value filled in.
- The task-x Output channel lists what was imported and what was skipped, and why.
- With the Claude Code engine, Claude Code starts its own configured servers. task-x adds the Copilot servers and `task-x.mcpServers` to the run.

### How tools are called

- task-x starts each server and lists its tools, following pagination.
- Tools appear to the model as `mcp__<server>__<tool>`, separate from the built-in tools.
- With `task-x.agent.approveAll` (the default), the model uses every MCP tool with no prompt, in every chat mode. Only `task-x.agent.denyTools` refuses one.
- With `approveAll` off, every call needs approval (permission class `network`) unless the tool is in `task-x.agent.allowTools`, and Ask and Plan modes do not offer MCP tools.
- Inputs are checked against the tool's schema (required fields and types) before the call. Each call has a timeout (`timeoutMs`, default 60 s) and can be cancelled.

Run **task-x: Show MCP Servers** to see which servers are running, their tools, and their errors.

## Old folder names

Before the rename to task-x, these locations used `.anvil`: `.anvil/skills/`, `~/.anvil/skills/`, `.anvil/hooks.json`, `.anvil/mcp.json`, and `.anvilignore`. They still work. When both exist, the `.task-x` location wins (hooks and MCP read only the first file found, `.task-x` first). `.taskxignore` works like `.anvilignore`.
