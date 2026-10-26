import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { SECTION } from './config.js';
import { type ChatViewProvider, describeError } from './chatView.js';
import type { AgentSummary, ApprovalDecision, ApprovalRequest, ChatMode } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';
import { formatCost, runParams } from './runParams.js';

const MAX_FINISHED = 20;
const MAX_TITLE = 60;

interface Task {
  id: string;
  title: string;
  mode: ChatMode;
  started: number;
  abort?: AbortController;
  summary?: AgentSummary;
  error?: string;
}

const short = (t: string) => (t.length > MAX_TITLE ? `${t.slice(0, MAX_TITLE - 1)}…` : t);

/**
 * Tasks that run outside the chat. They keep running while the chat is busy or hidden; the status
 * bar counts running work, and a notification opens the result in the chat when a task ends.
 */
export class BackgroundTasks implements vscode.Disposable {
  private readonly tasks = new Map<string, Task>();
  /** Other owners of runtime task ids (best-of-N), so their approvals reach the user. */
  private readonly owners = new Map<string, string>();
  private readonly status = vscode.window.createStatusBarItem(vscode.StatusBarAlignment.Right, 99);
  private chatBusy = false;

  constructor(
    private readonly secrets: vscode.SecretStorage,
    private readonly getClient: () => Promise<JsonRpcClient>,
    private readonly chat: ChatViewProvider,
    private readonly log: (level: 'info' | 'warn' | 'error', msg: string) => void,
  ) {
    this.status.command = 'task-x.showTasks';
    this.render();
  }

  dispose(): void {
    for (const t of this.tasks.values()) t.abort?.abort();
    this.status.dispose();
  }

  setChatBusy(busy: boolean): void {
    this.chatBusy = busy;
    this.render();
  }

  /** Marks task ids that start with `prefix` as owned by a feature (for approvals and the count). */
  claim(prefix: string, title: string): vscode.Disposable {
    this.owners.set(prefix, title);
    this.render();
    return new vscode.Disposable(() => {
      this.owners.delete(prefix);
      this.render();
    });
  }

  private get running(): Task[] {
    return [...this.tasks.values()].filter((t) => t.abort);
  }

  private render(): void {
    const n = this.running.length + this.owners.size + (this.chatBusy ? 1 : 0);
    if (!n) return void this.status.hide();
    this.status.text = `$(sync~spin) task-x ${n}`;
    this.status.tooltip = `task-x: ${n} task${n > 1 ? 's' : ''} running. Click to see tasks.`;
    this.status.show();
  }

  async start(initial?: string): Promise<void> {
    const prompt = await vscode.window.showInputBox({ title: 'task-x: Run in Background', prompt: 'The task. You can keep working; task-x tells you when it is done.', value: initial, ignoreFocusOut: true });
    if (!prompt?.trim()) return;
    await this.run(prompt.trim(), 'agent');
  }

  async run(prompt: string, mode: ChatMode): Promise<void> {
    const task: Task = { id: `b${Date.now()}${randomBytes(3).toString('hex')}`, title: short(prompt.split('\n')[0]), mode, started: Date.now(), abort: new AbortController() };
    this.tasks.set(task.id, task);
    this.render();
    this.log('info', `background task ${task.id} started (${mode})`);
    try {
      const client = await this.getClient();
      const params = await runParams(this.secrets, { prompt, mode, taskId: task.id, editor: { openFiles: [], diagnostics: [] } });
      task.summary = await client.request<AgentSummary>('agent/run', params, { signal: task.abort!.signal });
      void this.chat.refreshChanges();
    } catch (e) {
      task.error = describeError(e);
    } finally {
      task.abort = undefined;
      this.trim();
      this.render();
    }
    this.log(task.summary?.status === 'completed' ? 'info' : 'warn', `background task ${task.id} ${task.summary?.status ?? 'failed'}`);
    const s = task.summary;
    const ok = s?.status === 'completed';
    const detail = s ? [s.changedFiles.length ? `${s.changedFiles.length} file(s) changed` : '', formatCost(s.metrics?.costUsd)].filter(Boolean).join(' · ') : task.error;
    const msg = `task-x background task ${ok ? 'done' : s ? `stopped (${s.status})` : 'failed'}: ${task.title}${detail ? ` · ${detail}` : ''}`;
    if (vscode.workspace.getConfiguration(SECTION).get<string>('notifications', 'whenHidden') === 'off') return;
    const pick = await (ok ? vscode.window.showInformationMessage : vscode.window.showWarningMessage)(msg, ...(s ? ['Open in Chat'] : []));
    if (pick) await this.open(task.id);
  }

  private trim(): void {
    const done = [...this.tasks.values()].filter((t) => !t.abort).sort((a, b) => b.started - a.started);
    for (const t of done.slice(MAX_FINISHED)) this.tasks.delete(t.id);
  }

  private async open(id: string): Promise<void> {
    try {
      await this.chat.openTask(id);
    } catch (e) {
      vscode.window.showErrorMessage(`task-x: ${describeError(e)}`);
    }
  }

  async show(): Promise<void> {
    const rows = [...this.tasks.values()].sort((a, b) => b.started - a.started);
    const items: (vscode.QuickPickItem & { task?: Task; chat?: boolean })[] = [];
    if (this.chatBusy) items.push({ label: '$(sync~spin) Chat request', description: 'running', chat: true });
    for (const [, title] of this.owners) items.push({ label: `$(sync~spin) ${title}`, description: 'running' });
    for (const t of rows) {
      const icon = t.abort ? '$(sync~spin)' : t.summary?.status === 'completed' ? '$(pass-filled)' : '$(warning)';
      const state = t.abort ? 'running · select to stop' : (t.summary?.status ?? 'failed');
      items.push({ label: `${icon} ${t.title}`, description: `${t.mode} · ${state}`, detail: t.error, task: t });
    }
    items.push({ label: '$(add) Run a new background task…' });
    const pick = await vscode.window.showQuickPick(items, { title: 'task-x: Tasks' });
    if (!pick) return;
    if (pick.chat) return void vscode.commands.executeCommand('task-x.chat.focus');
    if (!pick.task) return pick.label.includes('new background') ? this.start() : undefined;
    if (pick.task.abort) {
      const stop = await vscode.window.showWarningMessage(`Stop "${pick.task.title}"?`, 'Stop');
      if (stop) pick.task.abort.abort();
    } else if (pick.task.summary) await this.open(pick.task.id);
  }

  /** Approval for a background or claimed task; undefined when the task is not ours. */
  approve(req: ApprovalRequest): Promise<{ decision: ApprovalDecision }> {
    const task = [...this.tasks.values()].find((t) => t.abort && req.taskId.startsWith(t.id));
    const owner = [...this.owners.entries()].find(([p]) => req.taskId.startsWith(p))?.[1];
    if (!task && !owner) return Promise.resolve({ decision: 'deny' });
    const what = typeof req.preview === 'string' && req.preview ? req.preview.split('\n')[0].slice(0, 160) : req.tool;
    return Promise.resolve(
      vscode.window.showWarningMessage(`task-x (${task?.title ?? owner}) wants to use ${req.tool} (${req.permission}): ${what}`, 'Allow', 'Allow for Session', 'Deny'),
    ).then((pick) => ({ decision: pick === 'Allow' ? 'allow' : pick === 'Allow for Session' ? 'allowSession' : 'deny' }));
  }
}
