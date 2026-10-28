import { execFile } from 'node:child_process';
import { accessSync, constants, statSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import * as vscode from 'vscode';
import { type EditMode, SECTION, chosenClaudeModel, chosenEngine, editMode, setWindowChoice } from './config.js';
import type { ChatMode } from './protocol.js';
import { runtimeEnv } from './runtime.js';

export const CLAUDE_KIND = 'claudeCode';
export const CLAUDE_LABEL = 'Claude Code CLI';
export const CLAUDE_MODELS = ['default', 'sonnet', 'opus', 'haiku'];
/** Reading the web never needs approval, in any chat mode. */
const WEB_TOOLS = ['WebFetch', 'WebSearch'];

const isExec = (p: string) => {
  try {
    if (!statSync(p).isFile()) return false;
    accessSync(p, process.platform === 'win32' ? constants.F_OK : constants.X_OK);
    return true;
  } catch {
    return false;
  }
};

/** Resolves the claude executable: the setting, then PATH, then the usual install folders. */
export function findClaude(): string | undefined {
  const configured = vscode.workspace.getConfiguration(SECTION).get<string>('claudeCode.path', '').trim();
  if (configured) return path.isAbsolute(configured) && isExec(configured) ? configured : undefined;
  const names = process.platform === 'win32' ? ['claude.exe', 'claude.cmd'] : ['claude'];
  const home = homedir();
  const dirs = [...(process.env.PATH ?? '').split(path.delimiter), path.join(home, '.local', 'bin'), path.join(home, '.claude', 'local'), '/opt/homebrew/bin', '/usr/local/bin'];
  for (const d of dirs.filter(Boolean)) for (const n of names) if (isExec(path.join(d, n))) return path.join(d, n);
  return undefined;
}

export const engine = (): 'anvil' | 'claudeCode' => ((chosenEngine() ?? vscode.workspace.getConfiguration(SECTION).get<string>('engine')) === CLAUDE_KIND ? 'claudeCode' : 'anvil');

export const claudeModel = () => (chosenClaudeModel() ?? vscode.workspace.getConfiguration(SECTION).get<string>('claudeCode.model', '')).trim() || 'default';

/**
 * Agent mode edits and runs sandboxed shell commands without asking in the auto and safe edit modes
 * (Claude Code keeps every command in its sandbox); in ask mode each edit and command goes to the
 * user. Other chat modes only plan. `approveAll` (auto only) also lets MCP and other tools run
 * without asking. Never bypassPermissions.
 */
export function claudePermissions(mode: ChatMode, em: EditMode = editMode()): { permissionMode: string; allowedTools: string[]; approveAll: boolean } {
  const approveAll = em === 'auto';
  if (mode !== 'agent') return { permissionMode: 'plan', allowedTools: WEB_TOOLS, approveAll };
  return em === 'ask' ? { permissionMode: 'default', allowedTools: [], approveAll } : { permissionMode: 'acceptEdits', allowedTools: ['Bash', ...WEB_TOOLS], approveAll };
}

export async function useClaude(model: string): Promise<void> {
  await setWindowChoice({ engine: CLAUDE_KIND, claudeModel: model === 'default' ? '' : model });
}

export interface ClaudeAuth {
  installed: boolean;
  loggedIn: boolean;
  email?: string;
  plan?: string;
}

const AUTH_TTL_MS = 30_000;
const LOGIN_TIMEOUT_MS = 10 * 60_000;
const LOGIN_POLL_MS = 2_000;
let cached: { at: number; auth: ClaudeAuth } | undefined;

const run = (cli: string, args: string[]) =>
  new Promise<string>((resolve, reject) => execFile(cli, args, { timeout: 15_000, maxBuffer: 1 << 20, env: runtimeEnv() }, (e, out) => (e ? reject(e) : resolve(out))));

/** The Anthropic account of the claude CLI (`claude auth status --json`), cached briefly. */
export async function claudeAuth(fresh = false): Promise<ClaudeAuth> {
  if (!fresh && cached && Date.now() - cached.at < AUTH_TTL_MS) return cached.auth;
  const cli = findClaude();
  let auth: ClaudeAuth = { installed: !!cli, loggedIn: false };
  if (cli) {
    try {
      const j = JSON.parse(await run(cli, ['auth', 'status', '--json'])) as Record<string, unknown>;
      const str = (v: unknown) => (typeof v === 'string' && v ? v : undefined);
      auth = { installed: true, loggedIn: j.loggedIn === true, email: str(j.email), plan: str(j.subscriptionType) };
    } catch {
      // A non-zero exit means no login.
    }
  }
  cached = { at: Date.now(), auth };
  return auth;
}

/** Signs in to an Anthropic account with `claude auth login` in a terminal, then selects Claude Code. */
export async function claudeSignIn(onChange: () => void): Promise<void> {
  const cli = findClaude();
  if (!cli) {
    const pick = await vscode.window.showErrorMessage('Sign in needs the Claude Code CLI. Install it, or set task-x.claudeCode.path.', 'Install Guide');
    if (pick) await vscode.env.openExternal(vscode.Uri.parse('https://docs.anthropic.com/en/docs/claude-code/setup'));
    return;
  }
  const term = vscode.window.createTerminal({ name: 'Anthropic Sign In', shellPath: cli, shellArgs: ['auth', 'login'], iconPath: new vscode.ThemeIcon('account') });
  term.show();
  let closed = false;
  const sub = vscode.window.onDidCloseTerminal((t) => (closed ||= t === term));
  try {
    const end = Date.now() + LOGIN_TIMEOUT_MS;
    while (Date.now() < end) {
      await new Promise((r) => setTimeout(r, LOGIN_POLL_MS));
      const auth = await claudeAuth(true);
      if (auth.loggedIn && (closed || term.exitStatus !== undefined)) {
        await useClaude(claudeModel());
        onChange();
        vscode.window.showInformationMessage(`Signed in to Anthropic${auth.email ? ` as ${auth.email}` : ''}. Chat now uses Claude Code.`);
        return;
      }
      if (closed) break;
    }
    onChange();
    if (!(await claudeAuth(true)).loggedIn) vscode.window.showWarningMessage('Anthropic sign-in did not finish.');
  } finally {
    sub.dispose();
  }
}

export async function claudeSignOut(onChange: () => void): Promise<void> {
  const cli = findClaude();
  if (!cli) return;
  const auth = await claudeAuth(true);
  const ok = await vscode.window.showWarningMessage(`Sign out of Anthropic${auth.email ? ` (${auth.email})` : ''}? This also signs out the claude CLI.`, { modal: true }, 'Sign Out');
  if (!ok) return;
  try {
    await run(cli, ['auth', 'logout']);
  } catch (e) {
    vscode.window.showErrorMessage(`Sign out failed: ${e instanceof Error ? e.message : String(e)}`);
  }
  await claudeAuth(true);
  if (engine() === 'claudeCode') await setWindowChoice({ engine: 'task-x' });
  onChange();
}
