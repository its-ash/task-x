import * as vscode from 'vscode';
import type { AgentChat, Lane, LaneAction } from './chatView.js';

/**
 * The Agents view in the task-x side bar: the subagents of every chat tab, grouped by chat, with
 * live state and Show / Open / Message / Stop / Retry / Apply / Discard. The chat view owns the
 * subagents; this view reads them and sends actions back.
 */

type Source = {
  agentChats(): AgentChat[];
  readonly onDidChangeLanes: vscode.Event<void>;
  laneAction(tabId: string, taskId: string, action: LaneAction, text?: string): Promise<void>;
};

type ChatNode = { kind: 'chat'; chat: AgentChat };
type LaneNode = { kind: 'lane'; tabId: string; lane: Lane };
type Node = ChatNode | LaneNode;

const STATUS: Record<string, string> = {
  running: 'Running',
  completed: 'Done',
  cancelled: 'Stopped',
  failed: 'Failed',
  maxIterations: 'Turn limit',
  stuck: 'Stuck',
  providerError: 'Model error',
  deadlineExceeded: 'Time limit',
  budgetExceeded: 'Budget',
};
/** Redraws come at most this often; the elapsed time ticks at this pace while something runs. */
const REFRESH_MS = 250;
const TICK_MS = 5_000;

const elapsed = (l: Lane) => {
  if (!l.startedMs) return '';
  const s = Math.max(0, Math.round(((l.endedMs ?? Date.now()) - l.startedMs) / 1000));
  return `${Math.floor(s / 60)}:${String(s % 60).padStart(2, '0')}`;
};
const steps = (l: Lane) => (l.hidden ?? 0) + l.steps.length;
const pendingMerge = (l: Lane) => !!l.worktree && !!l.files && l.status !== 'running' && (!l.merge || l.merge === 'conflict');
const lastStep = (l: Lane) => {
  const s = l.steps[l.steps.length - 1];
  return s ? `${s.tool === 'user_message' ? 'You:' : s.tool} ${s.detail}`.trim() : 'Starting…';
};

function laneIcon(l: Lane): vscode.ThemeIcon {
  if (l.status === 'running') return new vscode.ThemeIcon('loading~spin', new vscode.ThemeColor('charts.blue'));
  if (l.status === 'completed') return new vscode.ThemeIcon('pass-filled', new vscode.ThemeColor('testing.iconPassed'));
  if (l.status === 'cancelled') return new vscode.ThemeIcon('circle-slash', new vscode.ThemeColor('list.warningForeground'));
  return new vscode.ThemeIcon('error', new vscode.ThemeColor('testing.iconFailed'));
}

/** `lane` plus flags the menus match on: `:run`, `:msg`, `:stop`, `:retry`, `:merge`. */
function contextValue(l: Lane): string {
  const run = l.status === 'running';
  const control = run && l.stoppable !== false;
  return ['lane', run && 'run', control && 'msg', control && 'stop', !run && l.status !== 'completed' && 'retry', pendingMerge(l) && 'merge'].filter(Boolean).join(':');
}

export class AgentsView implements vscode.TreeDataProvider<Node>, vscode.Disposable {
  static readonly viewId = 'task-x.agents';
  private readonly changed = new vscode.EventEmitter<Node | undefined | void>();
  readonly onDidChangeTreeData = this.changed.event;
  private readonly tree: vscode.TreeView<Node>;
  private readonly subs: vscode.Disposable[] = [];
  private refreshTimer?: NodeJS.Timeout;
  private tick?: NodeJS.Timeout;

  constructor(private readonly source: Source) {
    this.tree = vscode.window.createTreeView(AgentsView.viewId, { treeDataProvider: this, showCollapseAll: true });
    this.subs.push(
      this.tree,
      source.onDidChangeLanes(() => this.refresh()),
      vscode.commands.registerCommand('task-x.agents.refresh', () => this.refresh()),
      vscode.commands.registerCommand('task-x.agents.show', (n?: Node) => this.act(n, 'show')),
      vscode.commands.registerCommand('task-x.agents.open', (n?: Node) => this.act(n, 'open')),
      vscode.commands.registerCommand('task-x.agents.stop', (n?: Node) => this.act(n, 'stop')),
      vscode.commands.registerCommand('task-x.agents.retry', (n?: Node) => this.act(n, 'retry')),
      vscode.commands.registerCommand('task-x.agents.apply', (n?: Node) => this.act(n, 'apply')),
      vscode.commands.registerCommand('task-x.agents.discard', (n?: Node) => this.act(n, 'discard')),
      vscode.commands.registerCommand('task-x.agents.message', (n?: Node) => this.message(n)),
      vscode.commands.registerCommand('task-x.agents.stopAll', () => this.stopAll()),
    );
    this.refresh();
  }

  dispose(): void {
    clearTimeout(this.refreshTimer);
    clearInterval(this.tick);
    this.changed.dispose();
    for (const d of this.subs) d.dispose();
  }

  /** Redraws soon (many lane updates in a burst make one redraw), and keeps the clock ticking while any runs. */
  refresh(): void {
    if (this.refreshTimer) return;
    this.refreshTimer = setTimeout(() => {
      this.refreshTimer = undefined;
      const chats = this.source.agentChats();
      const running = chats.flatMap((c) => c.lanes).filter((l) => l.status === 'running').length;
      this.tree.badge = running ? { value: running, tooltip: `${running} subtask${running === 1 ? '' : 's'} running` } : undefined;
      void vscode.commands.executeCommand('setContext', 'task-x.agentsRunning', running > 0);
      if (running && !this.tick) this.tick = setInterval(() => this.changed.fire(), TICK_MS);
      if (!running && this.tick) {
        clearInterval(this.tick);
        this.tick = undefined;
      }
      this.changed.fire();
    }, REFRESH_MS);
  }

  getChildren(n?: Node): Node[] {
    if (!n) return this.source.agentChats().map((chat) => ({ kind: 'chat', chat }));
    if (n.kind === 'chat') {
      // The newest chat state: the node may hold copies from the last redraw.
      const chat = this.source.agentChats().find((c) => c.tabId === n.chat.tabId) ?? n.chat;
      return chat.lanes.map((lane) => ({ kind: 'lane', tabId: chat.tabId, lane }));
    }
    return [];
  }

  getTreeItem(n: Node): vscode.TreeItem {
    if (n.kind === 'chat') {
      const { chat } = n;
      const running = chat.lanes.filter((l) => l.status === 'running').length;
      const item = new vscode.TreeItem(chat.title, vscode.TreeItemCollapsibleState.Expanded);
      item.id = `chat:${chat.tabId}`;
      item.iconPath = new vscode.ThemeIcon(chat.running ? 'comment-discussion' : 'comment');
      item.description = running ? `${running} running · ${chat.lanes.length} total` : `${chat.lanes.length} subtask${chat.lanes.length === 1 ? '' : 's'}`;
      item.contextValue = 'chat';
      return item;
    }
    const l = n.lane;
    const item = new vscode.TreeItem(l.agent || l.name, vscode.TreeItemCollapsibleState.None);
    item.id = `lane:${n.tabId}:${l.taskId}`;
    item.iconPath = laneIcon(l);
    const what = l.status === 'running' ? lastStep(l) : l.status === 'completed' ? (l.files ? `${l.files} file${l.files > 1 ? 's' : ''} +${l.added ?? 0} −${l.removed ?? 0}` : 'no file changes') : (l.error?.split('\n')[0] ?? STATUS[l.status] ?? l.status);
    item.description = [l.model, `${steps(l)} step${steps(l) === 1 ? '' : 's'}`, elapsed(l), what].filter(Boolean).join(' · ');
    const tip = new vscode.MarkdownString(undefined, true);
    tip.appendMarkdown(`**${l.agent || l.name}** · ${STATUS[l.status] ?? l.status}${l.model ? ` · \`${l.model}\`` : ''}\n\n`);
    tip.appendText(`${l.objective}\n\n`);
    if (l.status === 'running') tip.appendText(`Now: ${lastStep(l)}\n`);
    if (l.error) tip.appendText(`Error: ${l.error}\n`);
    if (l.files) tip.appendText(`Changed ${l.files} file${l.files > 1 ? 's' : ''} (+${l.added ?? 0} −${l.removed ?? 0})${l.merge ? `, ${l.merge}` : ''}\n`);
    if (l.cost) tip.appendText(`Cost: ${l.cost}\n`);
    item.tooltip = tip;
    item.contextValue = contextValue(l);
    item.command = { command: 'task-x.agents.show', title: 'Show in Chat', arguments: [n] };
    return item;
  }

  private async act(n: Node | undefined, action: LaneAction): Promise<void> {
    const node = n ?? this.tree.selection[0];
    if (node?.kind !== 'lane') return;
    await this.source.laneAction(node.tabId, node.lane.taskId, action);
    this.refresh();
  }

  private async message(n?: Node): Promise<void> {
    const node = n ?? this.tree.selection[0];
    if (node?.kind !== 'lane') return;
    const text = await vscode.window.showInputBox({
      title: `task-x: Message ${node.lane.agent || node.lane.name}`,
      prompt: 'The subtask reads it before its next step. The lead agent does not see it.',
      placeHolder: 'For example: also cover the error path',
      validateInput: (v) => (v.length > 4000 ? 'At most 4000 characters' : undefined),
    });
    if (text?.trim()) await this.source.laneAction(node.tabId, node.lane.taskId, 'message', text);
  }

  private async stopAll(): Promise<void> {
    for (const chat of this.source.agentChats()) {
      for (const l of chat.lanes) if (l.status === 'running' && l.stoppable !== false) await this.source.laneAction(chat.tabId, l.taskId, 'stop');
    }
  }
}
