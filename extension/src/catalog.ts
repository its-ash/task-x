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
    return list
      .filter((p): p is { id: string; version?: string; enabled?: boolean; installPath?: string } => typeof p?.id === 'string' && PLUGIN_ID.test(p.id))
      .map((p) => ({ id: p.id, version: String(p.version ?? ''), enabled: p.enabled !== false, path: typeof p.installPath === 'string' ? p.installPath : '' }))
      .slice(0, MAX_ITEMS);
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

/** Creates `.claude/agents/<name>.md` in the project and opens it. */
export async function newAgent(root: string | undefined): Promise<void> {
  if (!root) return void vscode.window.showWarningMessage('task-x: open a folder to add an agent to it.');
  const name = await askName('agent');
  if (!name) return;
  await scaffold(
    path.join(root, '.claude', 'agents', `${name}.md`),
    `---\nname: ${name}\ndescription: Say what this agent does and when to use it.\n---\n\nYou are the ${name} agent. Describe its job, its limits, and what it reports back.\n`,
  );
}

/** Runs a `claude plugin` command in a terminal, so the user sees its output and any prompt. */
function pluginTerminal(args: string[]): void {
  const cli = findClaude();
  if (!cli) return void vscode.window.showErrorMessage('task-x: plugins need the Claude Code CLI. Install it, or set task-x.claudeCode.path.');
  const term = vscode.window.createTerminal({ name: 'Claude plugins', shellPath: cli, shellArgs: ['plugin', ...args], iconPath: new vscode.ThemeIcon('extensions') });
  term.show();
}

/** Asks for a plugin id and installs it with `claude plugin install`. */
export async function installPlugin(): Promise<void> {
  const id = await vscode.window.showInputBox({
    title: 'Install a Claude Code plugin',
    prompt: 'Plugin id, for example name@marketplace',
    validateInput: (s) => (PLUGIN_ID.test(s.trim()) ? undefined : 'Use name or name@marketplace.'),
  });
  if (id) pluginTerminal(['install', id.trim()]);
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
