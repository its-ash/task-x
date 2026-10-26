import * as vscode from 'vscode';
import { homedir } from 'node:os';
import { dirname } from 'node:path';
import { SECTION } from './config.js';
import { IMPORT_SOURCES, type ImportSource, type Imported, type McpServer, forClaude, importServers } from './mcpImport.js';

/** Servers imported from Claude and Copilot, per `task-x.mcp.import`. Workspace files need a trusted workspace. */
export function importedMcp(ctx: vscode.ExtensionContext): Imported {
  const c = vscode.workspace.getConfiguration(SECTION);
  const sources = c.get<string[]>('mcp.import', IMPORT_SOURCES).filter((s): s is ImportSource => (IMPORT_SOURCES as string[]).includes(s));
  return importServers({
    home: homedir(),
    platform: process.platform,
    appData: process.env.APPDATA,
    // globalStorageUri is <User>/globalStorage/<extension id>.
    vscodeUserDir: dirname(dirname(ctx.globalStorageUri.fsPath)),
    vscodeSettingServers: sources.includes('vscode') ? vscode.workspace.getConfiguration('mcp').get('servers') : undefined,
    root: vscode.workspace.workspaceFolders?.[0]?.uri.fsPath,
    trusted: vscode.workspace.isTrusted,
    sources,
    env: process.env,
  });
}

/** Servers for the task-x runtime: imported ones first, `task-x.mcpServers` overriding them by name. */
export function runtimeMcpServers(ctx: vscode.ExtensionContext): { servers: Record<string, McpServer>; notes: string[] } {
  const { servers, notes } = importedMcp(ctx);
  return { servers: { ...servers, ...vscode.workspace.getConfiguration(SECTION).get<Record<string, McpServer>>('mcpServers', {}) }, notes };
}

/** Servers to add to a Claude Code run: Claude Code loads its own, so only the Copilot ones and `task-x.mcpServers`. */
export function claudeMcpServers(ctx: vscode.ExtensionContext): Record<string, McpServer> {
  const { nonClaude } = importedMcp(ctx);
  return forClaude({ ...nonClaude, ...vscode.workspace.getConfiguration(SECTION).get<Record<string, McpServer>>('mcpServers', {}) });
}
