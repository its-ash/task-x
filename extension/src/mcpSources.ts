import * as vscode from 'vscode';
import { homedir } from 'node:os';
import { dirname } from 'node:path';
import { SECTION } from './config.js';
import { pluginMcpServers } from './catalog.js';
import { IMPORT_SOURCES, type ImportSource, type Imported, type McpServer, forClaude, importServers } from './mcpImport.js';

/** `task-x.mcp.disabled`: servers the user turned off in the chat's MCP chip. */
export const disabledMcp = (): string[] => vscode.workspace.getConfiguration(SECTION).get<string[]>('mcp.disabled', []).filter((s) => typeof s === 'string');

export async function setMcpDisabled(name: string, off: boolean): Promise<void> {
  const list = new Set(disabledMcp());
  if (off) list.add(name);
  else list.delete(name);
  await vscode.workspace.getConfiguration(SECTION).update('mcp.disabled', [...list].sort(), vscode.ConfigurationTarget.Global);
}

/** Marks the servers the user turned off as disabled, so the runtime does not start them. */
const withDisabled = (servers: Record<string, McpServer>): Record<string, McpServer> => {
  const off = new Set(disabledMcp());
  return Object.fromEntries(Object.entries(servers).map(([n, s]) => [n, off.has(n) ? { ...s, disabled: true } : s]));
};

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
    pluginServers: pluginMcpServers(),
  });
}

/** Servers the extension itself serves, such as other extensions' tools (#14). */
let extensionServers: Record<string, McpServer> = {};
export function setExtensionServers(servers: Record<string, McpServer>): void {
  extensionServers = servers;
}

/** Servers for the task-x runtime: imported ones first, `task-x.mcpServers` overriding them by name. */
export function runtimeMcpServers(ctx: vscode.ExtensionContext): { servers: Record<string, McpServer>; notes: string[] } {
  const { servers, notes } = importedMcp(ctx);
  return { servers: withDisabled({ ...extensionServers, ...servers, ...vscode.workspace.getConfiguration(SECTION).get<Record<string, McpServer>>('mcpServers', {}) }), notes };
}

/** Where each server comes from, for the chat's MCP chip; `task-x.mcpServers` wins by name. */
export function mcpOrigins(ctx: vscode.ExtensionContext): Record<string, string> {
  const own = Object.keys(vscode.workspace.getConfiguration(SECTION).get<Record<string, McpServer>>('mcpServers', {}));
  return { ...Object.fromEntries(Object.keys(extensionServers).map((n) => [n, 'VS Code extensions'])), ...importedMcp(ctx).origins, ...Object.fromEntries(own.map((n) => [n, 'task-x settings'])) };
}

/**
 * Names for Claude Code to block: each server the user turned off, and for a plugin's server also the
 * name Claude Code gives it (`plugin_<plugin>_<server>`, so its tools are `mcp__plugin_…__<tool>`).
 */
export function claudeDisabledMcp(ctx: vscode.ExtensionContext): string[] {
  const off = disabledMcp();
  if (!off.length) return [];
  const origins = importedMcp(ctx).origins;
  const names = off.flatMap((n) => {
    const plugin = /^Plugin ([\w.-]+)$/.exec(origins[n] ?? '')?.[1];
    return plugin ? [n, `plugin_${plugin}_${n}`] : [n];
  });
  return [...new Set(names)].filter((n) => /^[\w.-]{1,64}$/.test(n));
}

/** Servers to add to a Claude Code run: Claude Code loads its own, so only the Copilot ones and `task-x.mcpServers`. */
export function claudeMcpServers(ctx: vscode.ExtensionContext): Record<string, McpServer> {
  const { nonClaude } = importedMcp(ctx);
  return forClaude(withDisabled({ ...extensionServers, ...nonClaude, ...vscode.workspace.getConfiguration(SECTION).get<Record<string, McpServer>>('mcpServers', {}) }));
}
