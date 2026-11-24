// The Antigravity CLI (`agy`) as a chat engine: finding it, its models, its permissions, and its limits.
// Claude Code lives in claudeCode.ts; the runtime side is runtime/src/agy_cli.rs.
import { accessSync, constants, statSync } from 'node:fs';
import { homedir } from 'node:os';
import path from 'node:path';
import * as vscode from 'vscode';
import { type LimitSummary, usageBars, usageLines } from './agyUsageFormat.js';
import { type EditMode, SECTION, chosenAgyModel, editMode, sandboxOn, setWindowChoice } from './config.js';
import type { AgyModels, AgyRunParams, AgyUsage, ChatMode } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';

export const AGY_KIND = 'antigravity';
export const AGY_LABEL = 'Antigravity CLI';
/** workspaceState: agy conversation id per task-x task, so follow-ups continue the conversation. */
export const AGY_CONVERSATIONS_KEY = 'task-x.agyConversations';
/** Limits are asked again after this long, or after every run. */
const USAGE_TTL_MS = 60_000;
const QUERY_TIMEOUT_MS = 90_000;

const isExec = (p: string) => {
  try {
    if (!statSync(p).isFile()) return false;
    accessSync(p, process.platform === 'win32' ? constants.F_OK : constants.X_OK);
    return true;
  } catch {
    return false;
  }
};

/** Resolves the agy executable: the setting, then PATH, then the usual install folders. */
export function findAgy(): string | undefined {
  const configured = vscode.workspace.getConfiguration(SECTION).get<string>('agy.path', '').trim();
  if (configured) return path.isAbsolute(configured) && isExec(configured) ? configured : undefined;
  const names = process.platform === 'win32' ? ['agy.exe', 'agy.cmd'] : ['agy'];
  const home = homedir();
  const dirs = [...(process.env.PATH ?? '').split(path.delimiter), path.join(home, '.local', 'bin'), '/opt/homebrew/bin', '/usr/local/bin'];
  for (const d of dirs.filter(Boolean)) for (const n of names) if (isExec(path.join(d, n))) return path.join(d, n);
  return undefined;
}

export const agyModel = () => (chosenAgyModel() ?? vscode.workspace.getConfiguration(SECTION).get<string>('agy.model', '')).trim() || 'default';

export async function useAgy(model: string): Promise<void> {
  await setWindowChoice({ engine: AGY_KIND, agyModel: model === 'default' ? '' : model });
}

/**
 * agy cannot ask for permission while it runs headless. Agent mode with the safe edit mode accepts
 * edits, auto lets agy decide sandboxed actions, and auto with Sandbox off passes
 * `--dangerously-skip-permissions` so unsandboxed commands (like scp/ssh) are not auto-denied.
 * Ask mode keeps agy's default, and other chat modes run agy's plan mode.
 */
export function agyPermissions(mode: ChatMode, em: EditMode = editMode(), sandbox = sandboxOn()): Pick<AgyRunParams, 'permissionMode' | 'sandbox' | 'skipPermissions'> {
  if (mode !== 'agent' || em === 'ask') return { permissionMode: 'default', sandbox, skipPermissions: false };
  return { permissionMode: em === 'auto' ? 'auto' : 'accept-edits', sandbox, skipPermissions: em === 'auto' && !sandbox };
}

/**
 * The models from `agy models` and the limits from `/usage` + `/credits`, both through the runtime,
 * and a status bar item with the limits while Antigravity is the chat engine.
 */
export class AgyCli implements vscode.Disposable {
  private readonly changedEmitter = new vscode.EventEmitter<void>();
  /** Fires when the models or the limits change (the model picker and Limits view show both). */
  readonly onDidChange = this.changedEmitter.event;
  models: { id: string; label: string }[] = [];
  usage?: AgyUsage;
  usageError?: string;
  private usageAt = 0;
  private modelsAt = 0;
  private usageLoad?: Promise<void>;
  private modelsLoad?: Promise<void>;

  constructor(private readonly getClient: () => Promise<JsonRpcClient>) {}

  /** The picker note: only shown when limits failed to load; normal limits render as progress bars. */
  get note(): string | undefined {
    if (this.usage) return undefined;
    return this.usageError ? `Limits unavailable: ${this.usageError}` : undefined;
  }

  /** Structured 5h + weekly usage bars for the model picker. */
  get limitSummary(): LimitSummary | undefined {
    return this.usage ? usageBars(this.usage) : undefined;
  }

  /** Loads the model list once (and again after an hour); fires onDidChange when it arrives. */
  loadModels(): void {
    const cliPath = findAgy();
    if (!cliPath || this.modelsLoad || Date.now() - this.modelsAt < 3_600_000) return;
    this.modelsLoad = (async () => {
      try {
        const client = await this.getClient();
        const r = await client.request<AgyModels>('agy/models', { cliPath }, { timeoutMs: QUERY_TIMEOUT_MS });
        this.models = Array.isArray(r.models) ? r.models : [];
        this.changedEmitter.fire();
      } catch {
        // The picker keeps agy's default model.
      } finally {
        this.modelsAt = Date.now();
        this.modelsLoad = undefined;
      }
    })();
  }

  /** Asks agy for its limits unless they are fresh; `force` after a run or from the command. */
  refreshUsage(force = false): Promise<void> {
    const cliPath = findAgy();
    if (!cliPath) return Promise.resolve();
    if (this.usageLoad) return this.usageLoad;
    if (!force && Date.now() - this.usageAt < USAGE_TTL_MS) return Promise.resolve();
    this.usageLoad = (async () => {
      try {
        const client = await this.getClient();
        this.usage = await client.request<AgyUsage>('agy/usage', { cliPath }, { timeoutMs: QUERY_TIMEOUT_MS });
        this.usageError = undefined;
      } catch (e) {
        this.usageError = e instanceof Error ? e.message : String(e);
      } finally {
        this.usageAt = Date.now();
        this.usageLoad = undefined;
        this.changedEmitter.fire();
      }
    })();
    return this.usageLoad;
  }

  /** `task-x.agyUsage`: the limits of every model group, asked fresh. */
  async show(): Promise<void> {
    if (!findAgy()) {
      void vscode.window.showErrorMessage('Antigravity CLI (agy) not found. Install it, or set task-x.agy.path.');
      return;
    }
    await vscode.window.withProgress({ location: vscode.ProgressLocation.Window, title: 'Antigravity limits' }, () => this.refreshUsage(true));
    if (!this.usage) {
      void vscode.window.showErrorMessage(`Antigravity limits unavailable: ${this.usageError ?? 'no answer'}`);
      return;
    }
    const upgrade = this.usage.credits?.upgradeUri;
    const items: vscode.QuickPickItem[] = usageLines(this.usage).map((label) => ({ label }));
    if (upgrade) items.push({ label: '$(link-external) Get more credits', detail: upgrade });
    const pick = await vscode.window.showQuickPick(items, { title: `${AGY_LABEL} limits`, placeHolder: this.usage.description || 'Share left per window' });
    if (pick?.detail && upgrade && pick.detail === upgrade) await vscode.env.openExternal(vscode.Uri.parse(upgrade));
  }

  dispose(): void {
    this.changedEmitter.dispose();
  }
}
