# Project memory

Notes task-x keeps about this project. Edit or delete them freely.
- The VS Code extension (extension/) is being dropped as of 2026-11-12: don't restore or edit it; protocol changes update docs/protocol.md + runtime tests only, and status/colour/icon UI goes in runtime/src/cli.rs render().
- Approval settings UI lives only in the installed VS Code extension build (~/.vscode/extensions/its-ash.task-x-0.1.28, setting `task-x.agent.allowTools`/`denyTools` → runtime `policy.allow`/`deny`); its source is not in this repo. MCP tool names are `mcp__<server>__<tool>`; allow/deny accept a trailing `*` (agent/policy.rs tool_matches).
