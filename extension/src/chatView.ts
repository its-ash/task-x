import { randomBytes } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import * as vscode from 'vscode';
import { activeRef, hasApiKey, isProviderRef, modelFor, providerConfig, providerRefs, refLabel, refNeedsKey, setProvider, settings, SECTION } from './config.js';
import {
  type AgentEvent,
  type AgentEventParams,
  type AgentSummary,
  type ApprovalDecision,
  type ApprovalMode,
  type ApprovalRequest,
  USER_MODES,
  type ChatMode,
  type EditorContext,
  type ProviderErrorData,
  type ResolvedSkill,
  type SkillInfo,
} from './protocol.js';
import { CLAUDE_KIND, CLAUDE_LABEL, CLAUDE_MODELS, claudeAuth, claudeModel, claudePermissions, claudeSignIn, claudeSignOut, engine, findClaude, useClaude } from './claudeCode.js';
import { type JsonRpcClient, RpcError } from './rpc.js';
import { diffStats, formatOutput } from './output.js';
import { APPROVAL_MODES, formatCost, runParams } from './runParams.js';
import { commitPaths, commitSubject } from './autoCommit.js';
import { WorkspaceSync } from './workspace.js';

const TRANSCRIPT_KEY = 'task-x.transcript';
const INCLUDE_EDITOR_KEY = 'task-x.includeEditor';
const AUTO_COMMIT_KEY = 'task-x.autoCommit';
/** `@path` references in a prompt: a path of letters, digits, and . _ - / after an @ at a word start. */
const MENTION = /(?:^|\s)@((?:[\w.-]+\/)*[\w.-]+\/?)/g;
const MAX_MENTIONS = 10;
const MAX_FILE_ITEMS = 50;
const FILE_LIST_TTL_MS = 10_000;
const MAX_LISTED_FILES = 20_000;
const FILE_EXCLUDE = '**/{node_modules,.git,target,out,dist,build,.next,.venv,__pycache__}/**';
const CLAUDE_SESSIONS_KEY = 'task-x.claudeSessions';
const TASK_KEY = 'task-x.taskId';
const MAX_ITEMS = 400;
const MAX_INPUT_CHARS = 100_000;
const MAX_SELECTION_CHARS = 20_000;
const MAX_PICKER_MODELS = 100;
const MAX_OPEN_FILES = 10;
const MAX_DIAGNOSTICS = 50;
const MAX_QUEUE = 10;
const MAX_STEERS = 10;
/** Output kept in the saved transcript per tool call; the full text stays in memory for the session. */
const MAX_SAVED_OUTPUT = 8_000;
/** Diff kept on each file item so its edit card can show the change after a reload. */
const MAX_CARD_DIFF = 6_000;
const OUTPUT_TOOLS = new Set(['Bash', 'BashOutput', 'run_command', 'run_tests', 'check_build', 'run_linter', 'format_code', 'get_diagnostics', 'git_status', 'git_diff', 'git_log', 'git_show']);
const SEVERITY = ['error', 'warning', 'info', 'hint'];
const CONTINUE_PROMPT = 'Continue the task from where you stopped. Check what is already done, finish the remaining work, then run the build and tests.';
const TASK_KIND_LABELS: Record<string, string> = {
  question: 'Question',
  bugInvestigation: 'Bug investigation',
  codeModification: 'Code change',
  newFeature: 'New feature',
  refactoring: 'Refactoring',
  testing: 'Tests',
  documentation: 'Documentation',
  repositoryWide: 'Repository-wide change',
  architecture: 'Architecture',
};

/** A short label for a context or instruction path: home-relative, or the last two path parts. */
function shortPath(p: string): string {
  const home = os.homedir();
  if (home && p.startsWith(home + path.sep)) return `~/${path.relative(home, p).split(path.sep).join('/')}`;
  const parts = p.split(/[\\/]/);
  return parts.length > 2 ? `…/${parts.slice(-2).join('/')}` : p;
}
/** Tool steps kept per subagent lane; older ones collapse into a count. */
const MAX_LANE_STEPS = 30;

/** A rendered transcript entry; persisted per workspace. */
type Item =
  | { kind: 'user'; text: string; mode: ChatMode }
  | { kind: 'assistant'; text: string }
  | { kind: 'tool'; id: string; tool: string; detail: string; ok?: boolean; output?: string; outputCut?: boolean }
  | { kind: 'file'; path: string; operation: number | null; diffKey: string; added: number; removed: number; diff: string; diffCut: boolean; undone?: boolean; kept?: boolean }
  | { kind: 'context'; text: string }
  | { kind: 'steer'; text: string }
  | { kind: 'compressed'; before: number; after: number; window: number; pruned: number; folded: number; dropped: boolean }
  | { kind: 'taskInfo'; taskKind: string; label: string; files: string[]; instructions: { label: string; path: string }[]; skills: string[] }
  | { kind: 'summary'; text: string; ok: boolean; canContinue?: boolean; title?: string; stats?: string[]; error?: string; signIn?: boolean; raiseLimit?: boolean }
  | { kind: 'error'; text: string; retryText?: string }
  | Lane;

/** One subagent: its role, model, live tool steps, and result. */
type Lane = {
  kind: 'lane';
  taskId: string;
  name: string;
  role: string;
  objective: string;
  model: string;
  step?: string | null;
  status: string;
  error?: string;
  steps: { id: string; tool: string; detail: string; ok?: boolean }[];
  hidden?: number;
  tokens?: number;
  cost?: string;
  files?: number;
  added?: number;
  removed?: number;
  worktree?: string | null;
  dir?: string | null;
  merge?: 'merged' | 'conflict' | 'discarded';
  mergeError?: string;
};

type Inbound =
  | { type: 'ready' }
  | { type: 'send'; text: string; queue?: boolean }
  | { type: 'cancel' }
  | { type: 'clear' }
  | { type: 'selectModel'; kind: string; model: string }
  | { type: 'selectMode'; mode: string }
  | { type: 'loadModels' }
  | { type: 'addProvider' }
  | { type: 'signIn' }
  | { type: 'listSkills' }
  | { type: 'copyText'; text: string }
  | { type: 'signOut' }
  | { type: 'approve'; callId: string; decision: string }
  | { type: 'openDiff'; diffKey: string; path?: string }
  | { type: 'openFile'; path: string }
  | { type: 'undo'; operation: number }
  | { type: 'keep'; paths?: string[] }
  | { type: 'revert'; paths?: string[] }
  | { type: 'diffFile'; path: string }
  | { type: 'selectApproval'; mode: string }
  | { type: 'selectAutoCommit'; enabled: boolean }
  | { type: 'openSettings'; query: string }
  | { type: 'suggest'; text: string; mode: string }
  | { type: 'copyOutput'; id: string }
  | { type: 'openOutput'; id: string }
  | { type: 'stopLane'; taskId: string }
  | { type: 'applyWorktree'; name: string }
  | { type: 'discardWorktree'; name: string }
  | { type: 'openWorktree'; name: string }
  | { type: 'showProblems' }
  | { type: 'listFiles'; query: string }
  | { type: 'toggleEditor'; include: boolean }
  | { type: 'showGraph' }
  | { type: 'listTasks' }
  | { type: 'openTask'; id: string };

/** One saved task in the History tab. */
interface TaskRow {
  id: string;
  title: string;
  status: string;
  chatMode: ChatMode;
  updatedMs: number;
  changedFiles: string[];
}

interface PendingFile {
  path: string;
  original: string | null;
  current: string | null;
  added: number;
  removed: number;
}

export const ORIGINAL_SCHEME = 'task-x-original';

/** Serves the pre-agent content of changed files to VS Code's diff editor. */
export class OriginalContent implements vscode.TextDocumentContentProvider {
  readonly files = new Map<string, string>();
  private readonly changed = new vscode.EventEmitter<vscode.Uri>();
  readonly onDidChange = this.changed.event;

  provideTextDocumentContent(uri: vscode.Uri): string {
    return this.files.get(uri.path.replace(/^\//, '')) ?? '';
  }

  set(path: string, content: string | null): void {
    this.files.set(path, content ?? '');
    this.changed.fire(vscode.Uri.from({ scheme: ORIGINAL_SCHEME, path: `/${path}` }));
  }
}

export function describeError(e: unknown): string {
  if (e instanceof RpcError) {
    const d = e.data as ProviderErrorData | undefined;
    if (e.cancelled) return 'Cancelled.';
    if (d?.kind === 'auth') return `${e.message} — run "task-x: Add LLM Provider…".`;
    if (d?.kind) return `${d.kind}${d.status ? ` (${d.status})` : ''}: ${e.message}`;
    return e.message;
  }
  return e instanceof Error ? e.message : String(e);
}

export function chatMode(): ChatMode {
  const m = vscode.workspace.getConfiguration(SECTION).get<string>('chatMode');
  return USER_MODES.includes(m as ChatMode) ? (m as ChatMode) : 'agent';
}

function toolDetail(tool: string, input: Record<string, unknown>): string {
  const s = (k: string) => (typeof input[k] === 'string' ? (input[k] as string) : Array.isArray(input[k]) ? (input[k] as unknown[]).map(String).join(' ') : '');
  const claude = s('command') || s('file_path') || s('url') || s('notebook_path');
  if (claude) return claude.length > 120 ? `${claude.slice(0, 117)}…` : claude;
  if (tool === 'apply_patch') return [...s('patch').matchAll(/^\+\+\+ (?:b\/)?(.+)$/gm)].map((m) => m[1]).join(', ');
  const v = s('path') || s('query') || s('pattern') || s('name') || s('glob') || (s('from') && `${s('from')} → ${s('to')}`) || s('rev');
  return v.length > 120 ? `${v.slice(0, 117)}…` : v;
}

const AUTH_ERROR = /not logged in|\/login|invalid (x-)?api[- ]key|authentication_error|unauthorized|oauth token/i;

/** A short label for where a skill comes from, such as "~/.claude" or ".github". */
function skillOrigin(source: string): string {
  const m = /[\\/](\.(?:claude|copilot|task-x|anvil|github))[\\/]/.exec(source);
  if (!m) return '';
  const root = WorkspaceSync.root();
  return root && source.startsWith(root) ? m[1] : `~/${m[1]}`;
}

const fmtTokens = (n: number) => (n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n));

function summaryText(s: AgentSummary): { text: string; ok: boolean; canContinue: boolean; title: string; stats: string[]; error?: string; signIn: boolean; raiseLimit: boolean } {
  const status: Record<string, string> = {
    completed: 'Done',
    cancelled: 'Stopped',
    maxIterations: `Paused after ${s.iterations} turns (the turn limit)`,
    budgetExceeded: 'Stopped: reached the tool or token budget',
    deadlineExceeded: 'Stopped: reached the time limit',
    providerError: 'Stopped: model request failed',
    stuck: 'Stopped: tool calls kept failing',
    failed: 'Failed',
  };
  const title = status[s.status] ?? s.status;
  const parts: string[] = [];
  if (s.changedFiles.length) parts.push(`${s.changedFiles.length} file${s.changedFiles.length > 1 ? 's' : ''} changed`);
  if (s.validations.length) {
    const bad = s.validations.filter((v) => !v.success).length;
    parts.push(bad ? `checks: ${bad} of ${s.validations.length} failed` : `checks: ${s.validations.length} passed`);
  } else if (s.changedFiles.length) {
    parts.push('no tests or builds were run');
  }
  if (s.skippedChecks?.length) parts.push(`not run: ${s.skippedChecks.join(', ')}`);
  const u = s.usage;
  if (u.inputTokens || u.outputTokens) parts.push(`${fmtTokens((u.inputTokens ?? 0) + (u.outputTokens ?? 0))} tokens (${fmtTokens(u.inputTokens ?? 0)} in · ${fmtTokens(u.outputTokens ?? 0)} out)`);
  const m = s.metrics;
  if (m?.subagents) parts.push(`${m.subagents} subagent${m.subagents > 1 ? 's' : ''}${m.subagentTokens ? ` · ${fmtTokens(m.subagentTokens)} tokens` : ''}`);
  const cost = formatCost(m?.costUsd);
  if (cost) parts.push(m?.subagentCostUsd ? `${cost} (subagents ${formatCost(m.subagentCostUsd) || '$0'})` : cost);
  if (m?.unpricedTokens) parts.push(`cost unknown for ${fmtTokens(m.unpricedTokens)} tokens (set task-x.pricing)`);
  const error = s.error?.trim() || undefined;
  const signIn = !!error && AUTH_ERROR.test(error);
  const canContinue = ['maxIterations', 'budgetExceeded', 'deadlineExceeded', 'cancelled'].includes(s.status);
  return {
    text: [title, ...parts, ...(error ? [error] : [])].join(' · '),
    ok: s.status === 'completed',
    canContinue,
    raiseLimit: s.status === 'maxIterations',
    title: signIn ? 'Sign in required' : title,
    stats: parts,
    error: signIn ? 'Your Anthropic session is not active. Sign in to continue with Claude Code.' : error,
    signIn,
  };
}

export class ChatViewProvider implements vscode.WebviewViewProvider, vscode.Disposable {
  static readonly viewId = 'task-x.chat';
  private view?: vscode.WebviewView;
  private abort?: AbortController;
  private items: Item[];
  private taskId: string | undefined;
  private readonly listed = new Map<string, string[]>();
  /** Messages sent while a request was running; each one runs after the one before it. */
  private readonly queue: { text: string; mode?: ChatMode }[] = [];
  private readonly approvals = new Map<string, (d: ApprovalDecision | 'always') => void>();
  private readonly diffs = new Map<string, string>();
  private readonly outputs = new Map<string, string>();
  private readonly wired = new WeakSet<JsonRpcClient>();
  private changesTimer: NodeJS.Timeout | undefined;
  /** Approvals for runtime tasks that are not this chat's (background tasks, best-of-N). */
  foreignApproval?: (req: ApprovalRequest) => Promise<{ decision: ApprovalDecision }>;
  /** Called with every finished chat task (review findings, notifications). */
  onSummary?: (s: AgentSummary) => void;
  /** Called when the chat starts or stops working (status bar). */
  onBusyChange?: (busy: boolean) => void;
  /** Opens the project graph panel. */
  onShowGraph?: () => void;
  private includeEditor: boolean;
  private autoCommit: boolean;
  /** Messages sent to the running task that it has not read yet. */
  private steering: string[] = [];
  private files: { at: number; list: string[] } | undefined;
  private indexState: object = { state: 'none' };

  constructor(
    private readonly ctx: vscode.ExtensionContext,
    private readonly getClient: () => Promise<JsonRpcClient>,
    private readonly onAddProvider: () => void,
    private readonly originals: OriginalContent,
    private readonly log: (level: 'info' | 'warn' | 'error', msg: string) => void = () => {},
  ) {
    this.items = ctx.workspaceState.get<Item[]>(TRANSCRIPT_KEY, []);
    this.includeEditor = ctx.workspaceState.get<boolean>(INCLUDE_EDITOR_KEY, true);
    this.autoCommit = ctx.workspaceState.get<boolean>(AUTO_COMMIT_KEY, false);
    this.taskId = ctx.workspaceState.get<string>(TASK_KEY);
  }

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    const media = vscode.Uri.joinPath(this.ctx.extensionUri, 'media');
    view.webview.options = { enableScripts: true, localResourceRoots: [media] };
    view.webview.html = this.html(view.webview, media);
    view.webview.onDidReceiveMessage((m: Inbound) => this.onMessage(m), undefined, this.ctx.subscriptions);
  }

  get busy(): boolean {
    return this.abort !== undefined;
  }

  cancel(): void {
    this.abort?.abort();
    for (const resolve of this.approvals.values()) resolve('deny');
    this.approvals.clear();
    this.queue.length = 0;
    this.post({ type: 'queue', items: [] });
  }

  async clear(): Promise<void> {
    this.cancel();
    this.items = [];
    this.taskId = undefined;
    this.diffs.clear();
    this.outputs.clear();
    await this.persist();
    this.post({ type: 'history', items: [] });
  }

  /** Starts an empty chat: a new task, no transcript, and the context meter reset. */
  async newChat(): Promise<void> {
    await this.clear();
    this.post({ type: 'contextUsage', used: 0, window: 0 });
  }

  invalidateModels(): void {
    this.listed.clear();
  }

  copySelection(): void {
    this.post({ type: 'copySelection' });
  }

  refreshStatus(): void {
    const ref = activeRef();
    const approval = vscode.workspace.getConfiguration(SECTION).get<string>('agent.approvalMode', 'askBeforeExecution');
    const claude = engine() === 'claudeCode';
    this.post({ type: 'status', provider: claude ? CLAUDE_LABEL : refLabel(ref), model: claude ? claudeModel() : modelFor(ref) || '(no model)', busy: this.busy, mode: chatMode(), approval, autoCommit: this.autoCommit });
    void this.postModels();
  }

  dispose(): void {
    this.cancel();
  }

  private post(msg: object): void {
    void this.view?.webview.postMessage(msg);
  }

  private async persist(): Promise<void> {
    this.items = this.items.slice(-MAX_ITEMS);
    await this.ctx.workspaceState.update(TRANSCRIPT_KEY, this.items);
    await this.ctx.workspaceState.update(TASK_KEY, this.taskId);
  }

  private add(item: Item): void {
    this.items.push(item);
    this.post({ type: 'item', item });
  }

  private setBusy(abort: AbortController | undefined): void {
    this.abort = abort;
    void vscode.commands.executeCommand('setContext', 'task-x.busy', this.busy);
    this.onBusyChange?.(this.busy);
    this.refreshStatus();
  }

  /** Configured providers with their saved model, plus models listed from the active endpoint. */
  private async postModels(): Promise<void> {
    const active = activeRef();
    const groups = [];
    for (const ref of providerRefs()) {
      const saved = modelFor(ref);
      const ready = ref === active || (saved && (!refNeedsKey(ref) || (await hasApiKey(this.ctx.secrets, ref))));
      if (!ready) continue;
      const models = [...new Set([saved, ...(this.listed.get(ref) ?? [])].filter(Boolean))].slice(0, MAX_PICKER_MODELS);
      groups.push({ kind: ref, label: refLabel(ref), models });
    }
    const auth = await claudeAuth();
    if (auth.loggedIn) groups.push({ kind: CLAUDE_KIND, label: auth.email ? `${CLAUDE_LABEL} · ${auth.email}` : CLAUDE_LABEL, models: [...new Set([claudeModel(), ...CLAUDE_MODELS])] });
    const current = engine() === 'claudeCode' ? { kind: CLAUDE_KIND, model: claudeModel() } : { kind: active, model: modelFor(active) };
    this.post({ type: 'models', active: current, groups, auth });
  }

  private async loadModels(): Promise<void> {
    const kind = activeRef();
    if (this.listed.has(kind)) return;
    this.listed.set(kind, []);
    try {
      const provider = await providerConfig(this.ctx.secrets, kind);
      const client = await this.getClient();
      const { models } = await client.request<{ models: string[] }>('provider/listModels', { provider }, { timeoutMs: settings().timeoutMs });
      this.listed.set(kind, models);
    } catch {
      // The picker still offers the saved model; listing is best effort.
      this.listed.delete(kind);
      return;
    }
    await this.postModels();
  }

  private async onMessage(m: Inbound): Promise<void> {
    switch (m?.type) {
      case 'ready':
        this.post({ type: 'history', items: this.items });
        this.refreshStatus();
        this.postEditor();
        this.post({ type: 'index', index: this.indexState });
        void this.refreshChanges();
        break;
      case 'listFiles':
        if (typeof m.query === 'string') await this.postFiles(m.query.slice(0, 300));
        break;
      case 'toggleEditor':
        this.includeEditor = m.include === true;
        await this.ctx.workspaceState.update(INCLUDE_EDITOR_KEY, this.includeEditor);
        this.postEditor();
        break;
      case 'showGraph':
        this.onShowGraph?.();
        break;
      case 'listTasks':
        await this.postTasks();
        break;
      case 'openTask':
        if (typeof m.id === 'string' && m.id.length <= 100) {
          await this.openTask(m.id).catch((e) => vscode.window.showErrorMessage(`task-x: ${describeError(e)}`));
        }
        break;
      case 'keep':
      case 'revert':
        await this.settle(m.type, Array.isArray(m.paths) ? m.paths.filter((p) => typeof p === 'string') : undefined);
        break;
      case 'diffFile':
        if (typeof m.path === 'string') await this.diffFile(m.path);
        break;
      case 'send':
        if (typeof m.text === 'string') await (this.busy && m.queue !== true ? this.steer(m.text) : this.send(m.text));
        break;
      case 'cancel':
        this.cancel();
        break;
      case 'clear':
        await this.newChat();
        break;
      case 'loadModels':
        await this.loadModels();
        break;
      case 'addProvider':
        this.onAddProvider();
        break;
      case 'copyText':
        if (typeof m.text === 'string') await vscode.env.clipboard.writeText(m.text.slice(0, MAX_INPUT_CHARS * 10));
        break;
      case 'listSkills':
        await this.postSkills();
        break;
      case 'signIn':
        await claudeSignIn(() => this.refreshStatus());
        break;
      case 'signOut':
        await claudeSignOut(() => this.refreshStatus());
        break;
      case 'selectModel':
        if (this.busy || typeof m.model !== 'string' || !m.model.trim() || m.model.length > 256) this.refreshStatus();
        else if (m.kind === CLAUDE_KIND) await useClaude(m.model.trim());
        else if (isProviderRef(m.kind)) {
          await vscode.workspace.getConfiguration(SECTION).update('engine', 'task-x', vscode.ConfigurationTarget.Global);
          await setProvider(m.kind, m.model.trim());
        } else this.refreshStatus();
        break;
      case 'selectMode':
        if (USER_MODES.includes(m.mode as ChatMode)) await vscode.workspace.getConfiguration(SECTION).update('chatMode', m.mode, vscode.ConfigurationTarget.Global);
        break;
      case 'approve': {
        const d = (['allow', 'allowSession', 'always', 'deny'] as const).find((x) => x === m.decision) ?? 'deny';
        this.approvals.get(m.callId)?.(d);
        this.approvals.delete(m.callId);
        break;
      }
      case 'selectApproval':
        if (APPROVAL_MODES.includes(m.mode as ApprovalMode)) await vscode.workspace.getConfiguration(SECTION).update('agent.approvalMode', m.mode, vscode.ConfigurationTarget.Global);
        break;
      case 'selectAutoCommit':
        this.autoCommit = m.enabled === true;
        await this.ctx.workspaceState.update(AUTO_COMMIT_KEY, this.autoCommit);
        this.refreshStatus();
        break;
      case 'openSettings':
        await vscode.commands.executeCommand('workbench.action.openSettings', typeof m.query === 'string' ? m.query : 'anvil');
        break;
      case 'copyOutput': {
        const text = this.outputs.get(m.id) ?? this.savedOutput(m.id);
        if (text !== undefined) {
          await vscode.env.clipboard.writeText(text);
          vscode.window.setStatusBarMessage('$(copy) task-x: output copied', 2_000);
        }
        break;
      }
      case 'openOutput': {
        const text = this.outputs.get(m.id) ?? this.savedOutput(m.id);
        if (text !== undefined) await vscode.window.showTextDocument(await vscode.workspace.openTextDocument({ language: 'log', content: text }), { preview: true });
        break;
      }
      case 'suggest':
        if (typeof m.text === 'string' && USER_MODES.includes(m.mode as ChatMode)) await this.send(m.text, m.mode as ChatMode);
        break;
      case 'openDiff': {
        if (typeof m.path === 'string' && this.originals.files.has(m.path)) return this.diffFile(m.path);
        const diff = this.diffs.get(m.diffKey);
        if (diff) await vscode.window.showTextDocument(await vscode.workspace.openTextDocument({ language: 'diff', content: diff }), { preview: true });
        else vscode.window.showInformationMessage('task-x: this diff is no longer available (diffs are kept for the current session).');
        break;
      }
      case 'openFile':
        await this.openFile(m.path);
        break;
      case 'undo':
        if (Number.isInteger(m.operation)) await this.undo(m.operation);
        break;
      case 'showProblems':
        await vscode.commands.executeCommand('workbench.actions.view.problems');
        break;
      case 'stopLane': {
        const lane = this.lane(m.taskId);
        if (lane?.status === 'running') await (await this.getClient()).request('agent/cancelSubagent', { taskId: lane.taskId }).catch((e) => this.log('warn', `cancelSubagent: ${describeError(e)}`));
        break;
      }
      case 'applyWorktree':
      case 'discardWorktree':
        if (typeof m.name === 'string') await this.settleWorktree(m.name, m.type === 'applyWorktree');
        break;
      case 'openWorktree': {
        // Only folders the runtime reported for a lane in this chat; never a path from the webview.
        const dir = this.items.find((i): i is Lane => i.kind === 'lane' && i.name === m.name && !!i.dir && !i.merge)?.dir;
        if (dir) await vscode.commands.executeCommand('vscode.openFolder', vscode.Uri.file(dir), { forceNewWindow: true });
        break;
      }
    }
  }

  /** The active file and selection that will go with the next request, for the chips in the input. */
  postEditor(): void {
    const ed = vscode.window.activeTextEditor;
    const rel = ed && WorkspaceSync.relative(ed.document.uri);
    const sel = ed && rel && !ed.selection.isEmpty ? { start: ed.selection.start.line + 1, end: ed.selection.end.line + (ed.selection.end.character === 0 && ed.selection.end.line > ed.selection.start.line ? 0 : 1) } : null;
    this.post({ type: 'editor', file: rel ?? null, selection: sel, include: this.includeEditor });
  }

  /** Saved tasks for the History tab, newest first. */
  private async postTasks(): Promise<void> {
    try {
      const rows = await (await this.getClient()).request<TaskRow[]>('agent/tasks');
      const tasks = rows
        .filter((t) => typeof t?.id === 'string')
        .sort((a, b) => (b.updatedMs ?? 0) - (a.updatedMs ?? 0))
        .map((t) => ({ id: t.id, title: t.title || '(untitled)', status: t.status, mode: t.chatMode, updatedMs: t.updatedMs, files: t.changedFiles?.length ?? 0 }));
      this.post({ type: 'tasks', tasks, current: this.taskId ?? null });
    } catch (e) {
      this.post({ type: 'tasks', tasks: [], current: this.taskId ?? null, error: describeError(e) });
    }
  }

  /** Indexing status for the input's index button and the welcome screen. */
  setIndexState(state: object): void {
    this.indexState = state;
    this.post({ type: 'index', index: state });
  }

  /** Workspace files and folders for the `@` menu, ranked by how well they match. */
  private async postFiles(query: string): Promise<void> {
    if (!this.files || Date.now() - this.files.at > FILE_LIST_TTL_MS) {
      const uris = await vscode.workspace.findFiles('**/*', FILE_EXCLUDE, MAX_LISTED_FILES);
      const list = uris.map((u) => WorkspaceSync.relative(u)).filter((p): p is string => !!p);
      const dirs = new Set<string>();
      for (const f of list) for (let i = f.indexOf('/'); i > 0; i = f.indexOf('/', i + 1)) dirs.add(f.slice(0, i) + '/');
      this.files = { at: Date.now(), list: [...list, ...dirs].sort() };
    }
    const q = query.toLowerCase();
    const score = (p: string) => {
      const l = p.toLowerCase(), name = l.replace(/\/$/, '').split('/').pop() ?? l;
      if (!q) return p.split('/').length;
      if (name.startsWith(q)) return 0 + p.length / 1000;
      if (name.includes(q)) return 1 + p.length / 1000;
      if (l.includes(q)) return 2 + p.length / 1000;
      // Characters in order (fuzzy).
      let i = 0;
      for (const ch of l) if (ch === q[i]) i++;
      return i === q.length ? 3 + p.length / 1000 : Infinity;
    };
    const items = this.files.list
      .map((p) => ({ p, s: score(p) }))
      .filter((x) => x.s !== Infinity)
      .sort((a, b) => a.s - b.s)
      .slice(0, MAX_FILE_ITEMS)
      .map((x) => x.p);
    this.post({ type: 'files', query, items });
  }

  private lane(taskId: string): Lane | undefined {
    return this.items.find((i): i is Lane => i.kind === 'lane' && i.taskId === taskId);
  }

  private updateLane(lane: Lane, patch: Partial<Lane>): void {
    Object.assign(lane, patch);
    this.post({ type: 'laneUpdate', item: lane });
  }

  /** Applies a kept subagent worktree through the edit log (reviewable, undoable) or discards it. */
  async settleWorktree(name: string, apply: boolean): Promise<boolean> {
    try {
      const client = await this.getClient();
      const r = await client.request<{ applied?: boolean; files?: string[] }>(apply ? 'worktrees/apply' : 'worktrees/discard', { name });
      for (const it of this.items) if (it.kind === 'lane' && it.name === name) this.updateLane(it, { merge: apply ? 'merged' : 'discarded', mergeError: undefined });
      if (apply && r.applied === false) vscode.window.showInformationMessage(`task-x: ${name} had no changes to apply.`);
      await this.persist();
      await this.refreshChanges();
      return true;
    } catch (e) {
      vscode.window.showErrorMessage(`task-x: ${apply ? 'apply' : 'discard'} failed: ${describeError(e)}`);
      return false;
    }
  }

  /** Loads the review list (net agent changes not yet kept or undone) into the panel above the input. */
  async refreshChanges(): Promise<void> {
    clearTimeout(this.changesTimer);
    try {
      const client = await this.getClient();
      const files = await client.request<PendingFile[]>('edits/pending');
      for (const f of files) this.originals.set(f.path, f.original);
      this.post({
        type: 'changes',
        files: files.map((f) => ({ path: f.path, added: f.added, removed: f.removed, status: f.original === null ? 'added' : f.current === null ? 'deleted' : 'modified' })),
      });
    } catch {
      // No runtime or workspace yet: nothing to review.
      this.post({ type: 'changes', files: [] });
    }
  }

  private queueChanges(): void {
    clearTimeout(this.changesTimer);
    this.changesTimer = setTimeout(() => void this.refreshChanges(), 150);
  }

  /** Auto-commit: commits the files this task changed, then keeps them in the edit log. */
  private async commitTask(summary: AgentSummary, prompt: string): Promise<void> {
    const root = WorkspaceSync.root();
    if (!this.autoCommit || !root || !summary.changedFiles.length) return;
    const paths = summary.changedFiles.filter((p) => !path.isAbsolute(p) && !p.split(/[\\/]/).includes('..'));
    try {
      const r = await commitPaths(root, paths, commitSubject(prompt));
      if (!r.committed) return void this.add({ kind: 'context', text: `Auto-commit skipped: ${r.reason}.` });
      this.add({ kind: 'context', text: `Committed ${paths.length} file${paths.length > 1 ? 's' : ''}: ${r.subject}` });
      await this.settle('keep', paths);
    } catch (e) {
      this.add({ kind: 'context', text: `Auto-commit failed: ${describeError(e)}` });
    }
  }

  async settle(kind: 'keep' | 'revert', paths?: string[]): Promise<void> {
    try {
      const client = await this.getClient();
      if (kind === 'keep') {
        await client.request('edits/keep', { paths });
        for (const it of this.items) if (it.kind === 'file' && !it.undone && (!paths || paths.includes(it.path))) it.kept = true;
        await this.persist();
        this.post({ type: 'history', items: this.items });
      } else {
        const r = await client.request<{ reverted: string[]; conflicts: { path: string }[] }>('edits/revert', { paths });
        if (r.conflicts.length) vscode.window.showWarningMessage(`task-x kept ${r.conflicts.map((c) => c.path).join(', ')} because it changed after the agent's edit.`);
        for (const it of this.items) if (it.kind === 'file' && r.reverted.includes(it.path)) it.undone = true;
        await this.persist();
        this.post({ type: 'history', items: this.items });
      }
    } catch (e) {
      vscode.window.showErrorMessage(`task-x: ${describeError(e)}`);
    }
    await this.refreshChanges();
  }

  async diffFile(rel: string): Promise<void> {
    const root = WorkspaceSync.root();
    if (!root || rel.split('/').includes('..')) return;
    if (!this.originals.files.has(rel)) await this.refreshChanges();
    const left = vscode.Uri.from({ scheme: ORIGINAL_SCHEME, path: `/${rel}` });
    const file = vscode.Uri.joinPath(vscode.Uri.file(root), ...rel.split('/'));
    const exists = await vscode.workspace.fs.stat(file).then(() => true, () => false);
    const right = exists ? file : vscode.Uri.from({ scheme: ORIGINAL_SCHEME, path: '/', query: 'deleted' });
    await vscode.commands.executeCommand('vscode.diff', left, right, `${rel.split('/').pop()} (task-x changes)`, { preview: true });
  }

  private savedOutput(id: string): string | undefined {
    const it = this.items.find((i): i is Extract<Item, { kind: 'tool' }> => i.kind === 'tool' && i.id === id);
    return it?.output;
  }

  /** Records a finished call's output and returns what the transcript shows. */
  private captureOutput(id: string, tool: string, ok: boolean, raw: string): { output?: string; outputCut?: boolean } {
    const base = tool.split(' › ').pop() ?? tool;
    if (ok && !OUTPUT_TOOLS.has(base)) return {};
    const text = formatOutput(raw);
    this.outputs.set(id, text);
    return { output: text.slice(0, MAX_SAVED_OUTPUT), outputCut: text.length > MAX_SAVED_OUTPUT };
  }

  private async openFile(rel: string): Promise<void> {
    const root = WorkspaceSync.root();
    if (!root || typeof rel !== 'string' || rel.split('/').includes('..')) return;
    await vscode.window.showTextDocument(vscode.Uri.joinPath(vscode.Uri.file(root), ...rel.split('/')), { preview: true });
  }

  async undo(operation: number): Promise<void> {
    try {
      const client = await this.getClient();
      const r = await client.request<{ restored: string[]; conflicts: { path: string; reason: string }[] }>('edits/undo', { operation });
      for (const it of this.items) if (it.kind === 'file' && it.operation === operation && r.restored.includes(it.path)) it.undone = true;
      await this.persist();
      this.post({ type: 'history', items: this.items });
      void this.refreshChanges();
      const msg = r.conflicts.length
        ? `Restored ${r.restored.length} file(s). Kept ${r.conflicts.map((c) => c.path).join(', ')} because it changed after the edit.`
        : `Undid the edit to ${r.restored.join(', ')}.`;
      (r.conflicts.length ? vscode.window.showWarningMessage : vscode.window.showInformationMessage)(msg);
    } catch (e) {
      vscode.window.showErrorMessage(`task-x: undo failed: ${describeError(e)}`);
    }
  }

  async undoLast(): Promise<void> {
    const last = [...this.items].reverse().find((i): i is Extract<Item, { kind: 'file' }> => i.kind === 'file' && !i.undone && i.operation != null);
    if (!last) return void vscode.window.showInformationMessage('task-x: no agent edit to undo in this chat.');
    await this.undo(last.operation!);
  }

  private wire(client: JsonRpcClient): void {
    if (this.wired.has(client)) return;
    this.wired.add(client);
    client.onRequest('agent/requestApproval', (p) => this.requestApproval(p as ApprovalRequest));
  }

  private requestApproval(req: ApprovalRequest): Promise<{ decision: ApprovalDecision }> {
    if (!this.taskId || !req.taskId.startsWith(this.taskId) || !this.busy) return this.foreignApproval?.(req) ?? Promise.resolve({ decision: 'deny' });
    this.view?.show?.(true);
    this.notify('input', `task-x needs your approval to use ${req.tool}`, 'Review');
    this.post({
      type: 'approval',
      callId: req.callId,
      tool: req.tool,
      permission: req.permission,
      detail: toolDetail(req.tool, req.input),
      preview: req.preview ?? JSON.stringify(req.input, null, 2).slice(0, 6_000),
    });
    return new Promise((resolve) => {
      this.approvals.set(req.callId, (decision) => {
        this.post({ type: 'approvalDone', callId: req.callId, decision });
        if (decision === 'always') {
          // Persist the user's choice; the runtime treats it like a session grant for this run.
          const c = vscode.workspace.getConfiguration(SECTION);
          const allow = new Set(c.get<string[]>('agent.allowTools', []));
          allow.add(req.tool);
          void c.update('agent.allowTools', [...allow], vscode.ConfigurationTarget.Global);
        }
        resolve({ decision: decision === 'always' ? 'allowSession' : decision });
      });
    });
  }

  private editorContext(prompt = ''): EditorContext {
    const ed = this.includeEditor ? vscode.window.activeTextEditor : undefined;
    const ctx: EditorContext = { openFiles: [], diagnostics: [] };
    const mentions = [...new Set([...prompt.matchAll(MENTION)].map((m) => m[1]).filter((p) => !p.split('/').includes('..')))].slice(0, MAX_MENTIONS);
    if (mentions.length) ctx.mentions = mentions;
    const rel = ed && WorkspaceSync.relative(ed.document.uri);
    if (ed && rel) {
      ctx.activeFile = rel;
      if (!ed.selection.isEmpty) {
        const text = ed.document.getText(ed.selection);
        if (text.length <= MAX_SELECTION_CHARS) ctx.selection = { path: rel, startLine: ed.selection.start.line + 1, endLine: ed.selection.end.line + 1, text };
      }
    }
    if (ed && rel) {
      const range = ed.selection.isEmpty ? undefined : ed.selection;
      ctx.diagnostics = vscode.languages
        .getDiagnostics(ed.document.uri)
        .filter((d) => d.severity <= vscode.DiagnosticSeverity.Warning && (!range || range.intersection(d.range)))
        .slice(0, MAX_DIAGNOSTICS)
        .map((d) => ({ path: rel, line: d.range.start.line + 1, severity: SEVERITY[d.severity], message: d.message.slice(0, 1_000), source: d.source }));
    }
    ctx.openFiles = vscode.window.visibleTextEditors
      .map((e) => WorkspaceSync.relative(e.document.uri))
      .filter((p): p is string => !!p && p !== rel)
      .slice(0, MAX_OPEN_FILES);
    return ctx;
  }

  /** Agent activity for the Logs panel: names and statuses only, never prompt or model text. */
  private trace(e: AgentEvent, prefix = ''): void {
    switch (e.type) {
      case 'started': return this.log('info', `${prefix}task started (${e.chatMode}, ${e.enrichment.kind}, ${e.tools.length} tools)`);
      case 'iteration': return this.log('info', `${prefix}turn ${e.n}`);
      case 'toolStarted': return this.log('info', `${prefix}tool ${e.tool} started (${e.permission})`);
      case 'toolFinished': return this.log(e.ok ? 'info' : 'warn', `${prefix}tool ${e.tool} ${e.ok ? 'ok' : 'failed'}`);
      case 'approvalRequested': return this.log('info', `${prefix}approval requested for ${e.tool}`);
      case 'fileChanged': return this.log('info', `${prefix}file changed: ${e.path}`);
      case 'retry': return this.log('warn', `${prefix}retry ${e.attempt} in ${e.waitMs} ms: ${e.reason}`);
      case 'error': return this.log('error', `${prefix}${e.error.kind}: ${e.error.message}`);
      case 'warning': return this.log('warn', `${prefix}${e.message}`);
      case 'modelSwitched': return this.log('warn', `${prefix}model ${e.from.kind}/${e.from.model} → ${e.to.kind}/${e.to.model}: ${e.reason}`);
      case 'model': return this.log('info', `${prefix}model ${e.kind}/${e.model}: ${e.reason}`);
      case 'compacted': return this.log('info', `${prefix}compacted ${e.folded} messages`);
      case 'hook': return this.log(e.blocked ? 'warn' : 'info', `${prefix}hook ${e.event}${e.blocked ? ' blocked' : ''}: ${e.command}`);
      case 'subagent': return this.trace(e.event, `${prefix}[${e.name}] `);
      case 'subagentStarted': return this.log('info', `${prefix}subagent ${e.name} started (${e.role}, ${e.kind}/${e.model})`);
      case 'subagentFinished': return this.log(e.status === 'completed' ? 'info' : 'warn', `${prefix}subagent ${e.name} ${e.status}`);
      case 'planStarted': return this.log('info', `${prefix}plan with ${e.steps.length} steps, up to ${e.maxParallel} in parallel`);
      case 'merged': return this.log(e.ok ? 'info' : 'warn', `${prefix}merge ${e.name}: ${e.ok ? 'ok' : e.error}`);
      case 'done': {
        const s = e.summary;
        return this.log(s.status === 'completed' ? 'info' : 'warn', `${prefix}task ${s.status}: ${s.iterations} turns, ${s.toolCalls} tool calls, ${s.changedFiles.length} files${s.error ? ` — ${s.error}` : ''}`);
      }
    }
  }

  private onEvent(e: AgentEvent, state: { assistant?: Extract<Item, { kind: 'assistant' }> }): void {
    this.trace(e);
    switch (e.type) {
      case 'started': {
        const en = e.enrichment;
        this.add({
          kind: 'taskInfo',
          taskKind: en.kind,
          label: TASK_KIND_LABELS[en.kind] ?? en.kind,
          files: [...new Set(en.sources.map((s) => s.path))],
          instructions: en.instructions.map((p) => ({ label: shortPath(p), path: p })),
          skills: (e.skills ?? []).map((k) => k.name),
        });
        break;
      }
      case 'textDelta':
        if (!state.assistant) {
          state.assistant = { kind: 'assistant', text: '' };
          this.add(state.assistant);
        }
        state.assistant.text += e.text;
        this.post({ type: 'delta', text: e.text });
        break;
      case 'retry':
        if (state.assistant) {
          this.items = this.items.filter((i) => i !== state.assistant);
          state.assistant = undefined;
          this.post({ type: 'history', items: this.items });
        }
        this.add({ kind: 'context', text: `Retrying (${e.attempt}) after: ${e.reason}` });
        break;
      case 'toolStarted':
        state.assistant = undefined;
        this.add({ kind: 'tool', id: e.callId, tool: e.tool, detail: toolDetail(e.tool, e.input) });
        break;
      case 'toolFinished': {
        state.assistant = undefined;
        const it = this.items.find((i): i is Extract<Item, { kind: 'tool' }> => i.kind === 'tool' && i.id === e.callId);
        const out = this.captureOutput(e.callId, e.tool, e.ok, e.output);
        if (it) {
          Object.assign(it, { ok: e.ok }, out);
          this.post({ type: 'toolDone', id: e.callId, ok: e.ok, ...out });
        } else {
          // Rejected before it ran (not available, denied, or repeated failure).
          this.add({ kind: 'tool', id: e.callId, tool: e.tool, detail: 'not run', ok: false, ...out });
        }
        break;
      }
      case 'steered': {
        const i = this.steering.indexOf(e.text);
        if (i >= 0) this.steering.splice(i, 1);
        this.add({ kind: 'steer', text: e.text });
        this.postQueue();
        break;
      }
      case 'fileChanged': {
        this.queueChanges();
        const diffKey = `${Date.now()}-${this.diffs.size}`;
        this.diffs.set(diffKey, e.diff);
        const { added, removed } = diffStats(e.diff);
        this.add({ kind: 'file', path: e.path, operation: e.operation, diffKey, added, removed, diff: e.diff.slice(0, MAX_CARD_DIFF), diffCut: e.diff.length > MAX_CARD_DIFF });
        break;
      }
      case 'error':
        this.add({ kind: 'error', text: `${e.error.kind}: ${e.error.message}` });
        break;
      case 'warning':
        this.add({ kind: 'context', text: e.message });
        break;
      case 'subagentStarted':
        this.add({ kind: 'lane', taskId: e.taskId, name: e.name, role: e.role, objective: e.objective, model: e.model, step: e.step, status: 'running', steps: [], dir: e.dir });
        break;
      case 'subagentFinished': {
        const lane = this.lane(e.taskId);
        if (lane)
          this.updateLane(lane, {
            status: e.status,
            error: e.error ?? undefined,
            tokens: e.tokens,
            cost: formatCost(e.costUsd) || (e.unpricedTokens ? 'cost unknown' : undefined),
            files: e.files?.length,
            added: e.added,
            removed: e.removed,
            worktree: e.worktree,
          });
        break;
      }
      case 'planStarted': {
        const names = e.steps.map((st) => (st.dependsOn.length ? `${st.id} (after ${st.dependsOn.join(', ')})` : st.id));
        this.add({ kind: 'context', text: `Plan: ${e.steps.length} steps, up to ${e.maxParallel} at a time · ${names.join(' · ')}` });
        break;
      }
      case 'stepSkipped':
        this.add({ kind: 'context', text: `Step ${e.step} ${e.reason}` });
        break;
      case 'merged':
        for (const it of this.items) if (it.kind === 'lane' && it.name === e.name) this.updateLane(it, e.ok ? { merge: 'merged' } : { merge: 'conflict', mergeError: e.error });
        if (e.ok) this.queueChanges();
        break;
      case 'subagent': {
        const inner = e.event;
        const tag = `${e.name} › `;
        const lane = this.lane(e.taskId);
        if (lane) {
          if (inner.type === 'toolStarted') {
            lane.steps.push({ id: inner.callId, tool: inner.tool, detail: toolDetail(inner.tool, inner.input) });
            if (lane.steps.length > MAX_LANE_STEPS) lane.hidden = (lane.hidden ?? 0) + lane.steps.splice(0, lane.steps.length - MAX_LANE_STEPS).length;
            this.updateLane(lane, {});
          } else if (inner.type === 'toolFinished') {
            const st = lane.steps.find((x) => x.id === inner.callId);
            if (st) this.updateLane(lane, ((st.ok = inner.ok), {}));
          } else if (inner.type === 'approvalRequested') this.view?.show?.(true);
          break;
        }
        if (inner.type === 'started') this.add({ kind: 'context', text: `Subagent ${e.name} started` });
        else if (inner.type === 'toolStarted') this.add({ kind: 'tool', id: `${e.taskId}:${inner.callId}`, tool: tag + inner.tool, detail: toolDetail(inner.tool, inner.input) });
        else if (inner.type === 'toolFinished') {
          const it = this.items.find((i): i is Extract<Item, { kind: 'tool' }> => i.kind === 'tool' && i.id === `${e.taskId}:${inner.callId}`);
          if (it) {
            const out = this.captureOutput(it.id, inner.tool, inner.ok, inner.output);
            Object.assign(it, { ok: inner.ok }, out);
            this.post({ type: 'toolDone', id: it.id, ok: inner.ok, ...out });
          }
        } else if (inner.type === 'done') this.add({ kind: 'context', text: `Subagent ${e.name}: ${summaryText(inner.summary).text}` });
        break;
      }
      case 'modelSwitched':
        this.add({ kind: 'context', text: `Switched model ${e.from.model} → ${e.to.model} (${e.reason})` });
        break;
      case 'model':
        this.add({ kind: 'context', text: `Using ${e.model} (${e.reason})` });
        break;
      case 'compacted':
        this.add({ kind: 'compressed', before: e.beforeTokens ?? 0, after: e.afterTokens ?? 0, window: e.windowTokens ?? 0, pruned: e.pruned ?? 0, folded: e.folded, dropped: !!e.droppedRetrieval });
        break;
      case 'context':
        this.post({ type: 'contextUsage', used: e.usedTokens, window: e.windowTokens });
        break;
      case 'hook':
        if (e.blocked) this.add({ kind: 'context', text: `Hook blocked ${e.event}: ${e.message}` });
        break;
    }
  }

  /** Sends a prompt from a command (context menu etc.), optionally in a specific mode. */
  async submit(text: string, mode?: ChatMode): Promise<void> {
    await vscode.commands.executeCommand(`${ChatViewProvider.viewId}.focus`);
    if (this.busy) return void vscode.window.showWarningMessage('task-x is busy. Stop the current request first.');
    await this.send(text, mode);
  }

  /** Switches the chat to an earlier task and shows its conversation. */
  async openTask(taskId: string): Promise<void> {
    if (this.busy) return void vscode.window.showWarningMessage('task-x is busy. Stop the current request first.');
    const client = await this.getClient();
    const t = await client.request<{ summary: AgentSummary; messages: { role: 'user' | 'assistant'; content: { type: string; text?: string }[] }[] }>('agent/task', { taskId });
    const items: Item[] = [];
    for (const m of t.messages) {
      const text = m.content.flatMap((c) => (c.type === 'text' && c.text ? [c.text] : [])).join('\n');
      if (!text) continue;
      if (m.role === 'user') {
        const req = text.match(/<user_request>\n([\s\S]*)\n<\/user_request>$/);
        items.push({ kind: 'user', text: req ? req[1] : text, mode: t.summary.chatMode });
      } else {
        items.push({ kind: 'assistant', text });
      }
    }
    items.push({ kind: 'summary', ...summaryText(t.summary) });
    this.items = items;
    this.taskId = taskId;
    this.diffs.clear();
    await this.persist();
    this.post({ type: 'history', items: this.items });
    this.post({ type: 'contextUsage', used: 0, window: 0 });
    this.post({ type: 'view', view: 'chat' });
    await vscode.commands.executeCommand(`${ChatViewProvider.viewId}.focus`);
  }

  get currentTaskId(): string | undefined {
    return this.taskId;
  }

  private async send(raw: string, modeOverride?: ChatMode): Promise<void> {
    const text = raw.trim();
    if (!text) return;
    if (text.length > MAX_INPUT_CHARS) return void this.add({ kind: 'error', text: `Message is longer than ${MAX_INPUT_CHARS} characters.` });
    if (this.busy) {
      if (this.queue.length >= MAX_QUEUE) return void this.add({ kind: 'error', text: `Only ${MAX_QUEUE} messages can wait. Stop the task or wait for one to finish.` });
      this.queue.push({ text, mode: modeOverride });
      return void this.postQueue();
    }
    const mode = modeOverride ?? chatMode();
    const abort = new AbortController();
    this.setBusy(abort);
    this.add({ kind: 'user', text, mode });
    let off = () => {};
    if (engine() === 'claudeCode') {
      try {
        await this.sendClaude(text, mode, abort.signal, (o) => (off = o));
      } catch (e) {
        this.add({ kind: 'error', text: describeError(e), retryText: e instanceof RpcError && e.cancelled ? undefined : text });
      } finally {
        off();
        this.setBusy(undefined);
        await this.persist();
        this.drain();
      }
      return;
    }
    try {
      const client = await this.getClient();
      this.wire(client);
      this.taskId ??= `c${Date.now()}${randomBytes(3).toString('hex')}`;
      const taskId = this.taskId;
      const params = await runParams(this.ctx.secrets, { prompt: text, mode, taskId, editor: this.editorContext(text) });
      const state: { assistant?: Extract<Item, { kind: 'assistant' }> } = {};
      off = client.onNotification('agent/event', (p) => {
        const { taskId: id, event } = p as AgentEventParams;
        if (id === taskId) this.onEvent(event, state);
      });
      let summary = await client.request<AgentSummary>('agent/run', params, { signal: abort.signal });
      // At the turn limit, the same task continues on its own a few times (task-x.agent.autoContinue).
      const auto = Math.min(5, Math.max(0, Math.floor(vscode.workspace.getConfiguration(SECTION).get<number>('agent.autoContinue', 2))));
      for (let n = 1; n <= auto && summary.status === 'maxIterations' && !abort.signal.aborted; n++) {
        this.add({ kind: 'context', text: `Reached the turn limit after ${summary.iterations} turns. Continuing automatically (${n} of ${auto}).` });
        state.assistant = undefined;
        summary = await client.request<AgentSummary>('agent/run', { ...params, prompt: CONTINUE_PROMPT }, { signal: abort.signal });
      }
      this.add({ kind: 'summary', ...summaryText(summary) });
      await this.commitTask(summary, text);
      this.finished(summary, text);
      void this.refreshChanges();
    } catch (e) {
      this.add({ kind: 'error', text: describeError(e), retryText: e instanceof RpcError && e.cancelled ? undefined : text });
    } finally {
      off();
      for (const it of this.items) if (it.kind === 'lane' && it.status === 'running') it.status = 'cancelled';
      this.approvals.clear();
      // Steers the task never read are sent again as the next message, so nothing is lost.
      this.queue.push(...this.steering.splice(0).map((text) => ({ text })));
      this.setBusy(undefined);
      await this.persist();
      this.drain();
    }
  }

  /** Shows the messages waiting for the task: steers first, then queued messages. */
  private postQueue(): void {
    this.post({ type: 'queue', items: this.queue.map((q) => q.text), steering: this.steering });
  }

  /** While a task runs, Enter steers it: the agent reads the message at its next step. Otherwise it is queued. */
  private async steer(text: string): Promise<void> {
    const t = text.trim();
    if (!t || engine() === 'claudeCode' || !this.taskId || this.steering.length >= MAX_STEERS) return this.send(text);
    let accepted = false;
    try {
      accepted = await (await this.getClient()).request<boolean>('agent/steer', { taskId: this.taskId, text: t });
    } catch (e) {
      this.log('warn', `steer failed: ${describeError(e)}`);
    }
    if (!accepted) return this.send(text);
    this.steering.push(t);
    this.postQueue();
  }

  /** Runs the next queued message once the request before it has finished. */
  private drain(): void {
    const next = this.queue.shift();
    this.postQueue();
    if (next) void this.send(next.text, next.mode);
  }

  /** Hooks for a finished task, and a notification when the chat is not on screen. */
  private finished(summary: AgentSummary, prompt: string): void {
    this.onSummary?.(summary);
    const title = prompt.length > 60 ? `${prompt.slice(0, 57)}…` : prompt;
    const { title: status } = summaryText(summary);
    // A final answer that ends with a question is the model asking for input.
    const asks = summary.status === 'completed' && /\?\s*(?:`{3}\s*)?$/.test(summary.text.trim().split('\n').slice(-3).join('\n'));
    if (asks) this.notify('input', `task-x is waiting for your reply · ${title}`, 'Reply');
    else this.notify(summary.status === 'completed' ? 'done' : 'stopped', `task-x: ${status} · ${title}`, 'Open Chat');
  }

  /**
   * Notifications for a finished task or a request for input. `task-x.notifications`: "whenHidden"
   * (default) when the chat is not on screen or VS Code is not focused, "always", or "off".
   */
  private notify(kind: 'done' | 'stopped' | 'input', text: string, action: string): void {
    const when = vscode.workspace.getConfiguration(SECTION).get<string>('notifications', 'whenHidden');
    if (when === 'off' || (when !== 'always' && this.view?.visible && vscode.window.state.focused)) return;
    const show = kind === 'done' ? vscode.window.showInformationMessage : vscode.window.showWarningMessage;
    void show(text, action).then((pick) => pick && vscode.commands.executeCommand(`${ChatViewProvider.viewId}.focus`));
  }

  /** Skills for the `/` menu in the input. */
  private async postSkills(): Promise<void> {
    try {
      const skills = await (await this.getClient()).request<SkillInfo[]>('skills/list');
      this.post({ type: 'skills', skills: skills.map((s) => ({ name: s.name, description: s.description, params: s.params, source: skillOrigin(s.source) })) });
    } catch (e) {
      this.log('warn', `skills/list failed: ${describeError(e)}`);
      this.post({ type: 'skills', skills: [] });
    }
  }

  /**
   * Gives task-x skills to Claude Code. Claude loads `.claude/skills` itself, so only the others are
   * added. A leading `/name` for an added skill is removed so Claude does not read it as its own command.
   */
  private async withSkills(client: JsonRpcClient, prompt: string, paths: string[]): Promise<string> {
    let active: ResolvedSkill[] = [];
    try {
      ({ active } = await client.request<{ active: ResolvedSkill[] }>('skills/resolve', { prompt, paths }));
    } catch (e) {
      this.log('warn', `skills/resolve failed: ${describeError(e)}`);
      return prompt;
    }
    const extra = active.filter((s) => !/[\\/]\.claude[\\/]skills[\\/]/.test(s.source));
    if (!extra.length) return prompt;
    for (const s of extra) this.log('info', `skill ${s.name} added for Claude Code (${s.reason})`);
    this.add({ kind: 'context', text: `Skills: ${extra.map((s) => s.name).join(', ')}` });
    let body = prompt;
    const lead = /^\s*\/([\w.-]+)((?:\s+[\w.-]+=\S*)*)\s*/.exec(body);
    if (lead && extra.some((s) => s.name === lead[1])) body = body.slice(lead[0].length) || `Run the ${lead[1]} skill.`;
    const blocks = extra.map((s) => `<skill name="${s.name}" reason="${s.reason.replace(/"/g, "'")}">\n${s.text.trim()}\n</skill>`).join('\n\n');
    return `Follow these skill instructions for this request:\n\n${blocks}\n\n${body}`;
  }

  /** Runs the request through the Claude Code CLI; follow-ups resume the same Claude session. */
  private async sendClaude(text: string, mode: ChatMode, signal: AbortSignal, setOff: (off: () => void) => void): Promise<void> {
    const cliPath = findClaude();
    if (!cliPath) throw new Error('Claude Code CLI not found. Install it (https://docs.anthropic.com/claude-code) or set task-x.claudeCode.path.');
    if (!(await claudeAuth()).loggedIn) {
      void vscode.window.showWarningMessage('Sign in to your Anthropic account to use Claude Code.', 'Sign In').then((pick) => pick && claudeSignIn(() => this.refreshStatus()));
      throw new Error('Not signed in to Anthropic. Run "task-x: Sign In with Anthropic".');
    }
    const client = await this.getClient();
    this.taskId ??= `c${Date.now()}${randomBytes(3).toString('hex')}`;
    const taskId = this.taskId;
    const c = vscode.workspace.getConfiguration(SECTION);
    const approval = c.get<string>('agent.approvalMode', 'askBeforeExecution') as ApprovalMode;
    const sessions = this.ctx.workspaceState.get<Record<string, string>>(CLAUDE_SESSIONS_KEY, {});
    const ed = this.editorContext();
    let prompt = text;
    if (ed.selection) prompt += `\n\nSelected code in ${ed.selection.path} (lines ${ed.selection.startLine}-${ed.selection.endLine}):\n\`\`\`\n${ed.selection.text}\n\`\`\``;
    else if (ed.activeFile) prompt += `\n\n(Active file: ${ed.activeFile})`;
    prompt = await this.withSkills(client, prompt, [ed.selection?.path, ed.activeFile].filter((p): p is string => !!p));
    const state: { assistant?: Extract<Item, { kind: 'assistant' }> } = {};
    setOff(
      client.onNotification('agent/event', (p) => {
        const { taskId: id, event } = p as AgentEventParams;
        if (id === taskId) this.onEvent(event, state);
      }),
    );
    const budget = c.get<number>('claudeCode.maxBudgetUsd', 0);
    const model = claudeModel();
    const summary = await client.request<AgentSummary & { sessionId?: string }>(
      'claude/run',
      {
        prompt,
        taskId,
        cliPath,
        chatMode: mode,
        model: model === 'default' ? undefined : model,
        sessionId: sessions[taskId],
        maxBudgetUsd: budget > 0 ? budget : undefined,
        useTaskTools: c.get<boolean>('claudeCode.useTaskXTools', true),
        ...claudePermissions(mode, approval),
      },
      { signal },
    );
    if (summary.sessionId) await this.ctx.workspaceState.update(CLAUDE_SESSIONS_KEY, { ...sessions, [taskId]: summary.sessionId });
    this.add({ kind: 'summary', ...summaryText(summary) });
    await this.commitTask(summary, text);
    this.finished(summary, text);
    void this.refreshChanges();
  }

  private html(webview: vscode.Webview, media: vscode.Uri): string {
    const nonce = randomBytes(16).toString('base64');
    const css = webview.asWebviewUri(vscode.Uri.joinPath(media, 'chat.css'));
    const tokens = webview.asWebviewUri(vscode.Uri.joinPath(media, 'tokens.css'));
    const js = webview.asWebviewUri(vscode.Uri.joinPath(media, 'chat.js'));
    const hljs = webview.asWebviewUri(vscode.Uri.joinPath(media, 'vendor', 'highlight.min.js'));
    const codicons = webview.asWebviewUri(vscode.Uri.joinPath(media, 'codicons', 'codicon.css'));
    const csp = `default-src 'none'; style-src ${webview.cspSource}; font-src ${webview.cspSource}; img-src ${webview.cspSource}; script-src 'nonce-${nonce}';`;
    return /* html */ `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="stylesheet" href="${codicons}">
<link rel="stylesheet" href="${tokens}">
<link rel="stylesheet" href="${css}">
</head>
<body>
<nav id="views" class="views" role="tablist" aria-label="View">
  <button type="button" role="tab" id="tabChat" class="view-tab" aria-controls="log"><span class="codicon codicon-comment-discussion" aria-hidden="true"></span>Chat</button>
  <button type="button" role="tab" id="tabHistory" class="view-tab" aria-controls="history"><span class="codicon codicon-history" aria-hidden="true"></span>History</button>
  <span class="spacer"></span>
  <button type="button" id="newChat" class="action" title="New chat" aria-label="New chat"><span class="codicon codicon-add" aria-hidden="true"></span></button>
</nav>
<div id="progress" role="progressbar" aria-label="Working" aria-hidden="true"></div>
<main id="log" role="log" aria-live="polite" aria-label="Chat"></main>
<section id="history" aria-label="Task history" hidden></section>
<section id="changes" hidden aria-label="Changed files">
  <div class="changes-head">
    <button type="button" id="changesToggle" class="toggle" aria-expanded="true"><span class="codicon codicon-chevron-down chev" aria-hidden="true"></span><span id="changesTitle"></span><span id="changesStat" class="stat"></span></button>
    <span class="spacer"></span>
    <button type="button" id="keepAll" class="btn small" title="Keep all changes">Keep</button>
    <button type="button" id="revertAll" class="btn small secondary" title="Undo all changes">Undo</button>
  </div>
  <ul id="changesList"></ul>
</section>
<form id="form">
  <div id="slash" role="listbox" aria-label="Skills" hidden></div>
  <div id="queue" class="queue" aria-label="Queued messages" hidden></div>
  <div id="ctx" class="ctx" aria-label="Context sent with the message"></div>
  <label for="input" class="sr-only">Message</label>
  <textarea id="input" rows="1"></textarea>
  <div class="toolbar" role="toolbar" aria-label="Chat options">
    <div id="modes" class="tabs" role="tablist" aria-label="Mode"></div>
    <button type="button" id="modelBtn" class="pick"></button>
    <button type="button" id="permBtn" class="pick"></button>
    <label class="pick auto-commit" title="Commit the files each task changes, when the task ends"><input type="checkbox" id="autoCommit"><span class="text">Auto-commit</span></label>
    <span class="spacer"></span>
    <span id="ctxMeter" class="meter" role="meter" aria-valuemin="0" aria-valuemax="100" hidden></span>
    <button type="button" id="indexBtn" class="pick index" hidden></button>
    <button type="button" id="send" class="action primary"></button>
  </div>
</form>
<script nonce="${nonce}" src="${hljs}"></script>
<script nonce="${nonce}" src="${js}"></script>
</body>
</html>`;
  }
}
