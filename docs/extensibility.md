# Skills, instructions, hooks, and MCP

You can extend task-x without rebuilding it. task-x reads the same instruction and skill files as GitHub Copilot and Claude Code, so most repositories need no changes.

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

## Hooks

Hooks run your commands at agent events. Each hook gets a JSON object on stdin: `{ event, tool, filePath, budgetUsed, budgetLimit, severity, payload }`. `filePath` is the tool's `path` (or `from`) input. `budgetUsed` is the task's tokens so far, and `budgetLimit` its token budget (null when there is none). `severity` is set for `afterTool` when the call failed: `retryable`, `permanent`, or `blocked`. `payload` holds the event data (task id, tool input, result).

```json
{ "hooks": [
  { "event": "beforeTool", "command": ["./scripts/guard.sh"], "matcher": "^(delete_file|run_tests)$", "timeoutMs": 10000 },
  { "event": "afterEdit", "command": ["npx", "prettier", "--write", "."] }
] }
```

- Events: `beforeTask`, `beforeTool`, `afterTool`, `beforeEdit`, `afterEdit`, `beforeValidation`, `afterTask`.
- A `before*` hook that exits with a non-zero code blocks the action. Its output goes back to the model.
- A hook runs before the permission policy decides, so a hook can block but never grant: an approved call still has to pass every hook, and the user is only asked to approve a call no hook blocks.
- Commands are argument arrays and never run through a shell.

Configure hooks in the `task-x.hooks` setting, or in `.task-x/hooks.json`. task-x loads `.task-x/hooks.json` only when VS Code trusts the workspace.

## MCP servers

```json
"task-x.mcpServers": {
  "github": { "command": "npx", "args": ["-y", "@modelcontextprotocol/server-github"], "env": { "GITHUB_TOKEN": "…" } }
}
```

A trusted workspace can also add servers in `.task-x/mcp.json` (`{ "servers": { … } }`).

- task-x starts each server over stdio and lists its tools, following pagination.
- Tools appear to the model as `mcp__<server>__<tool>`, separate from the built-in tools.
- Every call needs approval (permission class `network`), unless the tool is in `task-x.agent.allowTools`.
- Ask and Plan modes do not offer MCP tools.
- Inputs are checked against the tool's schema (required fields and types) before the call. Each call has a timeout (`timeoutMs`, default 60 s) and can be cancelled.

Run **task-x: Show MCP Servers** to see which servers are running, their tools, and their errors.

## Old folder names

Before the rename to task-x, these locations used `.anvil`: `.anvil/skills/`, `~/.anvil/skills/`, `.anvil/hooks.json`, `.anvil/mcp.json`, and `.anvilignore`. They still work. When both exist, the `.task-x` location wins (hooks and MCP read only the first file found, `.task-x` first). `.taskxignore` works like `.anvilignore`.
