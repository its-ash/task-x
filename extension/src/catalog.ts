/**
 * Agents, plugins, and plugin skills for the `/` menu, plus the actions behind it: create a skill or
 * an agent, open one to view it, and install, enable, or disable a Claude Code plugin.
 */
import { execFile } from 'node:child_process';
import { existsSync, mkdirSync, readdirSync, readFileSync, statSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import * as vscode from 'vscode';
import { findClaude } from './claudeCode.js';
import { runtimeEnv } from './runtime.js';

const MAX_ITEMS = 200;
const MAX_FILE_BYTES = 64 * 1024;
const CLI_TIMEOUT_MS = 20_000;
/** Names a new skill or agent may have: they become folder and file names. */
const NAME = /^[a-z0-9][a-z0-9-]{0,63}$/;
/** Plugin ids as Claude Code writes them: `name` or `name@marketplace`. */
const PLUGIN_ID = /^[\w.-]{1,128}(@[\w.-]{1,128})?$/;

export interface CatalogEntry {
  name: string;
  description: string;
  source: string;
  /** The file to open with "view". */
  path?: string;
}

export interface PluginEntry {
  id: string;
  version: string;
  enabled: boolean;
  path: string;
  /** The plugin's MCP servers, in Claude Code's `mcpServers` format. */
  mcpServers?: unknown;
}

/** MCP servers of enabled plugins, from the last `plugins()` call; read without waiting for the CLI. */
let pluginMcp: { plugin: string; servers: unknown }[] = [];
/** MCP servers of plugins the runtime found outside Claude Code (`~/.task-x/plugins`, `.task-x/plugins`, `task-x.plugins.dirs`). */
let localPluginMcp: { plugin: string; servers: unknown }[] = [];
export const pluginMcpServers = () => [...pluginMcp, ...localPluginMcp.filter((l) => !pluginMcp.some((p) => p.plugin === l.plugin))];

/** Keeps the MCP servers of enabled non-Claude plugins. Workspace plugins count only in a trusted workspace. */
export function setLocalPluginMcp(list: { name: string; enabled: boolean; source: string; mcpServers?: unknown }[], trusted: boolean): void {
  localPluginMcp = list
    .filter((p) => p.enabled && p.source !== 'claude' && p.mcpServers && typeof p.mcpServers === 'object' && (trusted || p.source !== 'workspace'))
    .map((p) => ({ plugin: p.name, servers: p.mcpServers }));
}

export interface Catalog {
  agents: CatalogEntry[];
  plugins: PluginEntry[];
  /** Skills that come with plugins; Claude Code calls them as `/plugin:skill`. */
  pluginSkills: CatalogEntry[];
}

/** `name` and `description` from a Markdown file's front matter. */
function frontMatter(file: string): { name?: string; description?: string } {
  try {
    if (statSync(file).size > MAX_FILE_BYTES) return {};
    const m = /^---\r?\n([\s\S]*?)\r?\n---/.exec(readFileSync(file, 'utf8'));
    if (!m) return {};
    const field = (k: string) => new RegExp(`^${k}:\\s*(.+)$`, 'm').exec(m[1])?.[1].trim().replace(/^["']|["']$/g, '');
    return { name: field('name'), description: field('description') };
  } catch {
    return {};
  }
}

const dirs = (d: string) => {
  try {
    return readdirSync(d, { withFileTypes: true });
  } catch {
    return [];
  }
};

/** Agent files (`*.md`) in a folder. */
function agentsIn(dir: string, source: string): CatalogEntry[] {
  return dirs(dir)
    .filter((e) => e.isFile() && e.name.endsWith('.md'))
    .map((e) => {
      const file = path.join(dir, e.name);
      const fm = frontMatter(file);
      return { name: fm.name || e.name.slice(0, -3), description: fm.description ?? '', source, path: file };
    });
}

/** Skill folders (`<name>/SKILL.md`) in a folder. */
function skillsIn(dir: string, source: string, prefix: string): CatalogEntry[] {
  return dirs(dir)
    .filter((e) => e.isDirectory() && existsSync(path.join(dir, e.name, 'SKILL.md')))
    .map((e) => {
      const file = path.join(dir, e.name, 'SKILL.md');
      const fm = frontMatter(file);
      return { name: `${prefix}${fm.name || e.name}`, description: fm.description ?? '', source, path: file };
    });
}

const run = (cli: string, args: string[]) =>
  new Promise<string>((resolve, reject) =>
    execFile(cli, args, { timeout: CLI_TIMEOUT_MS, maxBuffer: 4 << 20, env: runtimeEnv() }, (e, out) => (e ? reject(e) : resolve(out))),
  );

/** Installed Claude Code plugins (`claude plugin list --json`); empty without the CLI. */
export async function plugins(): Promise<PluginEntry[]> {
  const cli = findClaude();
  if (!cli) return [];
  try {
    const list = JSON.parse(await run(cli, ['plugin', 'list', '--json'])) as unknown;
    if (!Array.isArray(list)) return [];
    const out = list
      .filter((p): p is { id: string; version?: string; enabled?: boolean; installPath?: string; mcpServers?: unknown } => typeof p?.id === 'string' && PLUGIN_ID.test(p.id))
      .map((p) => ({ id: p.id, version: String(p.version ?? ''), enabled: p.enabled !== false, path: typeof p.installPath === 'string' ? p.installPath : '', mcpServers: p.mcpServers }))
      .slice(0, MAX_ITEMS);
    pluginMcp = out.filter((p) => p.enabled && p.mcpServers).map((p) => ({ plugin: p.id.split('@')[0], servers: p.mcpServers }));
    return out;
  } catch {
    return [];
  }
}

/** Agents from the project and the user folder, and the agents and skills of enabled plugins. */
export async function catalog(root: string | undefined): Promise<Catalog> {
  const home = homedir();
  const installed = await plugins();
  const agents = [
    ...(root ? [...agentsIn(path.join(root, '.claude', 'agents'), '.claude'), ...agentsIn(path.join(root, '.github', 'agents'), '.github')] : []),
    ...agentsIn(path.join(home, '.claude', 'agents'), '~/.claude'),
  ];
  const pluginSkills: CatalogEntry[] = [];
  for (const p of installed.filter((x) => x.enabled && x.path)) {
    const name = p.id.split('@')[0];
    agents.push(...agentsIn(path.join(p.path, 'agents'), name).map((a) => ({ ...a, name: `${name}:${a.name}` })));
    pluginSkills.push(...skillsIn(path.join(p.path, 'skills'), name, `${name}:`));
  }
  return { agents: agents.slice(0, MAX_ITEMS), plugins: installed, pluginSkills: pluginSkills.slice(0, MAX_ITEMS) };
}

async function askName(kind: string): Promise<string | undefined> {
  const v = await vscode.window.showInputBox({
    title: `New ${kind}`,
    prompt: `Name of the ${kind}: lowercase letters, digits, and hyphens`,
    validateInput: (s) => (NAME.test(s.trim()) ? undefined : 'Use lowercase letters, digits, and hyphens (up to 64).'),
  });
  return v?.trim();
}

/** Writes a file that does not exist yet and opens it; an existing one only opens. */
async function scaffold(file: string, text: string): Promise<void> {
  if (!existsSync(file)) {
    mkdirSync(path.dirname(file), { recursive: true });
    writeFileSync(file, text, { flag: 'wx' });
  }
  await vscode.window.showTextDocument(vscode.Uri.file(file));
}

/** Creates `.claude/skills/<name>/SKILL.md` in the project and opens it. */
export async function newSkill(root: string | undefined): Promise<void> {
  if (!root) return void vscode.window.showWarningMessage('task-x: open a folder to add a skill to it.');
  const name = await askName('skill');
  if (!name) return;
  await scaffold(
    path.join(root, '.claude', 'skills', name, 'SKILL.md'),
    `---\nname: ${name}\ndescription: Say what this skill does and when to use it.\n---\n\n# ${name}\n\nSteps for the agent to follow.\n`,
  );
}

/** Creates `.claude/commands/<name>.md` in the project and opens it. */
export async function newCommand(root: string | undefined): Promise<void> {
  if (!root) return void vscode.window.showWarningMessage('task-x: open a folder to add a command to it.');
  const name = await askName('command');
  if (!name) return;
  await scaffold(
    path.join(root, '.claude', 'commands', `${name}.md`),
    `---\ndescription: Say what /${name} does.\nargument-hint: [what to pass]\n---\n\nDo the task for: $ARGUMENTS\n`,
  );
}

/** Creates `.claude/agents/<name>.md` in the project and opens it. */
export async function newAgent(root: string | undefined): Promise<void> {
  if (!root) return void vscode.window.showWarningMessage('task-x: open a folder to add an agent to it.');
  const name = await askName('agent');
  if (!name) return;
  await scaffold(
    path.join(root, '.claude', 'agents', `${name}.md`),
    `---\nname: ${name}\ndescription: Say what this agent does and when the lead should delegate to it.\ntools: Read, Grep, Glob, Edit, Bash\nmodel: inherit\n---\n\nYou are the ${name} agent. Describe its job, its limits, and what it reports back.\n`,
  );
}

interface Marketplace {
  name: string;
  owner: 'task-x' | 'claude';
  plugins: { name: string; description?: string | null; version?: string | null }[];
}

interface RpcLike {
  request<T>(method: string, params?: unknown): Promise<T>;
}

const describe = (e: unknown) => (e instanceof Error ? e.message : String(e));

/** Adds a marketplace (git URL, owner/repo, or folder) through the runtime. */
export async function addMarketplace(client: RpcLike): Promise<boolean> {
  const source = await vscode.window.showInputBox({
    title: 'Add a plugin marketplace',
    prompt: 'A git URL, owner/repo on GitHub, or an absolute folder with .claude-plugin/marketplace.json',
    validateInput: (s) => (s.trim() && !s.trim().startsWith('-') ? undefined : 'Give a git URL, owner/repo, or folder.'),
  });
  if (!source) return false;
  try {
    const m = await vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title: `task-x: adding ${source.trim()}…` }, () =>
      client.request<Marketplace>('plugins/marketplaceAdd', { source: source.trim() }),
    );
    void vscode.window.showInformationMessage(`task-x: added the marketplace ${m.name} (${m.plugins.length} plugins).`);
    return true;
  } catch (e) {
    void vscode.window.showErrorMessage(`task-x: could not add the marketplace: ${describe(e)}`);
    return false;
  }
}

/**
 * Installs a plugin without the Claude Code CLI: pick one from a marketplace (task-x's or Claude
 * Code's), or give `name@marketplace`, a git URL, `owner/repo`, or a folder. Returns whether a plugin
 * was installed.
 */
export async function installPlugin(client: RpcLike, root: string | undefined): Promise<boolean> {
  let markets: Marketplace[] = [];
  try {
    markets = await client.request<Marketplace[]>('plugins/marketplaces');
  } catch {
    markets = [];
  }
  type Item = vscode.QuickPickItem & { spec?: string; action?: 'add' | 'manual' };
  const items: Item[] = [
    ...markets.flatMap((m) =>
      m.plugins.map((p) => ({ label: p.name, description: `${m.name}${p.version ? ` · v${p.version}` : ''}`, detail: p.description ?? undefined, spec: `${p.name}@${m.name}` })),
    ),
    { label: '$(add) Add a marketplace…', action: 'add' },
    { label: '$(cloud-download) Install from a git URL, owner/repo, or folder…', action: 'manual' },
  ];
  const pick = await vscode.window.showQuickPick(items, { title: 'task-x: Install a plugin', matchOnDescription: true, matchOnDetail: true });
  if (!pick) return false;
  if (pick.action === 'add') return (await addMarketplace(client)) && installPlugin(client, root);
  const spec =
    pick.spec ??
    (await vscode.window.showInputBox({ title: 'Install a plugin', prompt: 'name@marketplace, a git URL, owner/repo, or an absolute folder', validateInput: (s) => (s.trim() ? undefined : 'Required') }))?.trim();
  if (!spec) return false;
  const scope = root
    ? (await vscode.window.showQuickPick([{ label: 'For me', description: '~/.task-x/plugins', scope: 'user' }, { label: 'For this project', description: '.task-x/plugins (shared through the repository)', scope: 'workspace' }], { title: `Install ${spec}` }))?.scope
    : 'user';
  if (!scope) return false;
  try {
    const r = await vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title: `task-x: installing ${spec}…` }, () =>
      client.request<{ name: string; version?: string }>('plugins/install', { spec, scope }),
    );
    void vscode.window.showInformationMessage(`task-x: installed ${r.name}${r.version ? ` v${r.version}` : ''}.`);
    return true;
  } catch (e) {
    void vscode.window.showErrorMessage(`task-x: could not install ${spec}: ${describe(e)}`);
    return false;
  }
}

/** Updates or removes a plugin task-x installed. */
export async function managePlugin(client: RpcLike, name: string, source: string): Promise<boolean> {
  if (source !== 'user' && source !== 'workspace') {
    void vscode.window.showInformationMessage(`task-x: ${name} is managed by ${source === 'claude' ? 'Claude Code (claude plugin)' : 'your settings'}.`);
    return false;
  }
  const act = await vscode.window.showQuickPick(['Update', 'Uninstall'], { title: `task-x: ${name}` });
  if (!act) return false;
  try {
    await client.request(act === 'Update' ? 'plugins/update' : 'plugins/uninstall', { name, scope: source });
    void vscode.window.showInformationMessage(`task-x: ${act === 'Update' ? 'updated' : 'removed'} ${name}.`);
    return true;
  } catch (e) {
    void vscode.window.showErrorMessage(`task-x: ${describe(e)}`);
    return false;
  }
}

/** Enables or disables an installed plugin. Returns false for an id that is not well formed. */
export async function setPlugin(id: string, enable: boolean): Promise<boolean> {
  const cli = findClaude();
  if (!cli || !PLUGIN_ID.test(id)) return false;
  try {
    await run(cli, ['plugin', enable ? 'enable' : 'disable', id]);
    return true;
  } catch (e) {
    vscode.window.showErrorMessage(`task-x: could not ${enable ? 'enable' : 'disable'} ${id}: ${e instanceof Error ? e.message : String(e)}`);
    return false;
  }
}

/** Opens a plugin's README, else reveals its folder. */
export async function viewPlugin(p: PluginEntry): Promise<void> {
  const readme = ['README.md', 'readme.md', '.claude-plugin/plugin.json'].map((f) => path.join(p.path, f)).find((f) => existsSync(f));
  if (readme) await vscode.window.showTextDocument(vscode.Uri.file(readme));
  else if (p.path) await vscode.commands.executeCommand('revealFileInOS', vscode.Uri.file(p.path));
}
