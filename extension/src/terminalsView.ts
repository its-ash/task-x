import * as vscode from 'vscode';
import type { ProcInfo, ProcList, ProcOutput, ProcOutputParams } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';
import { type ProcGroup, endLine, groupProcs, headerLines, procDescription, procStatusText, shortCommand, toTerminal } from './terminalsFormat.js';

/**
 * The Terminals view in the task-x side bar: every command the agent runs (`run_command`,
 * `run_tests`, `check_build`, ...), grouped by chat, live. Each one opens as a VS Code terminal
 * that shows its output so far and then follows it; the terminal exists only once it is opened, so
 * the terminal panel never fills up with the agent's commands. Ctrl+C in it, or Stop, ends the command.
 *
 * The runtime keeps the list and the output (`procs/*`); this view mirrors them per runtime process.
 */

type GroupNode = { kind: 'group'; group: ProcGroup };
type ProcNode = { kind: 'proc'; proc: ProcInfo };
type Node = GroupNode | ProcNode;

const REFRESH_MS = 200;
const TICK_MS = 1000;
const message = (e: unknown) => (e instanceof Error ? e.message : String(e));

function procIcon(p: ProcInfo): vscode.ThemeIcon {
  switch (p.status) {
    case 'running':
      return new vscode.ThemeIcon('loading~spin', new vscode.ThemeColor('charts.blue'));
    case 'exited':
      return new vscode.ThemeIcon('pass-filled', new vscode.ThemeColor('testing.iconPassed'));
    case 'stopped':
      return new vscode.ThemeIcon('circle-slash', new vscode.ThemeColor('list.warningForeground'));
    case 'timedOut':
      return new vscode.ThemeIcon('watch', new vscode.ThemeColor('testing.iconFailed'));
    default:
      return new vscode.ThemeIcon('error', new vscode.ThemeColor('testing.iconFailed'));
  }
}

/** One command's terminal: its output so far (`procs/output`), then the live notifications. */
class ProcTerminal {
  readonly terminal: vscode.Terminal;
  private readonly writer = new vscode.EventEmitter<string>();
  /** `loading`: the output so far is on its way; notifications that come meanwhile wait in `early`. */
  private state: 'waiting' | 'loading' | 'live' = 'waiting';
  private early: { from: number; to: number; data: string }[] = [];
  private ended = false;
  private hinted = false;
  private disposed = false;

  constructor(
    private proc: ProcInfo,
    private readonly load: () => Promise<ProcOutput>,
    private readonly stop: () => void,
    onClose: () => void,
  ) {
    const pty: vscode.Pseudoterminal = {
      onDidWrite: this.writer.event,
      open: () => void this.fill(),
      close: () => {
        this.disposed = true;
        this.early = [];
        this.writer.dispose();
        onClose();
      },
      handleInput: (d) => this.input(d),
    };
    this.terminal = vscode.window.createTerminal({ name: `task-x: ${shortCommand(proc.command, 40)}`, pty, iconPath: new vscode.ThemeIcon('terminal'), isTransient: true });
  }

  private async fill(): Promise<void> {
    this.state = 'loading';
    this.writer.fire(headerLines(this.proc));
    try {
      const out = await this.load();
      if (this.disposed) return;
      if (out.truncated) this.writer.fire('\x1b[2m[… earlier output dropped …]\x1b[0m\r\n');
      this.writer.fire(toTerminal(out.data));
      // Notifications sent after the answer was made may have come first: append any bytes past out.to.
      for (const e of this.early) {
        if (e.from >= out.to) {
          this.writer.fire(toTerminal(e.data));
        } else if (e.to > out.to) {
          const buf = Buffer.from(e.data, 'utf8');
          const skip = out.to - e.from;
          if (skip < buf.length) this.writer.fire(toTerminal(buf.subarray(skip).toString('utf8')));
        }
      }
      // An end that came meanwhile is newer than the answer's state.
      if (this.proc.status === 'running') this.proc = out.proc;
    } catch (e) {
      if (!this.disposed) this.writer.fire(`\x1b[2m[the output is not available: ${message(e)}]\x1b[0m\r\n`);
    }
    this.early = [];
    this.state = 'live';
    if (!this.disposed && this.proc.status !== 'running') this.finish(this.proc);
  }

  output(p: ProcOutputParams): void {
    if (this.disposed) return;
    if (this.state === 'loading') {
      if (this.early.length < 2000) this.early.push({ from: p.from, to: p.to, data: p.data });
    } else if (this.state === 'live' && !this.ended) {
      this.writer.fire(toTerminal(p.data));
    }
  }

  finish(proc: ProcInfo): void {
    this.proc = proc;
    if (this.state !== 'live' || this.ended) return;
    this.ended = true;
    this.writer.fire(endLine(proc));
  }

  /** The runtime that ran the command is gone: no more output will come. */
  lost(): void {
    if (this.ended || this.proc.status !== 'running') return;
    this.ended = true;
    this.writer.fire('\r\n\x1b[2m[the task-x runtime stopped; no more output from this command]\x1b[0m\r\n');
  }

  private input(d: string): void {
    if (d === '\x03' && this.proc.status === 'running' && !this.ended) {
      this.writer.fire('^C');
      return this.stop();
    }
    if (this.hinted) return;
    this.hinted = true;
    this.writer.fire(`\r\n\x1b[2m[this terminal shows a command task-x runs; it takes no input${this.proc.status === 'running' ? '. Ctrl+C stops the command' : ''}]\x1b[0m\r\n`);
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.early = [];
    this.terminal.dispose();
    this.writer.dispose();
  }
}

export class TerminalsView implements vscode.TreeDataProvider<Node>, vscode.Disposable {
  static readonly viewId = 'task-x.terminals';
  private readonly changed = new vscode.EventEmitter<Node | undefined | void>();
  readonly onDidChangeTreeData = this.changed.event;
  private readonly countChanged = new vscode.EventEmitter<number>();
  readonly onDidChangeCount = this.countChanged.event;
  private readonly tree: vscode.TreeView<Node>;
  private readonly subs: vscode.Disposable[] = [];
  private readonly procs = new Map<string, ProcInfo>();
  private readonly terms = new Map<string, ProcTerminal>();
  private client: JsonRpcClient | undefined;
  private offs: (() => void)[] = [];
  private refreshTimer?: NodeJS.Timeout;
  private tick?: NodeJS.Timeout;

  /**
   * @param getClient starts the runtime when needed (Refresh).
   * @param titleOf the chat a runtime task belongs to, for the groups.
   * @param titlesChanged fires when chats start, end, or get renamed.
   */
  constructor(
    private readonly getClient: () => Promise<JsonRpcClient>,
    private readonly titleOf: (taskId: string) => string | undefined,
    titlesChanged: vscode.Event<void> | undefined,
    private readonly log: (msg: string) => void,
  ) {
    this.tree = vscode.window.createTreeView(TerminalsView.viewId, { treeDataProvider: this, showCollapseAll: true });
    this.subs.push(
      this.tree,
      vscode.commands.registerCommand('task-x.terminals.open', (n?: Node) => this.open(n)),
      vscode.commands.registerCommand('task-x.terminals.stop', (n?: Node) => this.stop(n)),
      vscode.commands.registerCommand('task-x.terminals.copyCommand', (n?: Node) => this.copy(n)),
      vscode.commands.registerCommand('task-x.terminals.stopAll', () => this.stopAll()),
      vscode.commands.registerCommand('task-x.terminals.clear', () => this.clear()),
      vscode.commands.registerCommand('task-x.terminals.refresh', () => this.reload()),
    );
    if (titlesChanged) this.subs.push(titlesChanged(() => this.refresh()));
  }

  dispose(): void {
    clearTimeout(this.refreshTimer);
    clearInterval(this.tick);
    for (const off of this.offs) off();
    for (const t of this.terms.values()) t.dispose();
    this.changed.dispose();
    this.countChanged.dispose();
    for (const d of this.subs) d.dispose();
  }

  get runningCount(): number {
    return [...this.procs.values()].filter((p) => p.status === 'running').length;
  }

  /** Seamlessly opens the live terminal for a command started by an agent, or focuses the Terminals view. */
  openForTask(taskId?: string, command?: string): boolean {
    const all = [...this.procs.values()].reverse();
    const norm = command?.trim();
    const match =
      (taskId && norm ? all.find((p) => p.taskId === taskId && (p.command === norm || p.command.includes(norm) || norm.includes(p.command))) : undefined) ??
      (norm ? all.find((p) => p.command === norm || p.command.includes(norm) || norm.includes(p.command)) : undefined) ??
      (taskId ? all.find((p) => p.taskId === taskId && p.status === 'running') ?? all.find((p) => p.taskId === taskId) : undefined);
    if (match) {
      this.open({ kind: 'proc', proc: match });
      return true;
    }
    void vscode.commands.executeCommand('workbench.view.extension.task-x-activity');
    return false;
  }

  /** Follows a newly started runtime: the commands of the one before it are gone with it. */
  attach(client: JsonRpcClient): void {
    if (this.client === client) return;
    this.detach();
    this.client = client;
    this.offs = [
      client.onNotification('procs/started', (p) => this.upsert((p as { proc: ProcInfo }).proc)),
      client.onNotification('procs/output', (p) => {
        const params = p as ProcOutputParams;
        this.terms.get(params.id)?.output(params);
      }),
      client.onNotification('procs/ended', (p) => {
        const proc = (p as { proc: ProcInfo }).proc;
        this.upsert(proc);
        this.terms.get(proc.id)?.finish(proc);
      }),
    ];
    void this.load(client);
  }

  /** The runtime stopped: its commands stopped with it. Open terminals stay, marked as ended. */
  detach(client?: JsonRpcClient): void {
    if (client && client !== this.client) return;
    for (const off of this.offs) off();
    this.offs = [];
    this.client = undefined;
    for (const t of this.terms.values()) t.lost();
    this.terms.clear();
    this.procs.clear();
    this.refresh();
  }

  private async load(client: JsonRpcClient): Promise<void> {
    try {
      const list = await client.request<ProcList>('procs/list');
      if (this.client !== client) return;
      // Ended commands the runtime dropped (it keeps the newest 100) go; one that started while
      // the list was on its way stays (its `procs/started` may have come first).
      const listed = new Set(list.procs.map((p) => p.id));
      for (const [id, p] of this.procs) if (!listed.has(id) && p.status !== 'running') this.procs.delete(id);
      // An end that came while the list was on its way is newer than the list's "running".
      for (const p of list.procs) if (!(p.status === 'running' && (this.procs.get(p.id)?.status ?? 'running') !== 'running')) this.procs.set(p.id, p);
      this.refresh();
    } catch (e) {
      this.log(`procs/list failed: ${message(e)}`);
    }
  }

  private async reload(): Promise<void> {
    try {
      const client = await this.getClient();
      this.attach(client);
      await this.load(client);
    } catch (e) {
      void vscode.window.showErrorMessage(`task-x: listing the terminals failed: ${message(e)}`);
    }
  }

  private upsert(p: ProcInfo): void {
    this.procs.set(p.id, p);
    this.refresh();
  }

  /** Redraws soon (a burst of changes makes one redraw), and ticks the clocks while a command runs. */
  private refresh(): void {
    if (this.refreshTimer) return;
    this.refreshTimer = setTimeout(() => {
      this.refreshTimer = undefined;
      const running = [...this.procs.values()].filter((p) => p.status === 'running').length;
      this.tree.badge = running ? { value: running, tooltip: `${running} command${running === 1 ? '' : 's'} running` } : undefined;
      void vscode.commands.executeCommand('setContext', 'task-x.terminalsRunning', running > 0);
      if (running && !this.tick) this.tick = setInterval(() => this.tree.visible && this.changed.fire(), TICK_MS);
      if (!running && this.tick) {
        clearInterval(this.tick);
        this.tick = undefined;
      }
      this.changed.fire();
      this.countChanged.fire(running);
    }, REFRESH_MS);
  }

  getChildren(n?: Node): Node[] {
    const groups = groupProcs([...this.procs.values()], this.titleOf);
    if (!n) return groups.map((group) => ({ kind: 'group', group }));
    if (n.kind === 'group') return (groups.find((g) => g.key === n.group.key)?.procs ?? []).map((proc) => ({ kind: 'proc', proc }));
    return [];
  }

  getTreeItem(n: Node): vscode.TreeItem {
    if (n.kind === 'group') {
      const { group } = n;
      const running = group.procs.filter((p) => p.status === 'running').length;
      const item = new vscode.TreeItem(group.title, vscode.TreeItemCollapsibleState.Expanded);
      item.id = `group:${group.key}`;
      item.iconPath = new vscode.ThemeIcon(group.title === 'Other' ? 'terminal' : 'comment');
      item.description = running ? `${running} running · ${group.procs.length} total` : `${group.procs.length} command${group.procs.length === 1 ? '' : 's'}`;
      item.contextValue = 'group';
      return item;
    }
    const p = this.procs.get(n.proc.id) ?? n.proc;
    const item = new vscode.TreeItem(shortCommand(p.command), vscode.TreeItemCollapsibleState.None);
    item.id = `proc:${p.id}`;
    item.iconPath = procIcon(p);
    item.description = procDescription(p);
    const tip = new vscode.MarkdownString(undefined, true);
    tip.appendCodeblock(p.command, 'sh');
    tip.appendMarkdown(`**${procStatusText[p.status]}**${p.exitCode !== undefined ? ` · exit code ${p.exitCode}` : ''} · pid ${p.pid} · in \`${p.cwd || '.'}\`\n\n`);
    tip.appendText(`Started ${new Date(p.startedMs).toLocaleTimeString()}${p.endedMs ? `, ended ${new Date(p.endedMs).toLocaleTimeString()}` : ''}`);
    item.tooltip = tip;
    item.contextValue = p.status === 'running' ? 'proc:run' : 'proc';
    item.command = { command: 'task-x.terminals.open', title: 'Open Terminal', arguments: [n] };
    return item;
  }

  private pick(n?: Node): ProcInfo | undefined {
    const node = n ?? this.tree.selection[0];
    return node?.kind === 'proc' ? (this.procs.get(node.proc.id) ?? node.proc) : undefined;
  }

  /** Shows the command's terminal, making it on first use. */
  private open(n?: Node): void {
    const p = this.pick(n);
    const client = this.client;
    if (!p) return;
    if (!client) return void vscode.window.showInformationMessage('task-x: that command ran in a runtime that has stopped; its output is gone.');
    let t = this.terms.get(p.id);
    if (!t) {
      const made: ProcTerminal = new ProcTerminal(
        p,
        () => client.request<ProcOutput>('procs/output', { id: p.id }),
        () => void this.stopProc(p.id),
        () => {
          if (this.terms.get(p.id) === made) this.terms.delete(p.id);
        },
      );
      t = made;
      this.terms.set(p.id, t);
    }
    t.terminal.show();
  }

  private async stopProc(id: string): Promise<void> {
    try {
      await this.client?.request<{ stopped: boolean }>('procs/stop', { id });
    } catch (e) {
      void vscode.window.showErrorMessage(`task-x: stopping the command failed: ${message(e)}`);
    }
  }

  private async stop(n?: Node): Promise<void> {
    const p = this.pick(n);
    if (p?.status === 'running') await this.stopProc(p.id);
  }

  private async copy(n?: Node): Promise<void> {
    const p = this.pick(n);
    if (p) await vscode.env.clipboard.writeText(p.command);
  }

  private async stopAll(): Promise<void> {
    const running = [...this.procs.values()].filter((p) => p.status === 'running');
    if (!running.length) return;
    const ok = await vscode.window.showWarningMessage(`Stop ${running.length} running command${running.length === 1 ? '' : 's'}? The agent sees each one as stopped by you.`, { modal: true }, 'Stop All');
    if (ok === 'Stop All') await Promise.allSettled(running.map((p) => this.stopProc(p.id)));
  }

  /** Drops the ended commands from the list; their open terminals stay until closed. */
  private async clear(): Promise<void> {
    try {
      await this.client?.request('procs/clear');
    } catch (e) {
      this.log(`procs/clear failed: ${message(e)}`);
    }
    for (const [id, p] of this.procs) {
      if (p.status === 'running') continue;
      this.procs.delete(id);
      const t = this.terms.get(id);
      if (t) {
        t.dispose();
        this.terms.delete(id);
      }
    }
    this.refresh();
  }
}
