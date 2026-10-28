import { randomBytes } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import * as vscode from 'vscode';
import { EDIT_MODES, type EditMode, activeRef, editMode, hasApiKey, isProviderKind, isProviderRef, modelFor, nightModels, setEditMode, providerConfig, providerConfigWith, providerRefs, refKind, refLabel, refNeedsKey, setProvider, setWindowChoice, settings, SECTION } from './config.js';
import {
  type AgentEvent,
  type AgentEventParams,
  type AgentSummary,
  type ApprovalDecision,
  type ApprovalRequest,
  type AskUserRequest,
  type AskUserResult,
  type Question,
  USER_MODES,
  type ChatMode,
  type ContextWindowResult,
  type EditorContext,
  type ImageInput,
  imagesFrom,
  type ModelInfo,
  type ProviderErrorData,
  type RefineResult,
  type ResolvedSkill,
  type SkillInfo,
} from './protocol.js';
import { CLAUDE_KIND, CLAUDE_LABEL, claudeAuth, claudeModel, claudePermissions, claudeSignIn, claudeSignOut, engine, findClaude, useClaude } from './claudeCode.js';
import { CLAUDE_MODEL_INFO, type Listed, MODEL_LIST_TTL_MS, formatTokens, listModels, missingNote, withSaved } from './models.js';
import { type JsonRpcClient, RpcError } from './rpc.js';
import { diffStats, formatOutput } from './output.js';
import { formatCost, limits, routes, runParams } from './runParams.js';
import { commitPaths, commitSubject } from './autoCommit.js';
import { WorkspaceSync } from './workspace.js';
import { claudeMcpServers } from './mcpSources.js';

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
/** Open chat tabs and the one on screen. */
const TABS_KEY = 'task-x.tabs';
const MAX_CHAT_TABS = 20;
const MAX_SUBAGENT_TABS = 12;
/** Longest wait before the changed-files list is loaded again from the edit log while edits arrive. */
const CHANGES_REFRESH_MS = 150;
/** Saved chats for the History tab, newest first; this also keeps Claude Code chats, which the runtime does not store. */
const CHATS_KEY = 'task-x.chats';
const MAX_CHATS = 50;
const MAX_TITLE = 80;
const MAX_ITEMS = 400;
const MAX_INPUT_CHARS = 100_000;
const MAX_SELECTION_CHARS = 20_000;
/** Models per provider in the picker. OpenRouter lists hundreds; a lower cap hid models from search. */
const MAX_PICKER_MODELS = 500;
const MAX_OPEN_FILES = 10;
const MAX_DIAGNOSTICS = 50;
const MAX_QUEUE = 10;
/** Permission classes of tool calls that run a process, which the user can stop alone. */
const STOPPABLE = new Set(['execute', 'install', 'gitWrite', 'network']);
const MAX_STEERS = 10;
/** Output kept in the saved transcript per tool call; the full text stays in memory for the session. */
const MAX_SAVED_OUTPUT = 8_000;
/** Diff kept on each file item so its edit card can show the change after a reload. */
const MAX_CARD_DIFF = 6_000;
const OUTPUT_TOOLS = new Set(['Bash', 'BashOutput', 'run_command', 'run_tests', 'check_build', 'run_linter', 'format_code', 'get_diagnostics', 'git_status', 'git_diff', 'git_log', 'git_show']);
const SEVERITY = ['error', 'warning', 'info', 'hint'];
const CONTINUE_PROMPT = 'Continue the task from where you stopped. Check what is already done, finish the remaining work, then run the build and tests.';
/** Loop Mode (stored under the old "night" keys): the last request runs again until it completes, for free models that drop requests or stall. */
const NIGHT_KEY = 'task-x.nightMode';
const NIGHT_REQUEST_KEY = 'task-x.nightRequest';
const NIGHT_STALL_MS = 10 * 60_000;
const NIGHT_CHECK_MS = 30_000;
const NIGHT_RETRY_BASE_MS = 15_000;
const NIGHT_RETRY_MAX_MS = 5 * 60_000;
/** Night mode with a Think model: it splits the request into todo.md, and the Loop model works on one item per run. */
const NIGHT_TODO_KEY = 'task-x.nightTodo';
const NIGHT_ITEM_KEY = 'task-x.nightItem';
const TODO_FILE = 'todo.md';
const MAX_TODO_ITEMS = 50;
const MAX_TODO_CHARS = 500;
const TODO_ITEM = /^\s*[-*]\s+\[([ xX])\]\s+(.+?)\s*$/;
const TODO_PROMPT =
  'Split the request below into a todo list that a smaller model can finish one item at a time. Look at the code first. Make each item one small change that can be checked: name the files to change and what to change in them. Put the items in order, so each item builds on the items before it. Make the last item run the build and tests. Do not make the changes yourself. Reply only with a Markdown checklist, one item per line, like "- [ ] item".';
const todoPrompt = (request: string, item: string, n: number, total: number) =>
  `Loop Mode works through ${TODO_FILE}, one item at a time. The original request was:\n\n${request}\n\nWork only on item ${n} of ${total}:\n\n${item}\n\nFirst check what is already done. Finish this item, then run the checks it needs. Do not start other items and do not edit ${TODO_FILE}; task-x checks the item off when you finish.`;

/** The items of a checklist from the Think model. Plain bullets or a numbered list also count. */
function parseTodo(text: string): string[] {
  const lines = text.split('\n');
  const checked = lines.flatMap((l) => TODO_ITEM.exec(l)?.[2] ?? []);
  const items = checked.length ? checked : lines.flatMap((l) => /^\s*(?:[-*]|\d+[.)])\s+(.+?)\s*$/.exec(l)?.[1] ?? []);
  return items.map((i) => i.replace(/\s+/g, ' ').slice(0, MAX_TODO_CHARS)).filter(Boolean).slice(0, MAX_TODO_ITEMS);
}
const nightPrompt = (request: string) =>
  `Continue the task from where you stopped. The original request was:\n\n${request}\n\nCheck what is already done, finish the remaining work, then run the build and tests.`;
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

/** Longest typed answer kept per question. */
const MAX_OTHER_CHARS = 1000;

/** Keeps only offered options, one choice for radio questions, and trimmed typed text. `answers` is null when they are not an object. */
function sanitizeAnswers(questions: Question[], raw: unknown, rawOther?: unknown): AskUserResult {
  if (!raw || typeof raw !== 'object') return { answers: null };
  const src = raw as Record<string, unknown>;
  const typed = rawOther && typeof rawOther === 'object' ? (rawOther as Record<string, unknown>) : {};
  const answers: Record<string, string[]> = {};
  const other: Record<string, string> = {};
  for (const q of questions) {
    const list = Array.isArray(src[q.id]) ? (src[q.id] as unknown[]) : [];
    const picked = [...new Set(list.filter((x): x is string => typeof x === 'string' && q.options.includes(x)))];
    answers[q.id] = q.kind === 'radio' ? picked.slice(0, 1) : picked;
    const t = typeof typed[q.id] === 'string' ? (typed[q.id] as string).trim().slice(0, MAX_OTHER_CHARS) : '';
    if (t) other[q.id] = t;
  }
  return { answers, other };
}

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
  | { kind: 'user'; text: string; mode: ChatMode; images?: number }
  | { kind: 'assistant'; text: string }
  | { kind: 'tool'; id: string; tool: string; detail: string; ok?: boolean; output?: string; outputCut?: boolean; stop?: { taskId: string; callId: string } }
  | { kind: 'file'; path: string; operation: number | null; diffKey: string; added: number; removed: number; diff: string; diffCut: boolean; undone?: boolean; kept?: boolean }
  | { kind: 'context'; text: string }
  | { kind: 'steer'; text: string }
  | { kind: 'compressed'; before: number; after: number; window: number; pruned: number; folded: number; dropped: boolean }
  | { kind: 'taskInfo'; taskKind: string; label: string; files: string[]; instructions: { label: string; path: string }[]; skills: string[] }
  | { kind: 'summary'; text: string; ok: boolean; canContinue?: boolean; title?: string; stats?: string[]; error?: string; signIn?: boolean; raiseLimit?: boolean }
  | { kind: 'error'; text: string; retryText?: string }
  | { kind: 'questions'; questions: Question[]; answers: Record<string, string[]> | null; other?: Record<string, string> }
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
  | { type: 'send'; text: string; queue?: boolean; images?: unknown }
  | { type: 'cancel' }
  | { type: 'clear' }
  | { type: 'newTab' }
  | { type: 'selectTab'; id: string }
  | { type: 'closeTab'; id: string }
  | { type: 'selectModel'; kind: string; model: string }
  | { type: 'loadModels'; force?: boolean }
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
  | { type: 'selectNight'; enabled: boolean }
  | { type: 'selectNightModel'; role: string; kind: string; model: string }
  | { type: 'selectAutoCommit'; enabled: boolean }
  | { type: 'selectEditMode'; mode: string }
  | { type: 'openSettings'; query: string }
  | { type: 'suggest'; text: string; mode: string }
  | { type: 'copyOutput'; id: string }
  | { type: 'openOutput'; id: string }
  | { type: 'stopLane'; taskId: string }
  | { type: 'stopTool'; id: string }
  | { type: 'refine'; text: string }
  | { type: 'applyWorktree'; name: string }
  | { type: 'discardWorktree'; name: string }
  | { type: 'openWorktree'; name: string }
  | { type: 'showProblems' }
  | { type: 'listFiles'; query: string }
  | { type: 'toggleEditor'; include: boolean }
  | { type: 'showGraph' }
  | { type: 'listTasks' }
  | { type: 'openTask'; id: string }
  | { type: 'deleteTask'; id: string }
  | { type: 'clearHistory' }
  | { type: 'answerQuestions'; callId: string; answers: unknown; other?: unknown };

/** One saved task in the History tab. */
interface TaskRow {
  id: string;
  title: string;
  status: string;
  chatMode: ChatMode;
  updatedMs: number;
  changedFiles: string[];
}

/** One row of the History tab. */
interface HistoryRow {
  id: string;
  title: string;
  status: string;
  mode: ChatMode;
  updatedMs: number;
  files: number;
}

/** A chat transcript saved in workspace state. */
interface SavedChat extends HistoryRow {
  items: Item[];
}

interface PendingFile {
  path: string;
  original: string | null;
  current: string | null;
  added: number;
  removed: number;
}

type ChangeStatus = 'added' | 'deleted' | 'modified';
/** One row of the changed-files list; `seq` orders edits that arrive while the list loads. */
interface ChangeRow {
  path: string;
  added: number;
  removed: number;
  status: ChangeStatus;
  seq: number;
}

/** One tab of the chat view: a chat with its own task, or one subagent's live activity. */
interface ChatTab {
  id: string;
  kind: 'chat' | 'subagent';
  items: Item[];
  taskId?: string;
  /** Subagent tabs: the name, role, owning chat tab, runtime task, and worktree folder. */
  title?: string;
  role?: string;
  parent?: string;
  laneTaskId?: string;
  dir?: string | null;
  status?: 'running' | 'completed' | 'failed' | 'cancelled';
  usage?: { used: number; window: number };
  assistant?: Extract<Item, { kind: 'assistant' }>;
}

const blankTab = (items: Item[] = [], taskId?: string): ChatTab => ({ id: `t${Date.now().toString(36)}${randomBytes(3).toString('hex')}`, kind: 'chat', items, taskId });

function tabTitle(t: ChatTab): string {
  if (t.kind === 'subagent') return t.title || t.role || 'Subagent';
  const first = t.items.find((i): i is Extract<Item, { kind: 'user' }> => i.kind === 'user');
  const text = first?.text.trim().split('\n')[0] ?? '';
  return !text ? 'New chat' : text.length > 40 ? `${text.slice(0, 39)}…` : text;
}

/** The status of a file after one more edit (`diff`), given its status before. Null when the file is gone again. */
function nextStatus(diff: string, prev?: ChangeStatus): ChangeStatus | null {
  if (/^\+\+\+ \/dev\/null$/m.test(diff)) return prev === 'added' ? null : 'deleted';
  if (/^--- \/dev\/null$/m.test(diff)) return prev === 'deleted' ? 'modified' : (prev ?? 'added');
  return prev ?? 'modified';
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


function isBackgroundStart(output: string): boolean {
  try {
    return (JSON.parse(output) as { background?: unknown })?.background === true;
  } catch {
    return false;
  }
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
    budgetExceeded: 'Stopped: reached the token budget',
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
  private tabs: ChatTab[];
  /** The tab on screen. */
  private activeTab: string;
  /** The chat tab the user works in: the last chat tab on screen. */
  private chatTabId: string;
  /** The chat tab bound to the agent: the running task's tab. It stays bound after the run until the user acts again. */
  private runTab: string | undefined;
  /** Night mode runs again in the tab of its last run. */
  private nightTab: string | undefined;
  /** Approval and question cards of the running task, shown again when its tab comes back on screen. */
  private readonly cards = new Map<string, object>();
  /** The changed-files list: the edit log, plus edits that arrived after it was loaded. */
  private changes = new Map<string, ChangeRow>();
  private changeSeq = 0;
  private changesGen = 0;
  /** Models listed per provider: when, what, and why a listing failed or came from somewhere else. */
  private readonly listed = new Map<string, Listed & { at: number }>();
  /** Providers whose models are being listed now. */
  private readonly listing = new Set<string>();
  // Context window per `kind:model`; 0 means unknown.
  private readonly windows = new Map<string, number>();
  /** Messages sent while a request was running; each one runs after the one before it. */
  private readonly queue: { text: string; mode?: ChatMode; images?: ImageInput[]; tab?: string }[] = [];
  private readonly approvals = new Map<string, (d: ApprovalDecision | 'always') => void>();
  private readonly questions = new Map<string, { questions: Question[]; done: (res: AskUserResult) => void }>();
  private night: boolean;
  /** The last request the user sent; Night mode runs it again until a run completes. */
  private nightRequest: string | undefined;
  /** True when the night request is split into todo.md; `nightItem` is the item the current run works on. */
  private nightTodo: boolean;
  private nightItem: string | undefined;
  private nightFailures = 0;
  private nightTimer: NodeJS.Timeout | undefined;
  private nightWatch: NodeJS.Timeout | undefined;
  /** True when Night mode stopped a stalled run, so the stop is not taken as the user's. */
  private stalled = false;
  private lastActivity = 0;
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
    const saved = ctx.workspaceState.get<{ tabs?: ChatTab[]; active?: string }>(TABS_KEY);
    const tabs = Array.isArray(saved?.tabs) ? saved.tabs.filter((t) => t?.kind === 'chat' && typeof t.id === 'string' && Array.isArray(t.items)) : [];
    this.tabs = tabs.length ? tabs : [blankTab(ctx.workspaceState.get<Item[]>(TRANSCRIPT_KEY, []), ctx.workspaceState.get<string>(TASK_KEY))];
    this.activeTab = this.chatTabId = this.tabs.find((t) => t.id === saved?.active)?.id ?? this.tabs[0].id;
    this.includeEditor = ctx.workspaceState.get<boolean>(INCLUDE_EDITOR_KEY, true);
    this.autoCommit = ctx.workspaceState.get<boolean>(AUTO_COMMIT_KEY, false);
    this.night = ctx.workspaceState.get<boolean>(NIGHT_KEY, false);
    this.nightRequest = ctx.workspaceState.get<string>(NIGHT_REQUEST_KEY);
    this.nightTodo = ctx.workspaceState.get<boolean>(NIGHT_TODO_KEY, false);
    this.nightItem = ctx.workspaceState.get<string>(NIGHT_ITEM_KEY);
    this.watchNight();
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

  /** The chat that requests and agent events go to: the bound run tab, else the user's chat tab. */
  private get chat(): ChatTab {
    return this.tabs.find((t) => t.id === (this.runTab ?? this.chatTabId)) ?? this.tabs.find((t) => t.kind === 'chat') ?? this.addTab(blankTab());
  }
  private get items(): Item[] {
    return this.chat.items;
  }
  private set items(v: Item[]) {
    this.chat.items = v;
  }
  private get taskId(): string | undefined {
    return this.chat.taskId;
  }
  private set taskId(v: string | undefined) {
    this.chat.taskId = v;
  }

  cancel(): void {
    this.abort?.abort();
    for (const resolve of this.approvals.values()) resolve('deny');
    this.approvals.clear();
    for (const p of this.questions.values()) p.done({ answers: null });
    this.questions.clear();
    clearTimeout(this.nightTimer);
    this.nightTimer = undefined;
    this.queue.length = 0;
    this.postQueue();
  }

  /** Empties the current chat tab: a new task, no transcript, and the context meter reset. */
  async clear(): Promise<void> {
    this.cancel();
    await this.archive();
    this.items = [];
    this.taskId = undefined;
    this.chat.usage = undefined;
    await this.setNightRequest(undefined);
    await this.persist();
    this.renderChat();
    this.postTabs();
  }

  /** Opens an empty chat in a new tab, after the open tabs and before History. */
  async newChat(): Promise<void> {
    if (this.tabs.filter((t) => t.kind === 'chat').length >= MAX_CHAT_TABS) return void vscode.window.showWarningMessage(`task-x: close a chat tab first. Up to ${MAX_CHAT_TABS} chats can be open.`);
    this.showTab(this.addTab(blankTab()).id);
  }

  private addTab(tab: ChatTab): ChatTab {
    this.tabs.push(tab);
    return tab;
  }

  /** Puts a tab on screen. A chat tab also becomes the chat for the next request. */
  private showTab(id: string): void {
    const tab = this.tabs.find((t) => t.id === id);
    if (!tab) return;
    this.activeTab = id;
    if (tab.kind === 'chat') {
      this.chatTabId = id;
      if (!this.busy) this.runTab = undefined;
    }
    this.postTabs();
    this.renderTab();
    this.post({ type: 'view', view: 'chat' });
    this.refreshStatus();
    void this.saveTabs();
  }

  /** Closes a tab. The running task's tab stays open; closing the last chat opens an empty one. */
  private async closeTab(id: string): Promise<void> {
    const i = this.tabs.findIndex((t) => t.id === id);
    const tab = this.tabs[i];
    if (!tab) return;
    if (this.busy && tab.id === this.runTab) return void vscode.window.showWarningMessage('task-x: stop the running task before you close its tab.');
    if (tab.kind === 'chat') await this.archive(tab);
    this.tabs.splice(i, 1);
    if (this.runTab === id) this.runTab = undefined;
    if (!this.tabs.some((t) => t.kind === 'chat')) this.addTab(blankTab());
    if (this.chatTabId === id) this.chatTabId = (this.tabs.slice(0, i).findLast((t) => t.kind === 'chat') ?? this.tabs.find((t) => t.kind === 'chat'))!.id;
    if (this.activeTab === id) return this.showTab(this.tabs[Math.min(i, this.tabs.length - 1)].id);
    this.postTabs();
    await this.saveTabs();
  }

  /** Empties a chat tab after its task was deleted. */
  private async resetTab(tab: ChatTab): Promise<void> {
    tab.items = [];
    tab.taskId = undefined;
    tab.usage = undefined;
    if (tab.id === this.chat.id) await this.setNightRequest(undefined);
    if (tab.id === this.activeTab) this.renderTab();
  }

  private postTabs(): void {
    const tabs = this.tabs.map((t) => ({
      id: t.id,
      kind: t.kind,
      title: tabTitle(t),
      role: t.role ?? null,
      status: t.kind === 'subagent' ? t.status : this.busy && t.id === this.runTab ? 'running' : null,
    }));
    this.post({ type: 'tabs', tabs, active: this.activeTab });
  }

  /** Draws the tab on screen: its transcript, context meter, waiting messages, and open cards. */
  private renderTab(): void {
    const tab = this.tabs.find((t) => t.id === this.activeTab) ?? this.chat;
    const live = tab.kind === 'subagent' ? tab.status === 'running' && !!tab.assistant : this.busy && tab.id === this.runTab;
    this.post({ type: 'history', items: tab.items, streaming: live && tab.items.at(-1)?.kind === 'assistant' });
    this.post({ type: 'contextUsage', used: tab.usage?.used ?? 0, window: tab.usage?.window ?? 0 });
    if (this.busy && tab.id === this.runTab) for (const c of this.cards.values()) this.post(c);
    this.postQueue();
  }

  /** Draws the chat again when it is the tab on screen. */
  private renderChat(): void {
    if (this.activeTab === this.chat.id) this.renderTab();
  }

  /** Posts a message about the chat only while its tab is on screen; the tab's items draw it later. */
  private postChat(msg: object): void {
    if (this.activeTab === this.chat.id) this.post(msg);
  }

  invalidateModels(): void {
    this.listed.clear();
  }

  copySelection(): void {
    this.post({ type: 'copySelection' });
  }

  refreshStatus(): void {
    const ref = activeRef();
    const claude = engine() === 'claudeCode';
    this.post({ type: 'status', provider: claude ? CLAUDE_LABEL : refLabel(ref), model: claude ? claudeModel() : modelFor(ref) || '(no model)', busy: this.busy && this.activeTab === this.runTab, mode: 'agent', editMode: editMode(), autoCommit: this.autoCommit, night: this.night, nightPlanner: nightModels().planner ?? null, nightWorker: nightModels().worker ?? null });
    void this.postModels();
  }

  dispose(): void {
    clearInterval(this.nightWatch);
    this.cancel();
  }

  private async setNightRequest(text: string | undefined): Promise<void> {
    this.nightRequest = text;
    await this.ctx.workspaceState.update(NIGHT_REQUEST_KEY, text);
    if (!text) await this.setNightTodo(false);
  }

  private async setNightTodo(on: boolean, item?: string): Promise<void> {
    this.nightTodo = on;
    this.nightItem = on ? item : undefined;
    await this.ctx.workspaceState.update(NIGHT_TODO_KEY, on);
    await this.ctx.workspaceState.update(NIGHT_ITEM_KEY, this.nightItem);
  }

  private todoUri(): vscode.Uri | undefined {
    const root = WorkspaceSync.root();
    return root ? vscode.Uri.file(path.join(root, TODO_FILE)) : undefined;
  }

  private async readTodoLines(): Promise<string[]> {
    const uri = this.todoUri();
    if (!uri) return [];
    try {
      return new TextDecoder().decode(await vscode.workspace.fs.readFile(uri)).split('\n');
    } catch {
      return [];
    }
  }

  private async readTodo(): Promise<{ text: string; done: boolean }[]> {
    return (await this.readTodoLines()).flatMap((l) => {
      const m = TODO_ITEM.exec(l);
      return m ? [{ text: m[2], done: m[1] !== ' ' }] : [];
    });
  }

  private async writeTodo(request: string, items: string[]): Promise<void> {
    const uri = this.todoUri();
    if (!uri) throw new Error('open a workspace folder first');
    const title = request.trim().split('\n')[0].slice(0, 200);
    const body = `# Todo\n\nLoop Mode works through this list one item at a time and checks each item off when it is done.\n\nRequest: ${title}\n\n${items.map((i) => `- [ ] ${i}`).join('\n')}\n`;
    await vscode.workspace.fs.writeFile(uri, new TextEncoder().encode(body));
  }

  /** Checks off the first open item with this text. The file is read again, so edits made during the run are kept. */
  private async checkTodo(item: string): Promise<void> {
    const uri = this.todoUri();
    const lines = await this.readTodoLines();
    const i = lines.findIndex((l) => {
      const m = TODO_ITEM.exec(l);
      return m?.[1] === ' ' && m[2] === item;
    });
    if (!uri || i < 0) return;
    lines[i] = lines[i].replace('[ ]', '[x]');
    await vscode.workspace.fs.writeFile(uri, new TextEncoder().encode(lines.join('\n')));
  }

  /** Night mode: the prompt for this run. With a todo list, the Loop model gets the first open item only. */
  private async nightTask(text: string, retry: boolean, signal: AbortSignal): Promise<string> {
    this.taskId ??= `c${Date.now()}${randomBytes(3).toString('hex')}`;
    if (!retry) {
      const items = await this.planTodo(text, this.taskId, signal);
      await this.setNightTodo(!!items?.length);
    }
    if (!this.nightTodo) return text;
    const items = await this.readTodo();
    const n = items.findIndex((i) => !i.done);
    if (n < 0) return text;
    await this.setNightTodo(true, items[n].text);
    return todoPrompt(this.nightRequest ?? text, items[n].text, n + 1, items.length);
  }

  /** Night mode: after a completed run, checks its item off and starts the next one, until the list is done. */
  private async nextTodo(): Promise<void> {
    if (!this.nightTodo || !this.nightRequest) return;
    if (this.nightItem) await this.checkTodo(this.nightItem);
    const items = await this.readTodo();
    const left = items.filter((i) => !i.done);
    if (!left.length) {
      await this.setNightTodo(false);
      this.add({ kind: 'context', text: `Loop Mode: all ${items.length} items in ${TODO_FILE} are done.` });
      return void this.persist();
    }
    await this.setNightTodo(true);
    this.add({ kind: 'context', text: `Loop Mode: ${items.length - left.length} of ${items.length} items in ${TODO_FILE} done.` });
    void this.persist();
    this.scheduleNight(0);
  }

  /** Night mode: the Think model splits the request into todo.md, once per request. Undefined without a Think model or when it fails. */
  private async planTodo(request: string, taskId: string, signal: AbortSignal): Promise<string[] | undefined> {
    const planner = nightModels().planner;
    if (!planner) return undefined;
    this.add({ kind: 'context', text: `Loop Mode: ${planner.model} is writing the todo list.` });
    const client = await this.getClient();
    this.wire(client);
    const planId = `${taskId}-plan${Date.now()}`;
    const state: { assistant?: Extract<Item, { kind: 'assistant' }> } = {};
    const off = client.onNotification('agent/event', (p) => {
      const { taskId: id, event } = p as AgentEventParams;
      if (id === planId) this.onEvent(event, state);
    });
    try {
      const prompt = `${TODO_PROMPT}\n\n${request}`;
      let summary: AgentSummary;
      if (planner.kind === CLAUDE_KIND) {
        const cliPath = findClaude();
        if (!cliPath) throw new Error('Claude Code CLI not found');
        const model = planner.model === 'default' ? undefined : planner.model;
        summary = await client.request<AgentSummary>('claude/run', { prompt, taskId: planId, cliPath, chatMode: 'plan', model, ...claudePermissions('plan') }, { signal });
      } else {
        if (!isProviderRef(planner.kind)) throw new Error(`unknown provider ${planner.kind}`);
        const provider = await providerConfigWith(this.ctx.secrets, planner.kind, planner.model);
        const params = await runParams(this.ctx.secrets, { prompt, mode: 'plan', taskId: planId, editor: this.editorContext(request), provider });
        summary = await client.request<AgentSummary>('agent/run', params, { signal });
      }
      if (summary.status !== 'completed') throw new Error(summary.error || `the Think model stopped (${summary.status})`);
      const items = parseTodo(summary.text);
      if (!items.length) throw new Error('the reply has no todo items');
      await this.writeTodo(request, items);
      this.add({ kind: 'context', text: `Loop Mode: wrote ${items.length} items to ${TODO_FILE}. The Loop model works on them one at a time.` });
      return items;
    } catch (e) {
      if (signal.aborted) throw e;
      this.add({ kind: 'context', text: `Loop Mode: the todo list failed (${describeError(e)}). The Loop model works on the whole request.` });
      return undefined;
    } finally {
      off();
    }
  }

  private lastSummary(): Extract<Item, { kind: 'summary' }> | undefined {
    return this.items.findLast((i): i is Extract<Item, { kind: 'summary' }> => i.kind === 'summary');
  }

  /** Night mode: stops a run with no progress for NIGHT_STALL_MS so it starts again. Waiting for the user is not a stall. */
  private watchNight(): void {
    clearInterval(this.nightWatch);
    this.nightWatch = undefined;
    if (!this.night) return;
    this.nightWatch = setInterval(() => {
      if (!this.busy) return;
      if (this.approvals.size || this.questions.size) return void (this.lastActivity = Date.now());
      if (Date.now() - this.lastActivity < NIGHT_STALL_MS) return;
      this.stalled = true;
      this.lastActivity = Date.now();
      this.add({ kind: 'context', text: `Loop Mode: no progress for ${NIGHT_STALL_MS / 60_000} minutes. Restarting the request.` });
      this.abort?.abort();
    }, NIGHT_CHECK_MS);
  }

  /** Night mode: after a run that did not complete, runs the last request again with a growing delay. A user Stop ends it. */
  private afterRun(before: Item | undefined, signal: AbortSignal): void {
    if (!this.night || !this.nightRequest || this.busy) return;
    const last = this.lastSummary();
    if (last && last !== before && last.ok) {
      this.nightFailures = 0;
      return void this.nextTodo();
    }
    if (signal.aborted && !this.stalled) return;
    this.nightFailures++;
    const wait = Math.min(NIGHT_RETRY_MAX_MS, NIGHT_RETRY_BASE_MS * 2 ** Math.min(this.nightFailures - 1, 10));
    this.add({ kind: 'context', text: `Loop Mode: the request did not finish. Retry ${this.nightFailures} in ${Math.round(wait / 1000)} s.` });
    void this.persist();
    this.scheduleNight(wait);
  }

  private scheduleNight(wait: number): void {
    clearTimeout(this.nightTimer);
    this.nightTimer = setTimeout(async () => {
      this.nightTimer = undefined;
      if (!this.night || this.busy || !this.nightRequest) return;
      // With a todo list, the transcript and the commit name the item.
      const items = this.nightTodo ? await this.readTodo() : [];
      const n = items.findIndex((i) => !i.done);
      void this.send(n >= 0 ? `Todo ${n + 1}/${items.length}: ${items[n].text}` : nightPrompt(this.nightRequest), undefined, true, [], this.nightTab);
    }, wait);
  }

  /** Night mode: starts the last request again when this chat's last run did not complete. */
  private resumeNight(): void {
    if (!this.night || this.busy || this.nightTimer || !this.nightRequest) return;
    const lastUser = this.items.findLastIndex((i) => i.kind === 'user');
    const lastDone = this.items.findLastIndex((i) => i.kind === 'summary' && i.ok);
    if (lastUser < 0) return;
    if (lastDone > lastUser) return void this.nextTodo();
    this.add({ kind: 'context', text: 'Loop Mode: the last request did not finish. Starting it again.' });
    this.scheduleNight(0);
  }

  private post(msg: object): void {
    void this.view?.webview.postMessage(msg);
  }

  private async persist(archive = true): Promise<void> {
    this.items = this.items.slice(-MAX_ITEMS);
    await this.saveTabs();
    if (archive) await this.archive();
  }

  /** Saves the open chat tabs. Subagent tabs show live activity only and are not saved. */
  private async saveTabs(): Promise<void> {
    const tabs = this.tabs.filter((t) => t.kind === 'chat').map(({ id, kind, items, taskId }) => ({ id, kind, items: items.slice(-MAX_ITEMS), taskId }));
    const active = this.tabs.find((t) => t.id === this.activeTab)?.kind === 'chat' ? this.activeTab : this.chatTabId;
    await this.ctx.workspaceState.update(TABS_KEY, { tabs, active });
  }

  private chats(): SavedChat[] {
    const list = this.ctx.workspaceState.get<SavedChat[]>(CHATS_KEY, []);
    return Array.isArray(list) ? list.filter((c) => typeof c?.id === 'string' && Array.isArray(c.items)) : [];
  }

  /** Saves a chat tab to the History list. The time changes only when the transcript grows. */
  private async archive(tab = this.chat): Promise<void> {
    const { taskId: id, items } = tab;
    const first = items.find((i): i is Extract<Item, { kind: 'user' }> => i.kind === 'user');
    if (!id || !first) return;
    const all = this.chats();
    const prev = all.find((c) => c.id === id);
    const last = items.findLast((i): i is Extract<Item, { kind: 'summary' }> => i.kind === 'summary');
    const chat: SavedChat = {
      id,
      title: first.text.trim().split('\n')[0].slice(0, MAX_TITLE) || '(untitled)',
      status: last ? (last.ok ? 'completed' : 'failed') : 'cancelled',
      mode: first.mode,
      updatedMs: prev && prev.items.length === items.length ? prev.updatedMs : Date.now(),
      files: new Set(items.flatMap((i) => (i.kind === 'file' ? [i.path] : []))).size,
      items,
    };
    const rest = all.filter((c) => c.id !== id);
    await this.ctx.workspaceState.update(CHATS_KEY, [chat, ...rest].sort((a, b) => b.updatedMs - a.updatedMs).slice(0, MAX_CHATS));
  }

  /** Deletes one saved chat here and in the runtime; the open chat starts fresh when it is the one deleted. */
  async deleteTask(id: string): Promise<void> {
    await this.ctx.workspaceState.update(CHATS_KEY, this.chats().filter((c) => c.id !== id));
    await this.getClient()
      .then((c) => c.request('agent/deleteTask', { taskId: id }))
      .catch((e) => this.log('warn', `deleteTask: ${describeError(e)}`));
    for (const t of this.tabs) if (t.kind === 'chat' && t.taskId === id && !(this.busy && t.id === this.runTab)) await this.resetTab(t);
    this.postTabs();
    await this.saveTabs();
    await this.postTasks();
  }

  /** Deletes all saved chats after the user confirms. */
  async clearHistory(): Promise<void> {
    const ok = 'Delete All';
    if ((await vscode.window.showWarningMessage('Delete all saved chats? You cannot undo this.', { modal: true }, ok)) !== ok) return;
    const ids = new Set(this.chats().map((c) => c.id));
    try {
      const client = await this.getClient();
      for (const t of await client.request<TaskRow[]>('agent/tasks')) if (typeof t?.id === 'string') ids.add(t.id);
      for (const id of ids) await client.request('agent/deleteTask', { taskId: id });
    } catch (e) {
      this.log('warn', `clearHistory: ${describeError(e)}`);
    }
    await this.ctx.workspaceState.update(CHATS_KEY, []);
    for (const t of this.tabs) if (t.kind === 'chat' && t.taskId && ids.has(t.taskId) && !(this.busy && t.id === this.runTab)) await this.resetTab(t);
    this.postTabs();
    await this.saveTabs();
    await this.postTasks();
  }

  private add(item: Item): void {
    this.items.push(item);
    this.postChat({ type: 'item', item });
    if (item.kind === 'user') this.postTabs();
  }

  private setBusy(abort: AbortController | undefined): void {
    this.abort = abort;
    void vscode.commands.executeCommand('setContext', 'task-x.busy', this.busy);
    this.onBusyChange?.(this.busy);
    this.refreshStatus();
    this.postTabs();
  }

  /**
   * Providers the picker shows: the active one, every named endpoint, providers that need no key
   * (Ollama), and providers with a saved key. The Anthropic API shows only when it has a key:
   * Claude runs through the Claude Code CLI.
   */
  private async pickerRefs(): Promise<string[]> {
    const active = activeRef();
    const out: string[] = [];
    for (const ref of providerRefs()) {
      const keyed = await hasApiKey(this.ctx.secrets, ref);
      if (ref === 'anthropic' ? keyed : ref === active || !isProviderKind(ref) || keyed || !refNeedsKey(ref)) out.push(ref);
    }
    return out;
  }

  /** Every provider's models with their context size and capabilities, plus Claude Code when its CLI is installed. */
  private async postModels(): Promise<void> {
    const active = activeRef();
    const row = (m: ModelInfo) => ({ id: m.id, ctx: formatTokens(m.contextWindow), caps: m.capabilities, details: m.details ?? '' });
    const groups = [];
    for (const ref of await this.pickerRefs()) {
      const l = this.listed.get(ref);
      const local = refKind(ref) === 'ollama';
      // A list the endpoint answered is complete: the saved model shows only if it is really there.
      const complete = !!l && !l.error;
      const models = withSaved(l?.models ?? [], modelFor(ref), complete).slice(0, MAX_PICKER_MODELS).map(row);
      const note = [l?.note, complete ? missingNote(l.models, modelFor(ref), local) : undefined].filter(Boolean).join(' ') || undefined;
      groups.push({ kind: ref, label: refLabel(ref), local, models, loading: this.listing.has(ref), note, error: l?.models.length ? undefined : l?.error });
    }
    if (findClaude()) groups.push({ kind: CLAUDE_KIND, label: CLAUDE_LABEL, local: false, models: withSaved(CLAUDE_MODEL_INFO, claudeModel()).map(row), loading: false });
    const current = engine() === 'claudeCode' ? { kind: CLAUDE_KIND, model: claudeModel() } : { kind: active, model: modelFor(active), window: this.windowFor(active, modelFor(active)) };
    this.post({ type: 'models', active: current, groups });
  }

  /** The window for the chat model: the user setting, a cached answer, or undefined while the runtime is asked. */
  private windowFor(kind: string, model: string | undefined): number | undefined {
    const set = limits().contextWindowTokens;
    if (set || !model || !isProviderRef(kind)) return set;
    const listed = this.listed.get(kind)?.models.find((m) => m.id === model)?.contextWindow;
    if (listed) return listed;
    const key = `${kind}:${model}`;
    if (!this.windows.has(key)) void this.loadWindow(kind, key);
    return this.windows.get(key) || undefined;
  }

  private async loadWindow(kind: Parameters<typeof providerConfig>[1], key: string): Promise<void> {
    this.windows.set(key, 0);
    try {
      const provider = await providerConfig(this.ctx.secrets, kind);
      const client = await this.getClient();
      const { tokens } = await client.request<ContextWindowResult>('provider/contextWindow', { provider }, { timeoutMs: settings().timeoutMs });
      if (!Number.isFinite(tokens) || tokens <= 0) return;
      this.windows.set(key, tokens);
    } catch {
      // The badge is best effort; the picker works without it.
      return;
    }
    await this.postModels();
  }

  /**
   * Lists the models of every provider in the picker, at the same time. A list is reused for
   * `MODEL_LIST_TTL_MS` unless `force` (the refresh button). A provider with no model set yet is
   * listed too; a failure is shown in the picker instead of hiding the provider's models.
   */
  private async loadModels(force = false): Promise<void> {
    const stale = (ref: string) => force || Date.now() - (this.listed.get(ref)?.at ?? 0) > MODEL_LIST_TTL_MS;
    const refs = (await this.pickerRefs()).filter((ref) => !this.listing.has(ref) && stale(ref));
    if (!refs.length) return;
    for (const ref of refs) this.listing.add(ref);
    await this.postModels();
    const timeoutMs = settings().timeoutMs;
    await Promise.all(
      refs.map(async (ref) => {
        try {
          const client = await this.getClient();
          const provider = await providerConfigWith(this.ctx.secrets, ref, modelFor(ref) || 'probe');
          const r = await listModels(async (p) => (await client.request<{ models: ModelInfo[] }>('provider/models', { provider: p }, { timeoutMs })).models, provider);
          this.listed.set(ref, { ...r, at: Date.now() });
        } catch (e) {
          this.listed.set(ref, { models: [], error: describeError(e), at: Date.now() });
        } finally {
          this.listing.delete(ref);
        }
        await this.postModels();
      }),
    );
  }

  private async onMessage(m: Inbound): Promise<void> {
    // Between runs, what the user does goes to the tab they work in.
    if (!this.busy) this.runTab = undefined;
    switch (m?.type) {
      case 'ready':
        this.postTabs();
        this.renderTab();
        this.refreshStatus();
        this.postEditor();
        this.post({ type: 'index', index: this.indexState });
        void this.refreshChanges();
        this.resumeNight();
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
      case 'selectTab':
        if (typeof m.id === 'string') this.showTab(m.id);
        break;
      case 'closeTab':
        if (typeof m.id === 'string') await this.closeTab(m.id);
        break;
      case 'openTask':
        if (typeof m.id === 'string' && m.id.length <= 100) {
          await this.openTask(m.id).catch((e) => vscode.window.showErrorMessage(`task-x: ${describeError(e)}`));
        }
        break;
      case 'deleteTask':
        if (typeof m.id === 'string' && m.id.length <= 100) await this.deleteTask(m.id);
        break;
      case 'clearHistory':
        await this.clearHistory();
        break;
      case 'answerQuestions': {
        const pending = typeof m.callId === 'string' ? this.questions.get(m.callId) : undefined;
        pending?.done(sanitizeAnswers(pending.questions, m.answers, m.other));
        break;
      }
      case 'keep':
      case 'revert':
        await this.settle(m.type, Array.isArray(m.paths) ? m.paths.filter((p) => typeof p === 'string') : undefined);
        break;
      case 'diffFile':
        if (typeof m.path === 'string') await this.diffFile(m.path);
        break;
      case 'send': {
        if (typeof m.text !== 'string') break;
        // Steering is text only, so a message with images waits for the running task instead.
        // A message from another tab while a task runs waits for it, then runs in its own tab.
        const images = imagesFrom(m.images);
        const here = this.activeTab === this.runTab;
        await (images.length || !here ? this.send(m.text, undefined, false, images, this.chatTabId) : this.busy && m.queue !== true ? this.steer(m.text) : this.send(m.text));
        break;
      }
      case 'cancel':
        this.cancel();
        await this.setNightRequest(undefined);
        break;
      case 'clear':
      case 'newTab':
        await this.newChat();
        break;
      case 'loadModels':
        await this.loadModels(m.force === true);
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
        else if (m.kind === CLAUDE_KIND) {
          await useClaude(m.model.trim());
          this.refreshStatus();
        } else if (isProviderRef(m.kind)) {
          await setWindowChoice({ engine: 'task-x' });
          await setProvider(m.kind, m.model.trim());
          this.refreshStatus();
        } else this.refreshStatus();
        break;
      case 'approve': {
        const d = (['allow', 'allowSession', 'always', 'deny'] as const).find((x) => x === m.decision) ?? 'deny';
        this.approvals.get(m.callId)?.(d);
        this.approvals.delete(m.callId);
        break;
      }
      case 'selectNight':
        this.night = m.enabled === true;
        await this.ctx.workspaceState.update(NIGHT_KEY, this.night);
        this.watchNight();
        if (this.night) this.resumeNight();
        else {
          clearTimeout(this.nightTimer);
          this.nightTimer = undefined;
        }
        this.refreshStatus();
        break;
      case 'selectNightModel': {
        const model = typeof m.model === 'string' ? m.model.trim() : '';
        const kindOk = isProviderRef(m.kind) || (m.role === 'planner' && m.kind === CLAUDE_KIND);
        if (model && model.length <= 256 && kindOk && (m.role === 'planner' || m.role === 'worker')) {
          await setWindowChoice(m.role === 'planner' ? { nightPlanner: { kind: m.kind, model } } : { nightWorker: { kind: m.kind, model } });
        }
        this.refreshStatus();
        break;
      }
      case 'selectAutoCommit':
        this.autoCommit = m.enabled === true;
        await this.ctx.workspaceState.update(AUTO_COMMIT_KEY, this.autoCommit);
        this.refreshStatus();
        break;
      case 'selectEditMode':
        // Applies from the next request; a running task keeps the mode it started with.
        if (EDIT_MODES.includes(m.mode as EditMode)) await setEditMode(m.mode as EditMode);
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
      case 'refine':
        if (typeof m.text === 'string') await this.refine(m.text);
        break;
      case 'stopTool': {
        const it = this.items.find((i): i is Extract<Item, { kind: 'tool' }> => i.kind === 'tool' && i.id === m.id);
        if (it?.stop && it.ok === undefined && this.busy) await (await this.getClient()).request('agent/cancelTool', it.stop).catch((e) => this.log('warn', `cancelTool: ${describeError(e)}`));
        break;
      }
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

  /** Saved chats for the History tab: runtime tasks merged with the chats kept here, newest first. */
  private async postTasks(): Promise<void> {
    const byId = new Map<string, HistoryRow>();
    let error: string | undefined;
    try {
      const rows = await (await this.getClient()).request<TaskRow[]>('agent/tasks');
      for (const t of rows) {
        if (typeof t?.id !== 'string') continue;
        byId.set(t.id, { id: t.id, title: t.title || '(untitled)', status: t.status, mode: t.chatMode, updatedMs: t.updatedMs ?? 0, files: t.changedFiles?.length ?? 0 });
      }
    } catch (e) {
      error = describeError(e);
    }
    for (const { items: _, ...c } of this.chats()) {
      const r = byId.get(c.id);
      byId.set(c.id, r ? { ...r, updatedMs: Math.max(r.updatedMs, c.updatedMs), files: Math.max(r.files, c.files) } : c);
    }
    const tasks = [...byId.values()].sort((a, b) => b.updatedMs - a.updatedMs);
    this.post({ type: 'tasks', tasks, current: this.taskId ?? null, error: tasks.length ? undefined : error });
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
    this.postChat({ type: 'laneUpdate', item: lane });
  }

  /** Opens a tab for a subagent next to its chat, without leaving the tab on screen. */
  private openSubagentTab(e: Extract<AgentEvent, { type: 'subagentStarted' }>): void {
    const parent = this.chat;
    const tab: ChatTab = { id: blankTab().id, kind: 'subagent', title: e.name, role: e.role, parent: parent.id, laneTaskId: e.taskId, dir: e.dir, status: 'running', items: [{ kind: 'user', text: e.objective, mode: 'agent' }] };
    let at = this.tabs.indexOf(parent);
    while (this.tabs[at + 1]?.parent === parent.id) at++;
    this.tabs.splice(at + 1, 0, tab);
    const done = this.tabs.filter((t) => t.kind === 'subagent' && t.status !== 'running' && t.id !== this.activeTab);
    const extra = this.tabs.filter((t) => t.kind === 'subagent').length - MAX_SUBAGENT_TABS;
    for (const old of done.slice(0, Math.max(0, extra))) this.tabs.splice(this.tabs.indexOf(old), 1);
    this.postTabs();
  }

  /** Adds one event of a subagent to its tab. Edits outside a worktree also go to the changed-files list. */
  private feedSubagent(tab: ChatTab, ev: AgentEvent): void {
    const show = (msg: object) => this.activeTab === tab.id && this.post(msg);
    const push = (item: Item) => {
      tab.items.push(item);
      if (tab.items.length > MAX_ITEMS) tab.items.splice(0, tab.items.length - MAX_ITEMS);
      show({ type: 'item', item });
    };
    switch (ev.type) {
      case 'textDelta':
        if (!tab.assistant) push((tab.assistant = { kind: 'assistant', text: '' }));
        tab.assistant.text += ev.text;
        show({ type: 'delta', text: ev.text });
        break;
      case 'toolStarted':
        tab.assistant = undefined;
        push({ kind: 'tool', id: `${tab.laneTaskId}:${ev.callId}`, tool: ev.tool, detail: toolDetail(ev.tool, ev.input) });
        break;
      case 'toolFinished': {
        tab.assistant = undefined;
        if (!ev.background && isBackgroundStart(ev.output)) break;
        const id = `${tab.laneTaskId}:${ev.callId}`;
        const it = tab.items.find((i): i is Extract<Item, { kind: 'tool' }> => i.kind === 'tool' && i.id === id);
        const out = this.captureOutput(id, ev.tool, ev.ok, ev.output);
        if (it) Object.assign(it, { ok: ev.ok }, out);
        else push({ kind: 'tool', id, tool: ev.tool, detail: 'not run', ok: false, ...out });
        if (it) show({ type: 'toolDone', id, ok: ev.ok, ...out });
        break;
      }
      case 'fileChanged': {
        tab.assistant = undefined;
        const diffKey = `${Date.now()}-${this.diffs.size}`;
        this.diffs.set(diffKey, ev.diff);
        const { added, removed } = diffStats(ev.diff);
        push({ kind: 'file', path: ev.path, operation: tab.dir ? null : ev.operation, diffKey, added, removed, diff: ev.diff.slice(0, MAX_CARD_DIFF), diffCut: ev.diff.length > MAX_CARD_DIFF });
        if (!tab.dir) this.liveChange(ev.path, ev.diff);
        break;
      }
      case 'error':
        push({ kind: 'error', text: `${ev.error.kind}: ${ev.error.message}` });
        break;
      case 'warning':
        push({ kind: 'context', text: ev.message });
        break;
      case 'context':
        tab.usage = { used: ev.usedTokens, window: ev.windowTokens };
        show({ type: 'contextUsage', used: ev.usedTokens, window: ev.windowTokens });
        break;
      case 'done':
        tab.assistant = undefined;
        push({ kind: 'summary', ...summaryText(ev.summary) });
        break;
    }
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

  /**
   * Loads the review list (net agent changes not yet kept or undone) into the panel above the input.
   * Edits that arrive while it loads stay in the list; only the newest load is shown.
   */
  async refreshChanges(): Promise<void> {
    clearTimeout(this.changesTimer);
    this.changesTimer = undefined;
    const gen = ++this.changesGen;
    const seq = this.changeSeq;
    let next = new Map<string, ChangeRow>();
    try {
      const client = await this.getClient();
      const files = await client.request<PendingFile[]>('edits/pending');
      if (gen !== this.changesGen) return;
      for (const f of files) this.originals.set(f.path, f.original);
      next = new Map(files.map((f) => [f.path, { path: f.path, added: f.added, removed: f.removed, status: f.original === null ? 'added' : f.current === null ? 'deleted' : 'modified', seq }]));
      for (const [p, row] of this.changes) if (row.seq > seq && !next.has(p)) next.set(p, row);
    } catch {
      // No runtime or workspace yet: nothing to review.
      if (gen !== this.changesGen) return;
    }
    this.changes = next;
    this.postChanges();
  }

  /** Reloads the list at most every CHANGES_REFRESH_MS while edits arrive, so a long run of edits does not hold it back. */
  private queueChanges(): void {
    if (this.changesTimer) return;
    this.changesTimer = setTimeout(() => void this.refreshChanges(), CHANGES_REFRESH_MS);
  }

  /** Shows an edit in the changed-files list at once; the next load from the edit log corrects the counts. */
  private liveChange(rel: string, diff: string): void {
    const { added, removed } = diffStats(diff);
    const prev = this.changes.get(rel);
    const status = nextStatus(diff, prev?.status);
    if (status) this.changes.set(rel, { path: rel, added: (prev?.added ?? 0) + added, removed: (prev?.removed ?? 0) + removed, status, seq: ++this.changeSeq });
    else this.changes.delete(rel);
    this.postChanges();
    this.queueChanges();
  }

  private postChanges(): void {
    this.post({ type: 'changes', files: [...this.changes.values()].map(({ seq: _, ...f }) => f) });
  }

  /** Auto-commit: commits the files this task changed, then keeps them in the edit log. */
  private async commitTask(summary: AgentSummary, prompt: string): Promise<void> {
    const root = WorkspaceSync.root();
    // Night mode always commits.
    if (!(this.autoCommit || this.night) || !root || !summary.changedFiles.length) return;
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
        for (const it of this.allItems()) if (it.kind === 'file' && !it.undone && !it.kept && (!paths || paths.includes(it.path))) it.kept = true;
        await this.persist();
        this.renderTab();
      } else {
        const r = await client.request<{ reverted: string[]; conflicts: { path: string }[] }>('edits/revert', { paths });
        if (r.conflicts.length) vscode.window.showWarningMessage(`task-x kept ${r.conflicts.map((c) => c.path).join(', ')} because it changed after the agent's edit.`);
        for (const it of this.allItems()) if (it.kind === 'file' && !it.kept && r.reverted.includes(it.path)) it.undone = true;
        await this.persist();
        this.renderTab();
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
    const it = this.allItems().find((i): i is Extract<Item, { kind: 'tool' }> => i.kind === 'tool' && i.id === id);
    return it?.output;
  }

  /** The items of every open tab. */
  private allItems(): Item[] {
    return this.tabs.flatMap((t) => t.items);
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
      for (const it of this.allItems()) if (it.kind === 'file' && it.operation === operation && r.restored.includes(it.path)) it.undone = true;
      await this.persist();
      this.renderTab();
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
    client.onRequest('agent/requestApproval', (p) =>
      editMode() === 'auto' ? Promise.resolve({ decision: 'allowSession' as const }) : this.requestApproval(p as ApprovalRequest),
    );
    client.onRequest('agent/askUser', (p) => this.askUser(p as AskUserRequest));
  }

  /** Shows the agent's questions in the chat and resolves with the answers, or null when dismissed. */
  private askUser(req: AskUserRequest): Promise<AskUserResult> {
    if (!this.taskId || !req.taskId.startsWith(this.taskId) || !this.busy) return Promise.resolve<AskUserResult>({ answers: null });
    this.view?.show?.(true);
    this.notify('input', 'task-x has questions for you', 'Answer');
    if (this.activeTab !== this.chat.id) this.showTab(this.chat.id);
    const card = { type: 'questions', callId: req.callId, questions: req.questions };
    this.cards.set(req.callId, card);
    this.post(card);
    return new Promise((resolve) => {
      this.questions.set(req.callId, {
        questions: req.questions,
        done: (res) => {
          this.questions.delete(req.callId);
          this.cards.delete(req.callId);
          this.postChat({ type: 'questionsDone', callId: req.callId });
          this.add({ kind: 'questions', questions: req.questions, answers: res.answers, other: res.other });
          resolve(res);
        },
      });
    });
  }

  private requestApproval(req: ApprovalRequest): Promise<{ decision: ApprovalDecision }> {
    if (!this.taskId || !req.taskId.startsWith(this.taskId) || !this.busy) return this.foreignApproval?.(req) ?? Promise.resolve({ decision: 'deny' });
    this.view?.show?.(true);
    this.notify('input', `task-x needs your approval to use ${req.tool}`, 'Review');
    if (this.activeTab !== this.chat.id) this.showTab(this.chat.id);
    const card = {
      type: 'approval',
      callId: req.callId,
      tool: req.tool,
      permission: req.permission,
      detail: toolDetail(req.tool, req.input),
      preview: req.preview ?? JSON.stringify(req.input, null, 2).slice(0, 6_000),
    };
    this.cards.set(req.callId, card);
    this.post(card);
    return new Promise((resolve) => {
      this.approvals.set(req.callId, (decision) => {
        this.cards.delete(req.callId);
        this.postChat({ type: 'approvalDone', callId: req.callId, decision });
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
    this.lastActivity = Date.now();
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
        this.postChat({ type: 'delta', text: e.text });
        break;
      case 'retry':
        if (state.assistant) {
          this.items = this.items.filter((i) => i !== state.assistant);
          state.assistant = undefined;
          this.renderChat();
        }
        this.add({ kind: 'context', text: `Retrying (${e.attempt}) after: ${e.reason}` });
        break;
      case 'toolStarted':
        state.assistant = undefined;
        this.add({ kind: 'tool', id: e.callId, tool: e.tool, detail: toolDetail(e.tool, e.input), stop: this.stoppable(e.permission, this.taskId, e.callId, e.tool) });
        break;
      case 'toolFinished': {
        state.assistant = undefined;
        // A background command answers twice: at once ("runs in the background"), then with its output. The row stays running until then.
        if (!e.background && isBackgroundStart(e.output)) break;
        const it = this.items.find((i): i is Extract<Item, { kind: 'tool' }> => i.kind === 'tool' && i.id === e.callId);
        const out = this.captureOutput(e.callId, e.tool, e.ok, e.output);
        if (it) {
          Object.assign(it, { ok: e.ok }, out);
          this.postChat({ type: 'toolDone', id: e.callId, ok: e.ok, ...out });
        } else {
          // Rejected before it ran (not available, denied, or repeated failure).
          this.add({ kind: 'tool', id: e.callId, tool: e.tool, detail: 'not run', ok: false, ...out });
        }
        break;
      }
      case 'usage':
        // Token counts per model turn, so the run shows its input and output as it goes. No price:
        // the chat reports what was used, not what it cost.
        this.postChat({ type: 'usage', input: e.usage.inputTokens ?? 0, output: e.usage.outputTokens ?? 0 });
        break;
      case 'steered': {
        const i = this.steering.indexOf(e.text);
        if (i >= 0) this.steering.splice(i, 1);
        this.add({ kind: 'steer', text: e.text });
        this.postQueue();
        break;
      }
      case 'fileChanged': {
        this.liveChange(e.path, e.diff);
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
        this.openSubagentTab(e);
        break;
      case 'subagentFinished': {
        const sub = this.tabs.find((t) => t.laneTaskId === e.taskId);
        if (sub) {
          sub.status = e.status === 'completed' ? 'completed' : e.status === 'cancelled' ? 'cancelled' : 'failed';
          sub.assistant = undefined;
          this.postTabs();
        }
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
        const sub = this.tabs.find((t) => t.laneTaskId === e.taskId);
        if (sub) this.feedSubagent(sub, inner);
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
        else if (inner.type === 'toolStarted') this.add({ kind: 'tool', id: `${e.taskId}:${inner.callId}`, tool: tag + inner.tool, detail: toolDetail(inner.tool, inner.input), stop: this.stoppable(inner.permission, e.taskId, inner.callId) });
        else if (inner.type === 'toolFinished') {
          const it = this.items.find((i): i is Extract<Item, { kind: 'tool' }> => i.kind === 'tool' && i.id === `${e.taskId}:${inner.callId}`);
          if (it) {
            const out = this.captureOutput(it.id, inner.tool, inner.ok, inner.output);
            Object.assign(it, { ok: inner.ok }, out);
            this.postChat({ type: 'toolDone', id: it.id, ok: inner.ok, ...out });
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
        this.chat.usage = { used: e.usedTokens, window: e.windowTokens };
        this.postChat({ type: 'contextUsage', used: e.usedTokens, window: e.windowTokens });
        break;
      case 'hook':
        if (e.blocked) this.add({ kind: 'context', text: `Hook blocked ${e.event}: ${e.message}` });
        break;
    }
  }

  /**
   * Rewrites the typed prompt with the routed model (`task-x.routing` for the prompt's task kind), else the
   * active provider model. The webview puts the result in the input, where Ctrl+Z restores the original.
   */
  private async refine(raw: string): Promise<void> {
    const text = raw.trim();
    const fail = (error: string) => this.post({ type: 'refined', error });
    if (!text) return fail('Type a prompt to refine.');
    if (text.length > MAX_INPUT_CHARS) return fail(`The prompt is longer than ${MAX_INPUT_CHARS} characters.`);
    try {
      const client = await this.getClient();
      const provider = await providerConfig(this.ctx.secrets).catch(() => undefined);
      const ed = this.editorContext(text);
      const focusPaths = [...(ed.mentions ?? []), ...(ed.activeFile ? [ed.activeFile] : [])];
      const r = await client.request<RefineResult>('prompt/refine', { prompt: text, provider, routes: await routes(this.ctx.secrets), focusPaths });
      this.log('info', `prompt refined by ${r.model}${r.routed ? ` (route for ${r.kind})` : ''}`);
      this.post({ type: 'refined', text: r.text, model: r.model });
    } catch (e) {
      fail(describeError(e));
      void vscode.window.showWarningMessage(`task-x: refine failed: ${describeError(e)}`);
    }
  }

  /** Commands of the task-x engine, and Claude Code's `Bash` calls, can be stopped one by one (`agent/cancelTool`). */
  private stoppable(permission: string, taskId: string | undefined, callId: string, tool = ''): { taskId: string; callId: string } | undefined {
    return taskId && (STOPPABLE.has(permission) || (permission === 'claude' && tool === 'Bash')) ? { taskId, callId } : undefined;
  }

  /** Sends a prompt from a command (context menu etc.), optionally in a specific mode. */
  async submit(text: string, mode?: ChatMode): Promise<void> {
    await vscode.commands.executeCommand(`${ChatViewProvider.viewId}.focus`);
    if (this.busy) return void vscode.window.showWarningMessage('task-x is busy. Stop the current request first.');
    await this.send(text, mode);
  }

  /** Shows an earlier task: in its open tab, else in the current tab when it is empty, else in a new tab. */
  async openTask(taskId: string): Promise<void> {
    const open = this.tabs.find((t) => t.kind === 'chat' && t.taskId === taskId);
    const cur = this.tabs.find((t) => t.id === this.chatTabId);
    const reuse = !open && cur && !cur.items.length && !cur.taskId && !(this.busy && cur.id === this.runTab) ? cur : undefined;
    if (!open && !reuse && this.tabs.filter((t) => t.kind === 'chat').length >= MAX_CHAT_TABS) {
      return void vscode.window.showWarningMessage(`task-x: close a chat tab first. Up to ${MAX_CHAT_TABS} chats can be open.`);
    }
    let tab = open;
    if (!tab) {
      const saved = this.chats().find((c) => c.id === taskId);
      const items = saved ? saved.items : await this.loadTask(taskId);
      tab = reuse ?? this.addTab(blankTab());
      tab.items = items;
      tab.taskId = taskId;
      if (!this.busy) await this.setNightRequest(undefined);
    }
    this.showTab(tab.id);
    await vscode.commands.executeCommand(`${ChatViewProvider.viewId}.focus`);
  }

  /** Rebuilds a transcript from a task the runtime saved. */
  private async loadTask(taskId: string): Promise<Item[]> {
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
    return items;
  }

  get currentTaskId(): string | undefined {
    return this.taskId;
  }

  /** Runs a request in a chat tab (`tabId`, else the current chat). While a task runs, the request waits in the queue. */
  private async send(raw: string, modeOverride?: ChatMode, retry = false, images: ImageInput[] = [], tabId?: string): Promise<void> {
    const text = raw.trim() || (images.length ? `Look at the attached image${images.length > 1 ? 's' : ''}.` : '');
    if (!text) return;
    if (text.length > MAX_INPUT_CHARS) return void vscode.window.showWarningMessage(`task-x: the message is longer than ${MAX_INPUT_CHARS} characters.`);
    if (this.busy) {
      if (this.queue.length >= MAX_QUEUE) return void vscode.window.showWarningMessage(`task-x: only ${MAX_QUEUE} messages can wait. Stop the task or wait for one to finish.`);
      this.queue.push({ text, mode: modeOverride, images, tab: tabId ?? this.chat.id });
      return void this.postQueue();
    }
    this.runTab = this.nightTab = tabId && this.tabs.some((t) => t.id === tabId && t.kind === 'chat') ? tabId : this.chat.id;
    const mode: ChatMode = modeOverride ?? 'agent';
    const abort = new AbortController();
    this.setBusy(abort);
    this.add({ kind: 'user', text, mode, ...(images.length ? { images: images.length } : {}) });
    const before = this.lastSummary();
    this.stalled = false;
    this.lastActivity = Date.now();
    if (!retry) {
      this.nightFailures = 0;
      clearTimeout(this.nightTimer);
      this.nightTimer = undefined;
      void this.setNightRequest(text);
    }
    let off = () => {};
    // Night mode with a loop model runs through task-x with that model, whatever the chat engine is.
    const worker = this.night ? nightModels().worker : undefined;
    if (engine() === 'claudeCode' && !worker) {
      try {
        const task = this.night ? await this.nightTask(text, retry, abort.signal) : text;
        await this.sendClaude(text, mode, abort.signal, (o) => (off = o), task, images);
      } catch (e) {
        this.add({ kind: 'error', text: describeError(e), retryText: e instanceof RpcError && e.cancelled ? undefined : text });
      } finally {
        off();
        const tab = this.chat.id;
        this.queue.push(...this.steering.splice(0).map((text) => ({ text, tab })));
        this.setBusy(undefined);
        await this.persist();
        this.drain();
        this.afterRun(before, abort.signal);
      }
      return;
    }
    try {
      const client = await this.getClient();
      this.wire(client);
      this.taskId ??= `c${Date.now()}${randomBytes(3).toString('hex')}`;
      const taskId = this.taskId;
      const prompt = this.night ? await this.nightTask(text, retry, abort.signal) : text;
      const provider = worker && isProviderRef(worker.kind) ? await providerConfigWith(this.ctx.secrets, worker.kind, worker.model) : undefined;
      const params = await runParams(this.ctx.secrets, { prompt, mode, taskId, editor: this.editorContext(text), provider, images });
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
        summary = await client.request<AgentSummary>('agent/run', { ...params, prompt: CONTINUE_PROMPT, images: [] }, { signal: abort.signal });
      }
      this.add({ kind: 'summary', ...summaryText(summary) });
      await this.commitTask(summary, text);
      this.finished(summary, text);
      void this.refreshChanges();
    } catch (e) {
      this.add({ kind: 'error', text: describeError(e), retryText: e instanceof RpcError && e.cancelled ? undefined : text });
    } finally {
      off();
      const tab = this.chat.id;
      for (const it of this.items) if (it.kind === 'lane' && it.status === 'running') it.status = 'cancelled';
      for (const t of this.tabs) {
        if (t.parent !== tab || t.status !== 'running') continue;
        t.status = 'cancelled';
        t.assistant = undefined;
      }
      this.approvals.clear();
      this.cards.clear();
      // Steers the task never read are sent again as the next message, so nothing is lost.
      this.queue.push(...this.steering.splice(0).map((text) => ({ text, tab })));
      this.setBusy(undefined);
      await this.persist();
      this.drain();
      this.afterRun(before, abort.signal);
    }
  }

  /** Shows the messages waiting in the tab on screen: steers of its running task first, then its queued messages. */
  private postQueue(): void {
    const tab = this.activeTab;
    const running = this.busy && tab === this.runTab;
    this.post({ type: 'queue', items: this.queue.filter((q) => (q.tab ?? this.runTab) === tab).map((q) => q.text), steering: running ? this.steering : [] });
  }

  /** While a task runs, Enter steers it: the agent reads the message at its next step. Otherwise it is queued. */
  private async steer(text: string): Promise<void> {
    const t = text.trim();
    if (!t || !this.taskId || this.steering.length >= MAX_STEERS) return this.send(text);
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
    if (next) void this.send(next.text, next.mode, false, next.images, next.tab);
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
  private async sendClaude(text: string, mode: ChatMode, signal: AbortSignal, setOff: (off: () => void) => void, task = text, images: ImageInput[] = []): Promise<void> {
    const cliPath = findClaude();
    if (!cliPath) throw new Error('Claude Code CLI not found. Install it (https://docs.anthropic.com/claude-code) or set task-x.claudeCode.path.');
    if (!(await claudeAuth()).loggedIn) {
      void vscode.window.showWarningMessage('Sign in to your Anthropic account to use Claude Code.', 'Sign In').then((pick) => pick && claudeSignIn(() => this.refreshStatus()));
      throw new Error('Not signed in to Anthropic. Run "task-x: Sign In with Anthropic".');
    }
    const client = await this.getClient();
    // Claude Code's permission prompts (MCP tools such as Figma, Bash, edits) come back as agent/requestApproval.
    this.wire(client);
    this.taskId ??= `c${Date.now()}${randomBytes(3).toString('hex')}`;
    const taskId = this.taskId;
    const c = vscode.workspace.getConfiguration(SECTION);
    const sessions = this.ctx.workspaceState.get<Record<string, string>>(CLAUDE_SESSIONS_KEY, {});
    const ed = this.editorContext();
    let prompt = task;
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
        images,
        mcpServers: claudeMcpServers(this.ctx),
        ...claudePermissions(mode),
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
    const csp = `default-src 'none'; style-src ${webview.cspSource}; font-src ${webview.cspSource}; img-src ${webview.cspSource} data:; script-src 'nonce-${nonce}';`;
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
<nav id="views" class="views" aria-label="Chats">
  <div id="tabStrip" class="tab-strip" role="tablist" aria-label="Open chats"></div>
  <button type="button" id="newChat" class="action new-tab" title="New chat in a new tab" aria-label="New chat"><span class="codicon codicon-add" aria-hidden="true"></span></button>
  <span class="spacer"></span>
  <button type="button" role="tab" id="tabHistory" class="view-tab" aria-controls="history" title="History"><span class="codicon codicon-history" aria-hidden="true"></span><span class="view-label">History</span></button>
</nav>
<div id="progress" role="progressbar" aria-label="Working" aria-hidden="true"></div>
<div id="runbar" class="runbar" role="status" hidden><span class="codicon codicon-loading codicon-modifier-spin" aria-hidden="true"></span><span class="runbar-text">Working</span><span id="runTimer" class="runbar-time">0:00</span><span class="spacer"></span><button type="button" id="runStop" class="btn small secondary"><span class="codicon codicon-debug-stop" aria-hidden="true"></span>Stop</button></div>
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
  <div id="attachments" class="attachments" aria-label="Attached images" hidden></div>
  <input type="file" id="attachFile" accept="image/png,image/jpeg,image/gif,image/webp" multiple hidden>
  <label for="input" class="sr-only">Message</label>
  <textarea id="input" rows="1"></textarea>
  <div class="toolbar" role="toolbar" aria-label="Chat options">
    <button type="button" id="modelBtn" class="pick"></button>
    <button type="button" id="nightPlanBtn" class="pick" hidden></button>
    <button type="button" id="nightLoopBtn" class="pick" hidden></button>
    <button type="button" id="settingsBtn" class="pick settings-pick"></button>
    <span class="spacer"></span>
    <span id="ctxMeter" class="meter" role="meter" aria-valuemin="0" aria-valuemax="100" hidden></span>
    <button type="button" id="indexBtn" class="pick index" hidden></button>
    <button type="button" id="attachBtn" class="action" title="Attach images. You can also paste or drop them into the input." aria-label="Attach images"><span class="codicon codicon-file-media" aria-hidden="true"></span></button>
    <button type="button" id="refineBtn" class="action" title="Refine: rewrite the prompt with the routed model (task-x.routing), else the active model. Ctrl+Z restores your text." aria-label="Refine prompt"><span class="codicon codicon-sparkle" aria-hidden="true"></span></button>
    <button type="button" id="send" class="action primary"></button>
  </div>
</form>
<script nonce="${nonce}" src="${hljs}"></script>
<script nonce="${nonce}" src="${js}"></script>
</body>
</html>`;
  }
}
