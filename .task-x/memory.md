# Project memory

Notes task-x keeps about this project. Edit or delete them freely.

- The VS Code extension (extension/) was restored on 2026-11-14 from git (it had been dropped in 16c6518 on 2026-11-12); the user wants options in the extension, with the CLI keeping its slash commands. Protocol changes update docs/protocol.md, extension/src/protocol.ts, and tests; terminal UI stays in runtime/src/cli.rs render().
- Extension build: `make ext` (release runtime copied into extension/bin/, then .vsix), `make ext-install` (installs into VS Code), `make ext-run` (Extension Development Host). Tests: `npm --prefix extension test` (e2e tests use extension/bin/task-x-runtime first, so refresh it after runtime changes or they test a stale binary).
- Extension screens for runtime session features live in extension/src/sessionTools.ts (HTML export via agent/turns + agent/export, timeline/*, tools/approvals + tools/setApproval, agent/active); chat slash commands /export /timeline /restore /approvals /agents-active go through ChatViewProvider.screenCommand.
- Approval settings: `task-x.agent.allowTools`/`denyTools` → runtime `policy.allow`/`deny`. MCP tool names are `mcp__<server>__<tool>`; allow/deny accept a trailing `*` (agent/policy.rs tool_matches).
- MCP import of other tools' servers (Claude, VS Code, Copilot CLI, Cursor) lives in runtime/src/mcp_import.rs; clients opt in with `mcp/configure { import: [...] }`, the CLI imports by default (`--no-mcp-import` turns it off). The extension still has its own older import (src/mcpImport.ts, mcpSources.ts) that skips names with `/` and type `local`.
- `mcp__vscode__*` tools come from the extension's bridge (src/lmToolsMcp.ts), which runs VS Code's LM tools via vscode.lm.invokeTool. VS Code applies its own approval and errors on top of task-x's policy; servers registered only in VS Code (e.g. chrome-devtools) reach task-x only this way.
