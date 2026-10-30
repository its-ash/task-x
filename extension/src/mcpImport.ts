// Reads MCP servers that Claude (Claude Code, Claude Desktop) and Copilot (VS Code, Copilot CLI)
// already have configured, so task-x models can use them too. No `vscode` import: plain Node tests it.
import { readFileSync } from 'node:fs';
import { join } from 'node:path';

/** A server in the runtime's `mcp/configure` shape (the same keys Claude and VS Code use). */
export interface McpServer {
  type?: string;
  command?: string;
  args?: string[];
  env?: Record<string, string>;
  cwd?: string;
  url?: string;
  headers?: Record<string, string>;
  timeoutMs?: number;
  disabled?: boolean;
}

export type ImportSource = 'claude' | 'vscode' | 'copilotCli';
export const IMPORT_SOURCES: ImportSource[] = ['claude', 'vscode', 'copilotCli'];

/** One config file to read: the key that holds the servers, and whether it belongs to the workspace. */
interface ConfigFile {
  source: ImportSource;
  label: string;
  path: string;
  /** Picks the server map out of the parsed file. */
  pick: (json: Record<string, unknown>) => unknown;
  workspace: boolean;
}

export interface ImportOptions {
  home: string;
  platform: NodeJS.Platform;
  /** `%APPDATA%` on Windows. */
  appData?: string;
  /** The VS Code user folder (holds `mcp.json` and `settings.json`). */
  vscodeUserDir?: string;
  /** VS Code `mcp.servers` setting (older Copilot setup). */
  vscodeSettingServers?: unknown;
  root?: string;
  trusted: boolean;
  sources: readonly ImportSource[];
  env: Record<string, string | undefined>;
  read?: (path: string) => string | undefined;
  /** Servers of installed Claude Code plugins (`claude plugin list --json`), by plugin id. */
  pluginServers?: { plugin: string; servers: unknown }[];
}

export interface Imported {
  /** Every imported server, later sources overriding earlier ones of the same name. */
  servers: Record<string, McpServer>;
  /** Servers not from Claude; Claude Code loads its own, so only these are passed to it. */
  nonClaude: Record<string, McpServer>;
  /** Where each server came from, such as "Claude Code" or "VS Code workspace". */
  origins: Record<string, string>;
  /** One line per file read and per server skipped. */
  notes: string[];
}

const MAX_SERVERS = 50;
const NAME = /^[\w.-]{1,64}$/;
/** Stands for the `mcp.servers` setting in the file list. */
const SETTING = '<mcp.servers setting>';

function claudeDesktopConfig(o: ImportOptions): string {
  if (o.platform === 'darwin') return join(o.home, 'Library', 'Application Support', 'Claude', 'claude_desktop_config.json');
  if (o.platform === 'win32') return join(o.appData ?? join(o.home, 'AppData', 'Roaming'), 'Claude', 'claude_desktop_config.json');
  return join(o.home, '.config', 'Claude', 'claude_desktop_config.json');
}

const obj = (v: unknown): Record<string, unknown> | undefined => (v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : undefined);

/** The files to read, from the most general to the most specific. */
export function configFiles(o: ImportOptions): ConfigFile[] {
  const files: ConfigFile[] = [];
  const add = (source: ImportSource, label: string, path: string | undefined, pick: ConfigFile['pick'], workspace = false) => {
    if (path && o.sources.includes(source) && (!workspace || o.trusted)) files.push({ source, label, path, pick, workspace });
  };
  add('claude', 'Claude Desktop', claudeDesktopConfig(o), (j) => j.mcpServers);
  // ~/.claude.json: user servers, then this project's servers.
  add('claude', 'Claude Code', join(o.home, '.claude.json'), (j) => ({ ...obj(j.mcpServers), ...(o.root ? obj(obj(obj(j.projects)?.[o.root])?.mcpServers) : undefined) }));
  add('copilotCli', 'Copilot CLI', join(o.home, '.copilot', 'mcp-config.json'), (j) => j.mcpServers);
  add('vscode', 'VS Code', o.vscodeUserDir && join(o.vscodeUserDir, 'mcp.json'), (j) => j.servers);
  // The older Copilot setup keeps servers in the `mcp.servers` setting.
  if (o.vscodeSettingServers) add('vscode', 'VS Code settings', SETTING, (j) => j.servers);
  add('claude', 'Claude Code project', o.root && join(o.root, '.mcp.json'), (j) => j.mcpServers, true);
  add('vscode', 'VS Code workspace', o.root && join(o.root, '.vscode', 'mcp.json'), (j) => j.servers, true);
  return files;
}

/**
 * Fills `${workspaceFolder}`, `${userHome}`, `${env:NAME}`, and Claude's `${NAME}` / `${NAME:-default}`.
 * Returns undefined when a value asks for an `${input:...}` prompt, which only VS Code can show.
 */
export function expand(s: string, o: Pick<ImportOptions, 'home' | 'root' | 'env'>): string | undefined {
  if (/\$\{input:/.test(s)) return undefined;
  return s.replace(/\$\{([^}]+)\}/g, (all, key: string) => {
    if (key === 'workspaceFolder') return o.root ?? all;
    if (key === 'userHome') return o.home;
    const env = /^env:(\w+)$/.exec(key)?.[1];
    if (env) return o.env[env] ?? '';
    const m = /^(\w+)(?::-(.*))?$/.exec(key);
    return m ? (o.env[m[1]] ?? m[2] ?? '') : all;
  });
}

/** One server entry, checked and with variables filled in; a string says why it was skipped. */
export function normalize(raw: unknown, o: Pick<ImportOptions, 'home' | 'root' | 'env'>): McpServer | string {
  const s = obj(raw);
  if (!s) return 'not an object';
  const str = (v: unknown) => (typeof v === 'string' ? v : undefined);
  const strMap = (v: unknown) => Object.fromEntries(Object.entries(obj(v) ?? {}).filter((e): e is [string, string] => typeof e[1] === 'string'));
  let needsInput = false;
  const x = (v: string) => {
    const r = expand(v, o);
    if (r === undefined) needsInput = true;
    return r ?? '';
  };
  const type = str(s.type);
  const command = str(s.command);
  const url = str(s.url) ?? str(s.serverUrl);
  if (!command && !url) return 'no command or url';
  if (type && !['stdio', 'http', 'sse', 'streamable-http'].includes(type)) return `unsupported type "${type}"`;
  const out: McpServer = {};
  if (type) out.type = type === 'streamable-http' ? 'http' : type;
  if (command) out.command = x(command);
  if (Array.isArray(s.args)) out.args = s.args.filter((a): a is string => typeof a === 'string').map(x);
  const env = strMap(s.env);
  if (Object.keys(env).length) out.env = Object.fromEntries(Object.entries(env).map(([k, v]) => [k, x(v)]));
  if (str(s.cwd)) out.cwd = x(str(s.cwd)!);
  if (url) out.url = x(url);
  const headers = strMap(s.headers);
  if (Object.keys(headers).length) out.headers = Object.fromEntries(Object.entries(headers).map(([k, v]) => [k, x(v)]));
  if (s.disabled === true) out.disabled = true;
  if (needsInput) return 'it asks for an ${input:...} value; copy it into task-x.mcpServers with the value filled in';
  return out;
}

function parse(text: string): Record<string, unknown> | undefined {
  try {
    return obj(JSON.parse(text));
  } catch {
    // VS Code's mcp.json allows comments and trailing commas.
    try {
      const stripped = text.replace(/("(?:\\.|[^"\\])*")|\/\/[^\n]*|\/\*[\s\S]*?\*\//g, (_, q: string | undefined) => q ?? '').replace(/,(\s*[}\]])/g, '$1');
      return obj(JSON.parse(stripped));
    } catch {
      return undefined;
    }
  }
}

const readText = (p: string): string | undefined => {
  try {
    return readFileSync(p, 'utf8');
  } catch {
    return undefined;
  }
};

/** Reads every enabled source. Missing files are skipped quietly; broken ones are noted. */
export function importServers(o: ImportOptions): Imported {
  const read = o.read ?? readText;
  const servers: Record<string, McpServer> = {};
  const nonClaude: Record<string, McpServer> = {};
  const origins: Record<string, string> = {};
  const notes: string[] = [];
  const take = (label: string, source: ImportSource, map: unknown) => {
    const entries = Object.entries(obj(map) ?? {});
    let n = 0;
    for (const [name, raw] of entries) {
      if (!NAME.test(name) || name === 'task-x') {
        notes.push(`${label}: skipped "${name}" (name must be 1-64 letters, digits, ".", "_", or "-", and not "task-x")`);
        continue;
      }
      const s = normalize(raw, o);
      if (typeof s === 'string') {
        notes.push(`${label}: skipped "${name}": ${s}`);
        continue;
      }
      if (!(name in servers) && Object.keys(servers).length >= MAX_SERVERS) {
        notes.push(`${label}: skipped "${name}": more than ${MAX_SERVERS} servers`);
        continue;
      }
      servers[name] = s;
      origins[name] = label;
      if (source === 'claude') delete nonClaude[name];
      else nonClaude[name] = s;
      n++;
    }
    if (n) notes.push(`${label}: ${n} server${n === 1 ? '' : 's'}`);
  };
  // Plugin servers come first, so the user's own files override them by name. Claude Code loads them itself.
  if (o.sources.includes('claude')) for (const p of o.pluginServers ?? []) take(`Plugin ${p.plugin}`, 'claude', p.servers);
  for (const f of configFiles(o)) {
    const text = f.path === SETTING ? JSON.stringify({ servers: o.vscodeSettingServers }) : read(f.path);
    if (text === undefined) continue;
    const json = parse(text);
    if (!json) {
      notes.push(`${f.label}: could not parse ${f.path}`);
      continue;
    }
    take(f.label, f.source, f.pick(json));
  }
  return { servers, nonClaude, origins, notes };
}

/** A server in Claude Code's `mcpServers` format, for `claude/run`. Disabled servers are left out. */
export function forClaude(servers: Record<string, McpServer>): Record<string, McpServer> {
  const out: Record<string, McpServer> = {};
  for (const [name, s] of Object.entries(servers)) {
    if (s.disabled || name === 'task-x') continue;
    const { timeoutMs: _t, disabled: _d, cwd: _c, ...rest } = s;
    out[name] = { ...rest, type: s.type ?? (s.url && !s.command ? 'http' : 'stdio') };
  }
  return out;
}
