# Project memory

Notes task-x keeps about this project. Edit or delete them freely.
- The VS Code extension (extension/) is being dropped as of 2026-11-12: don't restore or edit it; protocol changes update docs/protocol.md + runtime tests only, and status/colour/icon UI goes in runtime/src/cli.rs render().
- Approval settings UI lives only in the installed VS Code extension build (~/.vscode/extensions/its-ash.task-x-0.1.28, setting `task-x.agent.allowTools`/`denyTools` → runtime `policy.allow`/`deny`); its source is not in this repo. MCP tool names are `mcp__<server>__<tool>`; allow/deny accept a trailing `*` (agent/policy.rs tool_matches).
- MCP import of other tools' servers (Claude, VS Code, Copilot CLI, Cursor) lives in runtime/src/mcp_import.rs; clients opt in with `mcp/configure { import: [...] }`, the CLI imports by default (`--no-mcp-import` turns it off). The installed extension 0.1.28 has its own older import (out/mcpImport.js) that skips names with `/` and type `local`.
- `mcp__vscode__*` tools come from the extension's bridge (out/lmToolsMcp.js), which runs VS Code's LM tools via vscode.lm.invokeTool. VS Code applies its own approval and errors on top of task-x's policy; servers registered only in VS Code (e.g. chrome-devtools) reach task-x only this way.
