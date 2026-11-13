import { AsyncLocalStorage, AsyncResource } from 'node:async_hooks';
import { randomBytes } from 'node:crypto';
import os from 'node:os';
import path from 'node:path';
import * as vscode from 'vscode';
import { EDIT_MODES, type EditMode, activeRef, editMode, effort, sandboxOn, setSandbox, windowChoice, hasApiKey, isProviderKind, isProviderRef, modelFor, nightModels, setEditMode, toolModels, providerConfig, providerConfigWith, providerRefs, refKind, refLabel, refNeedsKey, setProvider, setWindowChoice, settings, SECTION } from './config.js';
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
  EFFORTS,
  type Effort,
  type EditorContext,
  type ImageInput,
  imagesFrom,
  type ModelInfo,
  type ProviderErrorData,
  type RefineResult,
  type ResolvedSkill,
  type AgentInfo,
  type PluginInfo,
  type SkillInfo,
} from './protocol.js';
import { type PluginEntry, catalog, managePlugin, newAgent, newCommand, newSkill, setPlugin, viewPlugin } from './catalog.js';
import { CLAUDE_KIND, CLAUDE_LABEL, claudeAuth, claudeModel, claudePermissions, claudeSignIn, claudeSubagentModel, claudeSignOut, engine, findClaude, useClaude } from './claudeCode.js';
import { CLAUDE_MODEL_INFO, type Listed, MODEL_LIST_TTL_MS, formatTokens, listModels, missingNote, withSaved } from './models.js';
import { type JsonRpcClient, RpcError } from './rpc.js';
import { diffStats, formatOutput } from './output.js';
import { EXPORT_SIZES, PREVIEW_SIZE, exportedFiles, isWorkspaceSvg, svgPathFrom } from './vector.js';
import { formatCost, limits, routes, runParams } from './runParams.js';
import { exportHtml, manageApprovals, showActiveAgents, showTimeline } from './sessionTools.js';
import { commitPaths, commitSubject } from './autoCommit.js';
import { WorkspaceSync } from './workspace.js';
import { claudeDisabledMcp, claudeMcpServers, disabledMcp, mcpOrigins, setMcpDisabled } from './mcpSources.js';
import { setResourceReader, withHashContext } from './hashContext.js';
import { activeChatMode, chatModes, setAgent, setChatMode, withAgent, withChatMode } from './chatModes.js';
import { elicit } from './mcpElicit.js';
import { type DocsSource, listDocs, withDocs } from './docs.js';

const TRANSCRIPT_KEY = 'task-x.transcript';
const INCLUDE_EDITOR_KEY = 'task-x.includeEditor';
const AUTO_COMMIT_KEY = 'task-x.autoCommit';
/** `@path` references in a prompt: a path of letters, digits, and . _ - / after an @ at a word start. */
// `@docs:<name>` is a docs set, not a file.
const MENTION = /(?:^|\s)@(?!docs:)((?:[\w.-]+\/)*[\w.-]+\/?)/g;
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

/**
 * Session guards (`task-x.session.*`), in minutes or hours; 0 turns one off. A chat idle this long
 * starts a fresh conversation, a run stops without progress or past its time, and Loop Mode ends.
 */
function sessionLimits() {
  const c = vscode.workspace.getConfiguration(SECTION);
  const n = (k: string, def: number, max: number) => {
    const v = c.get<number>(k, def);
    return typeof v === 'number' && Number.isFinite(v) ? Math.min(max, Math.max(0, v)) : def;
  };
  return {
    freshMin: n('session.freshAfterIdleMinutes', 60, 7 * 24 * 60),
    stallMin: n('session.stallMinutes', 30, 24 * 60),
    maxRunMin: n('session.maxRunMinutes', 120, 24 * 60),
    loopHours: n('session.loopMaxHours', 4, 72),
  };
}
const fmtIdle = (ms: number) => (ms >= 3_600_000 ? `${Math.round(ms / 3_600_000)} h` : `${Math.round(ms / 60_000)} min`);
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
  | { kind: 'user'; text: string; mode: ChatMode; images?: number; context?: SentContext[]; id?: string; operations?: number[] }
  | { kind: 'assistant'; text: string }
  | { kind: 'tool'; id: string; tool: string; detail: string; ok?: boolean; output?: string; outputCut?: boolean; stop?: { taskId: string; callId: string }; tokens?: number; browser?: string; svg?: string }
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
export type Lane = {
  kind: 'lane';
  taskId: string;
  name: string;
  role: string;
  /** The custom agent the subagent runs as, if any. */
  agent?: string | null;
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
  /** False for Claude Code subagents: the CLI cannot stop one subagent alone. */
  stoppable?: boolean;
  /** When it started and ended (epoch ms), for the elapsed time on the Subtasks board. */
  startedMs?: number;
  endedMs?: number;
};

type Inbound =
  | { type: 'ready' }
  | { type: 'send'; text: string; queue?: boolean; images?: unknown }
  | { type: 'cancel'; tab?: unknown }
  | { type: 'clear' }
  | { type: 'newTab' }
  | { type: 'selectTab'; id: string }
  | { type: 'closeTab'; id: string }
  | { type: 'openSubagent'; taskId: string }
  | { type: 'pinTab'; id: string }
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
  | { type: 'selectEffort'; effort: string }
  | { type: 'selectAgent'; name: string }
  | { type: 'slashAction'; action: string; key?: string; enable?: boolean }
  | { type: 'editWaiting'; kind: string; index: number; text: string }
  | { type: 'selectSandbox'; enabled: boolean }
  | { type: 'listMcp' }
  | { type: 'restoreTo'; id: string }
  | { type: 'listTools' }
  | { type: 'toggleTool'; name: string; on: boolean }
  | { type: 'mcpAction'; action: string; name?: string; enable?: boolean }
  | { type: 'openSettings'; query: string }
  | { type: 'suggest'; text: string; mode: string }
  | { type: 'copyOutput'; id: string }
  | { type: 'openOutput'; id: string }
  | { type: 'stopLane'; taskId: string }
  | { type: 'steerLane'; taskId: string; text: string }
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
  | { type: 'answerQuestions'; callId: string; answers: unknown; other?: unknown }
  | { type: 'svgPreview'; id: string; path: string }
  | { type: 'exportSvg'; path: string };

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
  /** Subagent tabs: on the tab strip (opened from its lane card), and kept there after the run ends. */
  shown?: boolean;
  pinned?: boolean;
  usage?: { used: number; window: number };
  assistant?: Extract<Item, { kind: 'assistant' }>;
  /** The plan checklist of this chat's newest task. */
  todos?: Todo[];
  /** Tokens the chat's task used so far (the runtime's running total for the task). */
  tokens?: { input: number; output: number; cached: number };
  /** When the chat's last request ended, for the fresh start after idle time. */
  lastUsed?: number;
}

/** One chat tab's running task: each tab runs its own, at the same time as the others. */
interface Run {
  abort: AbortController;
  started: number;
  /** Approval and question cards, shown again when the tab comes back on screen. */
  cards: Map<string, object>;
  approvals: Map<string, (d: ApprovalDecision | 'always') => void>;
  questions: Map<string, { questions: Question[]; done: (res: AskUserResult) => void }>;
  /** Messages sent to the running task that it has not read yet. */
  steering: string[];
  /** The request that runs, so its edits can be restored from its message (#11). */
  lastRequest?: Extract<Item, { kind: 'user' }>;
  /** True when Loop Mode stopped a stalled run, so the stop is not taken as the user's. */
  stalled: boolean;
  lastActivity: number;
}

/** Chat tabs that run a task at the same time; a request beyond this waits for a free slot. */
const MAX_PARALLEL_RUNS = 4;

const blankTab = (items: Item[] = [], taskId?: string): ChatTab => ({ id: `t${Date.now().toString(36)}${randomBytes(3).toString('hex')}`, kind: 'chat', items, taskId });

/** Chat tabs are always on the strip; a subagent's tab only after its lane card opened it. */
const onStrip = (t: ChatTab): boolean => t.kind === 'chat' || !!t.shown;

/** A chat tab and its subagents, for the Agents view. */
export type AgentChat = { tabId: string; title: string; running: boolean; lanes: Lane[] };
/** What the Agents view can do to one subagent. */
export type LaneAction = 'show' | 'open' | 'stop' | 'message' | 'retry' | 'apply' | 'discard';

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

export interface Todo {
  content: string;
  status: 'pending' | 'in_progress' | 'completed';
}

/** task-x's `todo_write` and Claude Code's `TodoWrite`, also as an MCP or subagent tool name. */
const TODO_TOOL = /(^|__|› )(todo_write|TodoWrite)$/;

/** The checklist of a `todo_write` or `TodoWrite` call, or undefined for any other tool. */
export function todoList(tool: string, input: Record<string, unknown>): Todo[] | undefined {
  if (!TODO_TOOL.test(tool) || !Array.isArray(input.todos)) return undefined;
  return (input.todos as unknown[])
    .map((t) => t as { content?: unknown; status?: unknown })
    .filter((t) => typeof t?.content === 'string' && t.content.trim())
    .slice(0, 30)
    .map((t) => ({ content: String(t.content).slice(0, 300), status: t.status === 'completed' || t.status === 'in_progress' ? t.status : 'pending' }));
}

/** A file that went with a message, shown as a chip under it. */
interface SentContext {
  path: string;
  lines?: string;
}

/** The open file (or its selection) and the @ mentions, in the order the chips show them. */
function sentContext(ed: EditorContext): SentContext[] {
  const sel = ed.selection;
  const out: SentContext[] = sel
    ? [{ path: sel.path, lines: sel.startLine === sel.endLine ? `L${sel.startLine}` : `L${sel.startLine}-${sel.endLine}` }]
    : ed.activeFile
      ? [{ path: ed.activeFile }]
      : [];
  for (const m of ed.mentions ?? []) if (!out.some((c) => c.path === m)) out.push({ path: m });
  return out;
}

function toolDetail(tool: string, input: Record<string, unknown>): string {
  const s = (k: string) => (typeof input[k] === 'string' ? (input[k] as string) : Array.isArray(input[k]) ? (input[k] as unknown[]).map(String).join(' ') : '');
  // The browser row says what the agent does there: "goto https://…", "click button.go".
  if (tool.replace(/^mcp__task-x__/, '') === 'browser') {
    const what = s('url') || s('selector') || s('key') || s('target');
    const v = [s('action'), what].filter(Boolean).join(' ');
    return v.length > 120 ? `${v.slice(0, 117)}…` : v;
  }
  // Commands are shown in full; the chat row wraps them.
  const cmd = s('command');
  if (cmd) return cmd;
  const claude = s('file_path') || s('url') || s('notebook_path');
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
const fmtDuration = (ms: number) => {
  const s = Math.max(0, Math.round(ms / 1000));
  return s >= 3600 ? `${Math.floor(s / 3600)} h ${Math.round((s % 3600) / 60)} min` : s >= 60 ? `${Math.floor(s / 60)} min ${s % 60} s` : `${s} s`;
};

function summaryText(s: AgentSummary): { text: string; ok: boolean; canContinue: boolean; title: string; stats: string[]; error?: string; signIn: boolean; raiseLimit: boolean } {
  const status: Record<string, string> = {
    completed: 'Done',
    cancelled: 'Stopped',
    interrupted: 'Interrupted: the editor reloaded or the runtime stopped',
    sleeping: 'Sleeping until a wake-up is due',
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
  const canContinue = ['maxIterations', 'budgetExceeded', 'deadlineExceeded', 'cancelled', 'interrupted'].includes(s.status);
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
  /** The running task of each chat tab, by tab id. */
  private readonly runs = new Map<string, Run>();
  /**
   * The tab a run's code works for. Several tabs run at once, so `chat`, `items`, and `taskId` resolve
   * through this (it follows the run across every `await`), not through what is on screen.
   */
  private readonly scope = new AsyncLocalStorage<string>();
  private tabs: ChatTab[];
  /** The tab on screen. */
  private activeTab: string;
  /** The chat tab the user works in: the last chat tab on screen. */
  private chatTabId: string;
  /** Night mode runs again in the tab of its last run. */
  private nightTab: string | undefined;
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
  /** Messages sent to a tab while its task runs (or while every run slot is taken); each runs after the one before it. */
  private readonly queue: { text: string; mode?: ChatMode; images?: ImageInput[]; tab: string }[] = [];
  private night: boolean;
  /** The last request the user sent; Night mode runs it again until a run completes. */
  private nightRequest: string | undefined;
  /** True when the night request is split into todo.md; `nightItem` is the item the current run works on. */
  private nightTodo: boolean;
  private nightItem: string | undefined;
  private nightFailures = 0;
  private nightTimer: NodeJS.Timeout | undefined;
  private nightWatch: NodeJS.Timeout | undefined;
  /** When Loop Mode started its current request, for `session.loopMaxHours`. */
  private nightSince: number | undefined;
  private readonly diffs = new Map<string, string>();
  private readonly outputs = new Map<string, string>();
  /** The browser the last `browser` call ran in ("VS Code", "Chrome", ...). */
  private lastBrowser: string | undefined;
  private readonly wired = new WeakSet<JsonRpcClient>();
  private changesTimer: NodeJS.Timeout | undefined;
  /** Approvals for runtime tasks that are not this chat's (background tasks, best-of-N). */
  foreignApproval?: (req: ApprovalRequest) => Promise<{ decision: ApprovalDecision }>;
  /** Called with every finished chat task (review findings, notifications). */
  onSummary?: (s: AgentSummary) => void;
  /** The pending edits with their original text, after each load (the editor shows them inline). */
  onPending?: (files: { path: string; original: string | null }[]) => void;
  /** Called when the chat starts or stops working (status bar). */
  onBusyChange?: (busy: boolean) => void;
  /** Opens the project graph panel. */
  onShowGraph?: () => void;
  private includeEditor: boolean;
  private autoCommit: boolean;
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

  /** True while any chat tab runs a task (the status bar, commands). */
  get busy(): boolean {
    return this.runs.size > 0;
  }
  /** True while Loop Mode waits to run its request again. */
  get loopPending(): boolean {
    return this.nightTimer !== undefined;
  }

  /** The chat that requests and agent events go to: the run's tab inside a run, else the user's chat tab. */
  private get chat(): ChatTab {
    const id = this.scope.getStore() ?? this.chatTabId;
    return this.tabs.find((t) => t.id === id) ?? this.tabs.find((t) => t.kind === 'chat') ?? this.addTab(blankTab());
  }
  /** The running task of `chat`, if it runs. */
  private get run(): Run | undefined {
    return this.runs.get(this.chat.id);
  }
  /** Runs `fn` for a tab: `chat`, `items`, and `taskId` mean that tab inside it. */
  private inTab<T>(tab: string, fn: () => T): T {
    return this.scope.run(tab, fn);
  }
  /** The chat tab whose running task owns a runtime task id (its own, a subagent's, or a plan's). */
  private tabForTask(taskId: string): string | undefined {
    return this.tabs.find((t) => t.kind === 'chat' && t.taskId && this.runs.has(t.id) && (taskId === t.taskId || taskId.startsWith(`${t.taskId}-`)))?.id;
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

  /**
   * Stops the task of one chat tab (default: the chat on screen, else the only running one) and drops
   * the messages waiting for that tab. Other tabs keep running.
   */
  cancel(tabId?: string): void {
    const tab = tabId ?? (this.runs.has(this.chatTabId) || this.runs.size !== 1 ? this.chatTabId : [...this.runs.keys()][0]);
    const r = this.runs.get(tab);
    if (r) {
      r.abort.abort();
      for (const resolve of r.approvals.values()) resolve('deny');
      r.approvals.clear();
      for (const p of r.questions.values()) p.done({ answers: null });
      r.questions.clear();
    }
    if (tab === this.nightTab || !this.nightTab) {
      clearTimeout(this.nightTimer);
      this.nightTimer = undefined;
    }
    for (let i = this.queue.length - 1; i >= 0; i--) if (this.queue[i].tab === tab) this.queue.splice(i, 1);
    this.postQueue();
  }

  /** Stops every tab's task and empties the queue (runtime restart, shutdown). */
  cancelAll(): void {
    for (const tab of [...this.runs.keys()]) this.cancel(tab);
    this.queue.length = 0;
    clearTimeout(this.nightTimer);
    this.nightTimer = undefined;
    this.postQueue();
  }

  /** Empties the current chat tab: a new task, no transcript, and the context meter reset. */
  async clear(): Promise<void> {
    this.cancel(this.chat.id);
    await this.archive();
    this.items = [];
    this.taskId = undefined;
    this.chat.usage = undefined;
    this.chat.tokens = undefined;
    await this.setNightRequest(undefined);
    await this.persist();
    this.renderChat();
    this.postTabs();
  }

  /** The chat on screen as Markdown: requests, replies, tool calls, changed files, and the summary. */
  chatMarkdown(): string {
    const tab = this.tabs.find((t) => t.id === this.activeTab && t.kind === 'chat') ?? this.chat;
    const fence = (s: string) => (s.includes('```') ? '~~~~' : '```');
    const out: string[] = [`# ${tab.title || 'task-x chat'}`, '', `_Exported from task-x on ${new Date().toISOString().slice(0, 16).replace('T', ' ')}._`, ''];
    for (const it of tab.items) {
      switch (it.kind) {
        case 'user':
          out.push('## You', '', it.text, ...(it.context?.length ? ['', `_Context: ${it.context.map((c) => `\`${c.path}${c.lines ? ` ${c.lines}` : ''}\``).join(', ')}_`] : []), '');
          break;
        case 'assistant':
          if (it.text.trim()) out.push('## task-x', '', it.text.trim(), '');
          break;
        case 'tool':
          out.push(`- \`${it.tool}\`${it.detail ? ` ${it.detail.split('\n')[0].slice(0, 200)}` : ''}${it.ok === false ? ' (failed)' : ''}`);
          if (it.output && /(^|__|› )(run_command|Bash)$/.test(it.tool)) {
            const f = fence(it.output);
            out.push('', `  ${f}`, ...it.output.slice(0, 4_000).split('\n').map((l) => `  ${l}`), `  ${f}`, '');
          }
          break;
        case 'file':
          out.push(`- Changed \`${it.path}\` (+${it.added} −${it.removed})${it.undone ? ', undone' : it.kept ? ', kept' : ''}`);
          break;
        case 'steer':
          out.push('', `> Added while it ran: ${it.text}`, '');
          break;
        case 'summary':
          out.push('', it.title ? `**${it.title}**${it.stats?.length ? ` · ${it.stats.join(' · ')}` : ''}${it.error ? `\n\n${it.error}` : ''}` : `**${it.text}**`, '');
          break;
        case 'error':
          out.push('', `> Error: ${it.text}`, '');
          break;
      }
    }
    return `${out.join('\n').replace(/\n{3,}/g, '\n\n').trim()}\n`;
  }

  /**
   * Exports the chat on screen: as an HTML page (the runtime's `agent/export`, with a turn range), or as
   * Markdown to open, save to a file, or copy.
   */
  async exportChat(): Promise<void> {
    const md = this.chatMarkdown();
    const taskId = this.taskId;
    const pick = await vscode.window.showQuickPick(
      [
        ...(taskId ? [{ label: '$(globe) Export as HTML…', id: 'html', detail: 'One page with the prompts, answers, tool calls, changed files, and timeline; pick the turns' }] : []),
        { label: '$(markdown) Open as Markdown', id: 'open' },
        { label: '$(save) Save to a file…', id: 'save' },
        { label: '$(copy) Copy to the clipboard', id: 'copy', detail: 'To paste into an issue, a pull request, or a message' },
      ],
      { title: 'Export chat' },
    );
    if (pick?.id === 'html' && taskId) await exportHtml(() => this.getClient(), taskId);
    else if (pick?.id === 'open') await vscode.window.showTextDocument(await vscode.workspace.openTextDocument({ language: 'markdown', content: md }));
    else if (pick?.id === 'copy') {
      await vscode.env.clipboard.writeText(md);
      vscode.window.showInformationMessage('task-x: the chat is on the clipboard as Markdown.');
    } else if (pick?.id === 'save') {
      const root = WorkspaceSync.root();
      const name = `${(this.chat.title || 'chat').replace(/[^\w.-]+/g, '-').slice(0, 60) || 'chat'}.md`;
      const uri = await vscode.window.showSaveDialog({ defaultUri: root ? vscode.Uri.joinPath(vscode.Uri.file(root), name) : undefined, filters: { Markdown: ['md'] } });
      if (uri) await vscode.workspace.fs.writeFile(uri, Buffer.from(md, 'utf8'));
    }
  }

  /** Opens an empty chat in a new tab, after the open tabs and before History. */
  async newChat(): Promise<void> {
    if (this.tabs.filter((t) => t.kind === 'chat').length >= MAX_CHAT_TABS) return void vscode.window.showWarningMessage(`task-x: close a chat tab first. Up to ${MAX_CHAT_TABS} chats can be open.`);
    this.showTab(this.addTab(blankTab()).id);
    // A new chat starts clean: no conversation (a new task id), no context meter, plan, or images.
    this.post({ type: 'contextUsage', used: 0, window: 0 });
    this.post({ type: 'clearDraft' });
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
    if (tab.kind === 'chat') this.chatTabId = id;
    this.postTabs();
    this.renderTab();
    this.post({ type: 'view', view: 'chat' });
    this.refreshStatus();
    void this.saveTabs();
  }

  /** Closes a tab. The running task's tab stays open; closing the last chat opens an empty one. */
  private async closeTab(id: string): Promise<void> {
    const tab = this.tabs.find((t) => t.id === id);
    if (!tab) return;
    // A subagent's tab only leaves the strip: its lane card can open it again.
    if (tab.kind === 'subagent') {
      tab.shown = tab.pinned = false;
      if (this.activeTab === id) return this.showTab(this.tabs.some((t) => t.id === tab.parent) ? tab.parent! : this.chatTabId);
      return this.postTabs();
    }
    if (this.runs.has(tab.id)) return void vscode.window.showWarningMessage('task-x: stop the running task before you close its tab.');
    await this.archive(tab);
    const strip = this.tabs.filter(onStrip);
    const i = strip.indexOf(tab);
    // A closed chat takes its subagents with it.
    this.tabs = this.tabs.filter((t) => t !== tab && t.parent !== id);
    if (!this.tabs.some((t) => t.kind === 'chat')) this.addTab(blankTab());
    if (this.chatTabId === id) this.chatTabId = (strip.slice(0, i).findLast((t) => t.kind === 'chat') ?? this.tabs.find((t) => t.kind === 'chat'))!.id;
    if (this.activeTab === id) {
      const left = this.tabs.filter(onStrip);
      return this.showTab(left[Math.min(i, left.length - 1)].id);
    }
    this.postTabs();
    await this.saveTabs();
  }

  /**
   * Opens a subagent's tab from its lane card. Unpinned subagent tabs share one preview slot, so
   * opening another one replaces it; pinning keeps a tab on the strip.
   */
  private openSubagent(taskId: string): void {
    const sub = this.tabs.find((t) => t.kind === 'subagent' && t.laneTaskId === taskId);
    if (!sub) return void vscode.window.showInformationMessage('task-x: this subagent’s activity is no longer available.');
    if (!sub.shown) for (const t of this.tabs) if (t.kind === 'subagent' && t.shown && !t.pinned) t.shown = false;
    sub.shown = true;
    this.showTab(sub.id);
  }

  private pinTab(id: string): void {
    const sub = this.tabs.find((t) => t.id === id && t.kind === 'subagent' && t.shown);
    if (!sub) return;
    sub.pinned = true;
    this.postTabs();
  }

  /** When a chat's run ends, the preview tabs of its subagents close; pinned ones stay. */
  private closePreviews(parent: string): void {
    const previews = this.tabs.filter((t) => t.parent === parent && t.shown && !t.pinned);
    for (const t of previews) t.shown = false;
    if (previews.some((t) => t.id === this.activeTab)) this.showTab(this.tabs.some((t) => t.id === parent) ? parent : this.chatTabId);
  }

  /** Empties a chat tab after its task was deleted. */
  private async resetTab(tab: ChatTab): Promise<void> {
    tab.items = [];
    tab.taskId = undefined;
    tab.usage = undefined;
    tab.tokens = undefined;
    if (tab.id === this.chat.id) await this.setNightRequest(undefined);
    if (tab.id === this.activeTab) this.renderTab();
  }

  private postTabs(): void {
    // Tabs that close or reset take their subagents off the Agents view.
    this.lanesChanged.fire();
    const tabs = this.tabs.filter(onStrip).map((t) => ({
      id: t.id,
      kind: t.kind,
      title: tabTitle(t),
      role: t.role ?? null,
      preview: t.kind === 'subagent' && !t.pinned,
      status: t.kind === 'subagent' ? t.status : this.runs.has(t.id) ? 'running' : null,
      started: t.kind === 'chat' ? (this.runs.get(t.id)?.started ?? null) : null,
    }));
    this.post({ type: 'tabs', tabs, active: this.activeTab });
  }

  /** Draws the tab on screen: its transcript, context meter, waiting messages, and open cards. */
  private renderTab(): void {
    const tab = this.tabs.find((t) => t.id === this.activeTab) ?? this.chat;
    const run = this.runs.get(tab.id);
    const live = tab.kind === 'subagent' ? tab.status === 'running' && !!tab.assistant : !!run;
    this.post({ type: 'history', items: tab.items, streaming: live && tab.items.at(-1)?.kind === 'assistant' });
    this.post({ type: 'contextUsage', used: tab.usage?.used ?? 0, window: tab.usage?.window ?? 0 });
    // A subagent tab shows the tokens of the chat it belongs to.
    const owner = tab.kind === 'chat' ? tab : this.tabs.find((t) => t.id === tab.parent);
    this.post({ type: 'tokens', ...(owner?.tokens ?? { input: 0, output: 0, cached: 0 }) });
    for (const c of run?.cards.values() ?? []) this.post(c);
    this.postQueue();
    this.postTodos();
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
    this.post({ type: 'status', provider: claude ? CLAUDE_LABEL : refLabel(ref), model: claude ? claudeModel() : modelFor(ref) || '(no model)', busy: this.runs.has(this.activeTab), mode: 'agent', editMode: editMode(), autoCommit: this.autoCommit, night: this.night, nightPlanner: nightModels().planner ?? null, nightWorker: nightModels().worker ?? null, refineModel: toolModels().refine ?? null, helperModel: toolModels().helper ?? null, effort: effort() ?? null, sandbox: sandboxOn(), chatMode: activeChatMode()?.name ?? null, agent: windowChoice().agent ?? null });
    void this.postModels();
  }

  dispose(): void {
    clearInterval(this.nightWatch);
    this.cancelAll();
    this.lanesChanged.dispose();
  }

  private readonly lanesChanged = new vscode.EventEmitter<void>();
  /** Fires when a subagent starts, changes, or ends in any chat tab, or a tab's run starts or ends. */
  readonly onDidChangeLanes = this.lanesChanged.event;

  /** Every chat tab that has subagents, with copies of them (the Agents view). */
  agentChats(): AgentChat[] {
    return this.tabs
      .filter((t) => t.kind === 'chat')
      .map((t) => ({ tabId: t.id, title: tabTitle(t), running: this.runs.has(t.id), lanes: t.items.filter((i): i is Lane => i.kind === 'lane').map((l) => ({ ...l, steps: [...l.steps] })) }))
      .filter((c) => c.lanes.length);
  }

  /** Shows a chat tab in the view, revealing the view first. */
  private async revealTab(tabId: string): Promise<boolean> {
    if (!this.tabs.some((t) => t.id === tabId && t.kind === 'chat')) return false;
    await vscode.commands.executeCommand(`${ChatViewProvider.viewId}.focus`);
    if (this.activeTab !== tabId) {
      this.chatTabId = tabId;
      this.showTab(tabId);
    }
    return true;
  }

  /** One action on a subagent of any chat tab (the Agents view). `text` is the message for `message`. */
  async laneAction(tabId: string, taskId: string, action: LaneAction, text?: string): Promise<void> {
    const lane = this.tabs.find((t) => t.id === tabId)?.items.find((i): i is Lane => i.kind === 'lane' && i.taskId === taskId);
    if (!lane) return void vscode.window.showInformationMessage('task-x: that subtask is no longer in its chat.');
    switch (action) {
      case 'show':
        if (await this.revealTab(tabId)) this.post({ type: 'jumpLane', taskId });
        return;
      case 'open':
        if (await this.revealTab(tabId)) this.openSubagent(taskId);
        return;
      case 'stop':
        return this.inTab(tabId, () => this.onMessage({ type: 'stopLane', taskId }));
      case 'message':
        if (text?.trim()) await this.inTab(tabId, () => this.onMessage({ type: 'steerLane', taskId, text }));
        return;
      case 'apply':
      case 'discard':
        if (lane.worktree) await this.inTab(tabId, () => this.settleWorktree(lane.worktree!, action === 'apply'));
        return;
      case 'retry': {
        const why = lane.error ? ` It ended with: ${lane.error.split('\n')[0].slice(0, 300)}` : '';
        const ask = `Run the subtask "${lane.agent || lane.name}" again.${why}\n\nIts goal: ${lane.objective}`;
        // A running chat reads it as a steer; an idle one starts a new request.
        await (this.runs.has(tabId) ? this.inTab(tabId, () => this.steer(ask)) : this.send(ask, undefined, false, [], tabId));
        return;
      }
    }
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
    // Bound to this run's tab: the event arrives outside the run's async context.
    const off = client.onNotification(
      'agent/event',
      AsyncResource.bind((p: unknown) => {
        const { taskId: id, event } = p as AgentEventParams;
        if (id === planId) this.onEvent(event, state);
      }),
    );
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

  /** Watches every run (see `watchRuns`). */
  private watchNight(): void {
    clearInterval(this.nightWatch);
    this.nightWatch = setInterval(() => this.watchRuns(), NIGHT_CHECK_MS);
  }

  /**
   * Loop Mode restarts its run after NIGHT_STALL_MS without progress. Any other run stops after
   * `session.stallMinutes` without progress or after `session.maxRunMinutes`, so no session runs for
   * hours unseen. Waiting for the user is not a stall.
   */
  private watchRuns(): void {
    const now = Date.now();
    const { stallMin, maxRunMin } = sessionLimits();
    for (const [tab, r] of this.runs) {
      if (r.abort.signal.aborted) continue;
      if (r.approvals.size || r.questions.size) {
        r.lastActivity = now;
        continue;
      }
      const idle = now - r.lastActivity;
      if (this.night && tab === this.nightTab) {
        if (idle < NIGHT_STALL_MS) continue;
        r.stalled = true;
        r.lastActivity = now;
        this.inTab(tab, () => this.add({ kind: 'context', text: `Loop Mode: no progress for ${NIGHT_STALL_MS / 60_000} minutes. Restarting the request.` }));
        r.abort.abort();
        continue;
      }
      const why =
        stallMin && idle >= stallMin * 60_000
          ? `no progress for ${stallMin} minutes (task-x.session.stallMinutes)`
          : maxRunMin && now - r.started >= maxRunMin * 60_000
            ? `it ran for ${maxRunMin} minutes (task-x.session.maxRunMinutes)`
            : undefined;
      if (!why) continue;
      this.inTab(tab, () => this.add({ kind: 'context', text: `Stopped the request: ${why}. Long sessions use many tokens. Send Continue to go on, or /clear to start fresh.` }));
      r.abort.abort();
    }
  }

  /** Loop Mode ends after `session.loopMaxHours` on one request, so it does not run all night unseen. */
  private loopExpired(): boolean {
    const h = sessionLimits().loopHours;
    if (!h || !this.nightSince || Date.now() - this.nightSince < h * 3_600_000) return false;
    this.nightSince = undefined;
    clearTimeout(this.nightTimer);
    this.nightTimer = undefined;
    this.inTab(this.nightTab ?? this.chatTabId, () => this.add({ kind: 'context', text: `Loop Mode stopped: it worked on this request for ${h} h (task-x.session.loopMaxHours). Send the request again to go on.` }));
    void this.setNightRequest(undefined).then(() => this.persist());
    return true;
  }

  /** Night mode: after a run that did not complete, runs the last request again with a growing delay. A user Stop ends it. */
  private afterRun(before: Item | undefined, signal: AbortSignal, stalled: boolean): void {
    // Loop Mode follows one chat: the tab of its last request.
    if (!this.night || !this.nightRequest || this.chat.id !== this.nightTab || this.runs.has(this.chat.id)) return;
    if (this.loopExpired()) return;
    const last = this.lastSummary();
    if (last && last !== before && last.ok) {
      this.nightFailures = 0;
      return void this.nextTodo();
    }
    if (signal.aborted && !stalled) return;
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
      if (!this.night || !this.nightRequest || (this.nightTab && this.runs.has(this.nightTab))) return;
      if (this.loopExpired()) return;
      // With a todo list, the transcript and the commit name the item.
      const items = this.nightTodo ? await this.readTodo() : [];
      const n = items.findIndex((i) => !i.done);
      void this.send(n >= 0 ? `Todo ${n + 1}/${items.length}: ${items[n].text}` : nightPrompt(this.nightRequest), undefined, true, [], this.nightTab);
    }, wait);
  }

  /** Night mode: starts the last request again when this chat's last run did not complete. */
  private resumeNight(): void {
    if (!this.night || this.runs.has(this.chat.id) || this.nightTimer || !this.nightRequest) return;
    const lastUser = this.items.findLastIndex((i) => i.kind === 'user');
    const lastDone = this.items.findLastIndex((i) => i.kind === 'summary' && i.ok);
    if (lastUser < 0) return;
    if (lastDone > lastUser) return void this.nextTodo();
    this.nightSince ??= Date.now();
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
    const tabs = this.tabs.filter((t) => t.kind === 'chat').map(({ id, kind, items, taskId, tokens, lastUsed }) => ({ id, kind, items: items.slice(-MAX_ITEMS), taskId, tokens, lastUsed }));
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
    for (const t of this.tabs) if (t.kind === 'chat' && t.taskId === id && !this.runs.has(t.id)) await this.resetTab(t);
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
    for (const t of this.tabs) if (t.kind === 'chat' && t.taskId && ids.has(t.taskId) && !this.runs.has(t.id)) await this.resetTab(t);
    this.postTabs();
    await this.saveTabs();
    await this.postTasks();
  }

  private add(item: Item): void {
    this.items.push(item);
    this.postChat({ type: 'item', item });
    if (item.kind === 'user') this.postTabs();
    if (item.kind === 'lane') this.lanesChanged.fire();
  }

  /** After a tab's task starts or ends: the busy context, the status bar, and the tab strip. */
  private runsChanged(): void {
    this.lanesChanged.fire();
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
      // Ollama that does not answer (not installed or not running) is left out, unless it is the chat provider.
      if (local && ref !== active && l?.error && !l.models.length) continue;
      // A list the endpoint answered is complete: the saved model shows only if it is really there.
      const complete = !!l && !l.error;
      const models = withSaved(l?.models ?? [], modelFor(ref), complete).slice(0, MAX_PICKER_MODELS).map(row);
      const note = [l?.note, complete ? missingNote(l.models, modelFor(ref), local) : undefined].filter(Boolean).join(' ') || undefined;
      groups.push({ kind: ref, brand: refKind(ref), label: refLabel(ref), local, models, loading: this.listing.has(ref), note, error: l?.models.length ? undefined : l?.error });
    }
    if (findClaude()) groups.push({ kind: CLAUDE_KIND, brand: CLAUDE_KIND, label: CLAUDE_LABEL, local: false, models: withSaved(CLAUDE_MODEL_INFO, claudeModel()).map(row), loading: false });
    // `brand` is the provider kind behind a ref (a custom endpoint is openaiCompatible), for the chip colour.
    const current = engine() === 'claudeCode'
      ? { kind: CLAUDE_KIND, brand: CLAUDE_KIND, model: claudeModel() }
      : { kind: active, brand: refKind(active), local: refKind(active) === 'ollama', model: modelFor(active), window: this.windowFor(active, modelFor(active)) };
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
      case 'openSubagent':
        if (typeof m.taskId === 'string') this.openSubagent(m.taskId);
        break;
      case 'pinTab':
        if (typeof m.id === 'string') this.pinTab(m.id);
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
        const pending = typeof m.callId === 'string' ? this.pendingIn((r) => r.questions.get(m.callId as string)) : undefined;
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
        // Each chat tab runs its own task. In a tab whose task runs, Enter steers it (text only); a
        // message with images, or one sent to be queued, waits for that task.
        const images = imagesFrom(m.images);
        const tab = this.chatTabId;
        const steer = this.activeTab === tab && this.runs.has(tab) && !images.length && m.queue !== true;
        const text = m.text;
        await (steer ? this.inTab(tab, () => this.steer(text)) : this.send(text, undefined, false, images, tab));
        break;
      }
      case 'cancel': {
        const tab = typeof m.tab === 'string' && this.tabs.some((t) => t.id === m.tab) ? m.tab : undefined;
        this.cancel(tab);
        if (!tab || tab === this.nightTab) await this.setNightRequest(undefined);
        break;
      }
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
      case 'slashAction':
        await this.slashAction(m);
        break;
      case 'listMcp':
        await this.postMcp();
        break;
      case 'restoreTo':
        if (typeof m.id === 'string') await this.restoreTo(m.id);
        break;
      case 'listTools':
        await this.postTools();
        break;
      case 'toggleTool': {
        if (typeof m.name !== 'string' || !/^[\w.-]{1,64}$/.test(m.name)) break;
        const off = new Set(windowChoice().offTools ?? []);
        if (m.on) off.delete(m.name);
        else off.add(m.name);
        await setWindowChoice({ offTools: [...off].sort() });
        await this.postTools();
        break;
      }
      case 'mcpAction':
        await this.mcpAction(m);
        break;
      case 'selectSandbox':
        await setSandbox(m.enabled === true);
        this.refreshStatus();
        break;
      case 'editWaiting':
        if (Number.isInteger(m.index) && typeof m.text === 'string') {
          const [kind, index, text] = [m.kind === 'steer' ? ('steer' as const) : ('queue' as const), m.index as number, m.text];
          await this.inTab(this.activeTab, () => this.editWaiting(kind, index, text));
        }
        break;
      case 'signIn':
        await claudeSignIn(() => this.refreshStatus());
        break;
      case 'signOut':
        await claudeSignOut(() => this.refreshStatus());
        break;
      case 'selectModel':
        // A running task keeps the model it started with, so only the tab on screen must be idle.
        if (this.runs.has(this.chatTabId) || typeof m.model !== 'string' || !m.model.trim() || m.model.length > 256) this.refreshStatus();
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
        const id = m.callId as string;
        // The card on screen belongs to the tab on screen; look there first, then in the other runs.
        const owner = [this.runs.get(this.activeTab), ...this.runs.values()].find((r) => r?.approvals.has(id));
        owner?.approvals.get(id)?.(d);
        owner?.approvals.delete(id);
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
        // Loop Mode's Think and Loop models, and the Refine and Helper models from the settings dialog.
        const model = typeof m.model === 'string' ? m.model.trim() : '';
        const kindOk = isProviderRef(m.kind) || (m.role === 'planner' && m.kind === CLAUDE_KIND);
        const key = ({ planner: 'nightPlanner', worker: 'nightWorker', refine: 'refineModel', helper: 'helperModel' } as const)[m.role as 'planner'];
        if (key && m.kind === '' && (m.role === 'refine' || m.role === 'helper')) await setWindowChoice({ [key]: undefined });
        else if (key && model && model.length <= 256 && kindOk) await setWindowChoice({ [key]: { kind: m.kind, model } });
        this.refreshStatus();
        break;
      }
      case 'selectEffort':
        // An unknown value (the slider's "Default" stop) clears the choice.
        await setWindowChoice({ effort: EFFORTS.includes(m.effort as Effort) ? (m.effort as Effort) : undefined });
        this.refreshStatus();
        break;
      case 'selectAgent':
        await setAgent(typeof m.name === 'string' ? m.name : undefined);
        this.refreshStatus();
        break;
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
      case 'svgPreview':
        if (typeof m.id === 'string' && typeof m.path === 'string') await this.svgPreview(m.id, m.path);
        break;
      case 'exportSvg':
        if (typeof m.path === 'string') await this.exportSvg(m.path);
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
        if (it?.stop && it.ok === undefined && this.runs.size) await (await this.getClient()).request('agent/cancelTool', it.stop).catch((e) => this.log('warn', `cancelTool: ${describeError(e)}`));
        break;
      }
      case 'stopLane': {
        const lane = this.lane(m.taskId);
        if (lane?.status === 'running') await (await this.getClient()).request('agent/cancelSubagent', { taskId: lane.taskId }).catch((e) => this.log('warn', `cancelSubagent: ${describeError(e)}`));
        break;
      }
      case 'steerLane': {
        // A message for one running subagent: it joins that subagent's conversation at its next step.
        const lane = this.lane(m.taskId);
        const text = typeof m.text === 'string' ? m.text.trim() : '';
        if (!lane || lane.status !== 'running' || lane.stoppable === false || !text) break;
        const ok = await (await this.getClient())
          .request<boolean>('agent/steer', { taskId: lane.taskId, text })
          .catch((e) => (this.log('warn', `steer subagent: ${describeError(e)}`), false));
        if (!ok) void vscode.window.showWarningMessage(`task-x: ${lane.agent || lane.name} is not taking messages right now. It may be finishing, or it has 10 unread messages.`);
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
    // Indexed docs sets come first in the @ menu, as @docs:<name>.
    const docs = await this.getClient().then(listDocs, () => [] as DocsSource[]);
    const docItems = docs.filter((d) => !q || `docs:${d.name}`.includes(q) || d.name.toLowerCase().includes(q)).map((d) => `docs:${d.name}`);
    this.post({ type: 'files', query, items: [...docItems, ...items].slice(0, MAX_FILE_ITEMS) });
  }

  private lane(taskId: string): Lane | undefined {
    return this.items.find((i): i is Lane => i.kind === 'lane' && i.taskId === taskId);
  }

  private updateLane(lane: Lane, patch: Partial<Lane>): void {
    Object.assign(lane, patch);
    this.postChat({ type: 'laneUpdate', item: lane });
    this.lanesChanged.fire();
  }

  /**
   * Records a subagent's activity next to its chat. Its tab stays off the strip until the lane card
   * opens it, so many subagents do not crowd the tabs.
   */
  private openSubagentTab(e: Extract<AgentEvent, { type: 'subagentStarted' }>): void {
    const parent = this.chat;
    const tab: ChatTab = { id: blankTab().id, kind: 'subagent', title: e.agent || e.name, role: e.role, parent: parent.id, laneTaskId: e.taskId, dir: e.dir, status: 'running', items: [{ kind: 'user', text: e.objective, mode: 'agent' }] };
    let at = this.tabs.indexOf(parent);
    while (this.tabs[at + 1]?.parent === parent.id) at++;
    this.tabs.splice(at + 1, 0, tab);
    const done = this.tabs.filter((t) => t.kind === 'subagent' && t.status !== 'running' && !t.shown);
    const extra = this.tabs.filter((t) => t.kind === 'subagent').length - MAX_SUBAGENT_TABS;
    for (const old of done.slice(0, Math.max(0, extra))) this.tabs.splice(this.tabs.indexOf(old), 1);
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
        // A subagent in a worktree drew its SVG there, not in the workspace: no preview until it merges.
        const out = { ...this.captureOutput(id, ev.tool, ev.ok, ev.output), tokens: ev.tokens, svg: tab.dir ? undefined : svgPathFrom(ev.tool, ev.ok, ev.output) };
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
      this.onPending?.(files);
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
  private captureOutput(id: string, tool: string, ok: boolean, raw: string): { output?: string; outputCut?: boolean; browser?: string } {
    const base = tool.split(' › ').pop() ?? tool;
    const browser = base.replace(/^mcp__task-x__/, '') === 'browser' ? this.browserUsed(raw, ok) : undefined;
    const where = browser ? { browser } : {};
    if (ok && !OUTPUT_TOOLS.has(base)) return where;
    const text = formatOutput(raw);
    this.outputs.set(id, text);
    return { ...where, output: text.slice(0, MAX_SAVED_OUTPUT), outputCut: text.length > MAX_SAVED_OUTPUT };
  }

  /** The browser a `browser` call ran in: the runtime names it in `browserName` (also when the result came over MCP). */
  private browserUsed(raw: string, ok: boolean): string | undefined {
    const named = /\\?"browserName\\?"\s*:\s*\\?"([^"\\]{1,40})\\?"/.exec(raw)?.[1];
    if (named) this.lastBrowser = named;
    // A screenshot's image can push the name past the event's cut-off; it ran where the calls before it did.
    return named ?? (ok ? this.lastBrowser : undefined);
  }

  /** Only an SVG that a `render_svg` call in an open chat drew; never just any path from the webview. */
  private knownSvg(rel: string): boolean {
    return isWorkspaceSvg(rel) && this.allItems().some((i) => i.kind === 'tool' && i.svg === rel);
  }

  /** Renders the SVG as it is now (through the runtime, so it matches what the model saw) for the chat preview. */
  private async svgPreview(id: string, rel: string): Promise<void> {
    if (!this.knownSvg(rel)) return;
    try {
      const client = await this.getClient();
      const r = await client.request<{ base64?: string; warnings?: string[] }>('tools/call', { name: 'render_svg', input: { path: rel, size: PREVIEW_SIZE, background: 'transparent' } });
      this.post({ type: 'svgImage', id, path: rel, data: r.base64 ?? null, warnings: r.warnings ?? [] });
    } catch (e) {
      this.post({ type: 'svgImage', id, path: rel, data: null, error: describeError(e) });
    }
  }

  /** Exports the SVG as PNG files at the sizes the user picks; the files join the review list and can be undone. */
  private async exportSvg(rel: string): Promise<void> {
    if (!this.knownSvg(rel)) return;
    const picks = await vscode.window.showQuickPick(
      EXPORT_SIZES.map((s) => ({ label: `${s.size} px`, description: s.label, picked: s.picked, size: s.size })),
      { canPickMany: true, title: `Export ${rel.split('/').pop()} as PNG`, placeHolder: 'Pick the sizes; each becomes <name>-<size>.png next to the SVG' },
    );
    if (!picks?.length) return;
    const sizes = picks.map((p) => p.size);
    try {
      const client = await this.getClient();
      const run = (overwrite: boolean) => client.request<unknown>('tools/call', { name: 'export_png', input: { path: rel, sizes, overwrite } });
      let r: unknown;
      try {
        r = await run(false);
      } catch (e) {
        const exists = /^(.+?) already exists/.exec(describeError(e));
        if (!exists) throw e;
        const ok = await vscode.window.showWarningMessage(`${exists[1].split('/').pop()} already exists. Replace the existing PNG files?`, { modal: true }, 'Replace');
        if (ok !== 'Replace') return;
        r = await run(true);
      }
      const files = exportedFiles(r);
      void this.refreshChanges();
      const root = WorkspaceSync.root();
      const pick = await vscode.window.showInformationMessage(`task-x: exported ${files.map((f) => f.split('/').pop()).join(', ')}`, 'Reveal in Explorer');
      if (pick && root && files[0]) await vscode.commands.executeCommand('revealInExplorer', vscode.Uri.joinPath(vscode.Uri.file(root), ...files[0].split('/')));
    } catch (e) {
      vscode.window.showErrorMessage(`task-x: PNG export failed: ${describeError(e)}`);
    }
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
    // Requests from the runtime name their task; each goes to the tab whose run owns that task.
    client.onRequest('agent/requestApproval', (p) => {
      const req = p as ApprovalRequest;
      if (editMode() === 'auto') return Promise.resolve({ decision: 'allowSession' as const });
      const tab = this.tabForTask(req.taskId);
      return tab ? this.inTab(tab, () => this.requestApproval(req)) : (this.foreignApproval?.(req) ?? Promise.resolve({ decision: 'deny' as const }));
    });
    client.onRequest('agent/askUser', (p) => {
      const req = p as AskUserRequest;
      const tab = this.tabForTask(req.taskId);
      return tab ? this.inTab(tab, () => this.askUser(req)) : Promise.resolve<AskUserResult>({ answers: null });
    });
    // #15: an MCP server asks the user something while it works.
    client.onRequest('mcp/elicit', (p) => elicit(p));
    setResourceReader(async (server, uri) => (await client.request<{ text: string }>('mcp/readResource', { server, uri })).text);
  }

  /**
   * Screens shared with the command palette, for either engine and while a task runs: `/approvals`,
   * `/timeline`, `/restore <step>`, `/export` (HTML), and `/agents-active`. Returns false for other text.
   */
  private screenCommand(text: string): boolean {
    const s = /^\/(approvals|timeline|restore|export|agents-active)(?:\s+(\d+))?\s*$/.exec(text);
    if (!s) return false;
    const [, cmd, arg] = s;
    const client = () => this.getClient();
    if (cmd === 'approvals') void manageApprovals(client);
    else if (cmd === 'agents-active') void showActiveAgents(client, (id) => this.openTask(id));
    else if (!this.taskId && cmd !== 'timeline') this.add({ kind: 'context', text: `/${cmd}: there is no conversation yet in this chat.` });
    else if (cmd === 'export') void exportHtml(client, this.taskId!);
    else if (cmd === 'restore' && arg == null) this.add({ kind: 'context', text: '/restore: give the step, as in /restore 2. /timeline lists them.' });
    else void showTimeline(client, this.taskId, cmd === 'restore' ? Number(arg) : undefined);
    return true;
  }

  /**
   * The session commands of the task-x engine: `/cost`, `/compact`, `/checkpoints` and `/rewind [n]`
   * (cut the conversation back to before a prompt and undo its edits), and `/fork [n]`. Returns false
   * for any other text, which goes to the agent.
   */
  private async sessionCommand(text: string): Promise<boolean> {
    const m = /^\/(cost|compact|checkpoints|rewind|fork)(?:\s+(\d+))?\s*$/.exec(text);
    if (!m) return false;
    const [, cmd, arg] = m;
    const taskId = this.taskId;
    if (!taskId) {
      this.add({ kind: 'context', text: `/${cmd}: there is no conversation yet in this chat.` });
      return true;
    }
    const client = await this.getClient();
    try {
      if (cmd === 'cost') {
        const c = await client.request<{ usage: { inputTokens: number | null; outputTokens: number | null; cachedTokens?: number }; costUsd: number; unpricedTokens: number; modelCalls: number; toolCalls: number; prompts: number; subagents: number; subagentCostUsd: number }>('agent/cost', { taskId });
        const u = c.usage;
        const money = c.costUsd > 0 ? `about $${c.costUsd.toFixed(4)}` : 'no price known';
        this.add({
          kind: 'context',
          text: `Cost of this conversation: ${money}. ${fmtTokens(u.inputTokens ?? 0)} input tokens (${fmtTokens(u.cachedTokens ?? 0)} cached), ${fmtTokens(u.outputTokens ?? 0)} output tokens, ${c.modelCalls} model calls, ${c.toolCalls} tool calls over ${c.prompts} prompts${c.subagents ? `, ${c.subagents} subagents` : ''}.`,
        });
        return true;
      }
      if (cmd === 'compact') {
        const r = await client.request<{ beforeTokens: number; afterTokens: number }>('agent/compact', { taskId });
        this.add({ kind: 'context', text: `Compressed the conversation: about ${fmtTokens(r.beforeTokens)} tokens before, ${fmtTokens(r.afterTokens)} after.` });
        return true;
      }
      const points = await client.request<{ index: number; prompt: string }[]>('agent/checkpoints', { taskId });
      if (!points.length) {
        this.add({ kind: 'context', text: 'This conversation has no checkpoints yet.' });
        return true;
      }
      let at = arg != null ? Number(arg) : undefined;
      if (at == null) {
        const items = points.map((p) => ({ label: `${p.index}`, description: p.prompt.split('\n')[0], index: p.index }));
        const title = cmd === 'fork' ? 'Fork before which prompt? (Esc: fork the whole conversation)' : cmd === 'rewind' ? 'Rewind to before which prompt?' : 'Prompts of this conversation';
        const pick = await vscode.window.showQuickPick(items, { title: `task-x: ${title}` });
        if (cmd === 'checkpoints') return true;
        at = pick?.index;
        if (at == null && cmd === 'rewind') return true;
      }
      if (cmd === 'checkpoints') return true;
      if (cmd === 'rewind') {
        const ok = await vscode.window.showWarningMessage(`Rewind to before prompt ${at}? Later messages are removed and their edits are undone (files you changed since are left as they are).`, { modal: true }, 'Rewind');
        if (ok !== 'Rewind') return true;
        const r = await client.request<{ undone: number[]; conflicts: unknown[] }>('agent/rewind', { taskId, checkpoint: at });
        await this.openTask(taskId);
        this.add({ kind: 'context', text: `Rewound to before prompt ${at}: undid ${r.undone.length} edit${r.undone.length === 1 ? '' : 's'}${r.conflicts.length ? `, ${r.conflicts.length} left as they are` : ''}.` });
        return true;
      }
      const f = await client.request<{ taskId: string; title: string }>('agent/fork', { taskId, ...(at != null ? { checkpoint: at } : {}) });
      await this.openTask(f.taskId);
      this.add({ kind: 'context', text: `Forked into a new conversation (${f.title}). The original stays in the history.` });
    } catch (e) {
      this.add({ kind: 'context', text: `/${cmd} failed: ${describeError(e)}` });
    }
    return true;
  }

  /** The first value `f` finds in a run: the tab on screen first, then the others. */
  private pendingIn<T>(f: (r: Run) => T | undefined): T | undefined {
    for (const r of [this.runs.get(this.activeTab), ...this.runs.values()]) {
      const v = r && f(r);
      if (v !== undefined) return v;
    }
    return undefined;
  }

  /** Shows the agent's questions in the chat and resolves with the answers, or null when dismissed. */
  private askUser(req: AskUserRequest): Promise<AskUserResult> {
    const run = this.run;
    if (!this.taskId || !req.taskId.startsWith(this.taskId) || !run) return Promise.resolve<AskUserResult>({ answers: null });
    this.view?.show?.(true);
    this.notify('input', 'task-x has questions for you', 'Answer');
    if (this.activeTab !== this.chat.id) this.showTab(this.chat.id);
    // Plan steps run in parallel and can reuse a call id; the task id keeps each question card apart.
    const id = `${req.taskId}:${req.callId}`;
    run.questions.get(id)?.done({ answers: null });
    const card = { type: 'questions', callId: id, questions: req.questions };
    run.cards.set(id, card);
    this.postChat(card);
    return new Promise((resolve) => {
      run.questions.set(id, {
        questions: req.questions,
        done: (res) => {
          run.questions.delete(id);
          run.cards.delete(id);
          this.postChat({ type: 'questionsDone', callId: id });
          this.add({ kind: 'questions', questions: req.questions, answers: res.answers, other: res.other });
          resolve(res);
        },
      });
    });
  }

  private requestApproval(req: ApprovalRequest): Promise<{ decision: ApprovalDecision }> {
    const run = this.run;
    if (!this.taskId || !req.taskId.startsWith(this.taskId) || !run) return this.foreignApproval?.(req) ?? Promise.resolve({ decision: 'deny' });
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
    run.cards.set(req.callId, card);
    this.postChat(card);
    return new Promise((resolve) => {
      run.approvals.set(req.callId, (decision) => {
        run.cards.delete(req.callId);
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
      case 'subagentStarted': return this.log('info', `${prefix}subagent ${e.name} started (${e.agent ? `agent ${e.agent}` : e.role}, ${e.kind}/${e.model})`);
      case 'subagentFinished': return this.log(e.status === 'completed' ? 'info' : 'warn', `${prefix}subagent ${e.name} ${e.status}`);
      case 'planStarted': return this.log('info', `${prefix}plan with ${e.steps.length} steps, up to ${e.maxParallel} in parallel`);
      case 'merged': return this.log(e.ok ? 'info' : 'warn', `${prefix}merge ${e.name}: ${e.ok ? 'ok' : e.error}`);
      case 'timelineSnapshot': return this.log('info', `${prefix}timeline step ${e.snapshot.step}: ${e.snapshot.files.length} files`);
      case 'wakeupScheduled': return this.log('info', `${prefix}${e.wakeup.kind} ${e.wakeup.id} scheduled`);
      case 'wakeupFired': return this.log('info', `${prefix}${e.kind} ${e.id} fired`);
      case 'wakeupCancelled': return this.log('info', `${prefix}wake-up ${e.id} cancelled (${e.by})`);
      case 'sleeping': return this.log('info', `${prefix}sleeping, ${e.pending.length} pending`);
      case 'awake': return this.log('info', `${prefix}awake (${e.by}) after ${Math.round(e.sleptMs / 1000)} s`);
      case 'done': {
        const s = e.summary;
        return this.log(s.status === 'completed' ? 'info' : 'warn', `${prefix}task ${s.status}: ${s.iterations} turns, ${s.toolCalls} tool calls, ${s.changedFiles.length} files${s.error ? ` — ${s.error}` : ''}`);
      }
    }
  }

  private onEvent(e: AgentEvent, state: { assistant?: Extract<Item, { kind: 'assistant' }> }): void {
    const run = this.run;
    if (run) run.lastActivity = Date.now();
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
        // Text after any other row (a note, a background result) starts a new reply, so the screen
        // and the saved chat both keep it; appending to the older reply would hide it on screen.
        if (!state.assistant || this.items[this.items.length - 1] !== state.assistant) {
          state.assistant = { kind: 'assistant', text: '' };
          this.add(state.assistant);
        }
        state.assistant.text += e.text;
        this.postChat({ type: 'delta', text: e.text });
        break;
      case 'assistantMessage': {
        // The whole text of the model turn. When it did not arrive as deltas (a provider that sends
        // it at the end), or only part of it did, the reply shows the full text now.
        const full = typeof e.text === 'string' ? e.text : '';
        if (!full.trim()) break;
        const shown = state.assistant && this.items.includes(state.assistant) ? state.assistant : undefined;
        if (!shown) {
          state.assistant = { kind: 'assistant', text: full };
          this.add(state.assistant);
        } else if (shown.text !== full && full.startsWith(shown.text)) {
          const rest = full.slice(shown.text.length);
          shown.text = full;
          if (this.items[this.items.length - 1] === shown) this.postChat({ type: 'delta', text: rest });
          else this.renderChat();
        } else if (!shown.text.trim()) {
          shown.text = full;
          this.renderChat();
        }
        break;
      }
      case 'retry':
        // Older runtimes send no `discardText`: they always restarted the reply.
        if (state.assistant && e.discardText !== false) {
          this.items = this.items.filter((i) => i !== state.assistant);
          state.assistant = undefined;
          this.renderChat();
        }
        this.add({ kind: 'context', text: `Retrying (${e.attempt}) after: ${e.reason}` });
        break;
      case 'toolStarted': {
        state.assistant = undefined;
        // #7: the plan checklist (task-x `todo_write`, Claude Code `TodoWrite`) shows above the input, not as a row.
        const todos = todoList(e.tool, e.input);
        if (todos) {
          this.todos = todos;
          this.postTodos();
          break;
        }
        this.add({ kind: 'tool', id: e.callId, tool: e.tool, detail: toolDetail(e.tool, e.input), stop: this.stoppable(e.permission, this.taskId, e.callId, e.tool) });
        break;
      }
      case 'toolFinished': {
        state.assistant = undefined;
        if (TODO_TOOL.test(e.tool)) break;
        // A background command answers twice: at once ("runs in the background"), then with its output. The row stays running until then.
        if (!e.background && isBackgroundStart(e.output)) break;
        const it = this.items.find((i): i is Extract<Item, { kind: 'tool' }> => i.kind === 'tool' && i.id === e.callId);
        // `tokens`: about how much this result adds to every later turn.
        const out = { ...this.captureOutput(e.callId, e.tool, e.ok, e.output), tokens: e.tokens, svg: svgPathFrom(e.tool, e.ok, e.output) };
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
        this.postChat({ type: 'usage', input: e.usage.inputTokens ?? 0, output: e.usage.outputTokens ?? 0, cached: e.usage.cachedTokens ?? 0 });
        // The task's total so far; it stays with the tab for the counter above the input.
        this.chat.tokens = { input: e.usage.inputTokens ?? 0, output: e.usage.outputTokens ?? 0, cached: e.usage.cachedTokens ?? 0 };
        this.postChat({ type: 'tokens', ...this.chat.tokens });
        break;
      case 'steered': {
        const i = run?.steering.indexOf(e.text) ?? -1;
        if (i >= 0) run?.steering.splice(i, 1);
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
        this.add({ kind: 'lane', taskId: e.taskId, name: e.name, role: e.role, agent: e.agent ?? null, objective: e.objective, model: e.model, step: e.step, status: 'running', steps: [], dir: e.dir, stoppable: e.kind !== 'claudeCode', startedMs: Date.now() });
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
            endedMs: Date.now(),
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
          } else if (inner.type === 'steered') {
            // The subagent read the user's message: it shows among its steps.
            lane.steps.push({ id: `steer-${Date.now()}-${lane.steps.length}`, tool: 'user_message', detail: inner.text, ok: true });
            if (lane.steps.length > MAX_LANE_STEPS) lane.hidden = (lane.hidden ?? 0) + lane.steps.splice(0, lane.steps.length - MAX_LANE_STEPS).length;
            this.updateLane(lane, {});
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
      case 'wakeupScheduled':
        state.assistant = undefined;
        this.add({ kind: 'context', text: `${e.wakeup.kind === 'watch' ? 'Watching' : 'Wake-up'} at ${new Date(e.wakeup.dueMs).toLocaleTimeString()}${e.wakeup.reason ? `: ${e.wakeup.reason}` : ''}` });
        break;
      case 'wakeupCancelled':
        if (e.by === 'model') this.add({ kind: 'context', text: `Cancelled wake-up ${e.id}` });
        break;
      case 'sleeping': {
        state.assistant = undefined;
        const next = e.pending[0];
        this.add({ kind: 'context', text: `🌙 Sleeping until ${new Date(e.untilMs).toLocaleTimeString()}${next?.reason ? `: ${next.reason}` : ''} (${e.pending.length} pending). Send a message to wake it now; Stop ends it.` });
        break;
      }
      case 'awake':
        state.assistant = undefined;
        if (e.by !== 'cancelled') this.add({ kind: 'context', text: `☀️ Awake after ${fmtDuration(e.sleptMs)} (${e.by === 'wakeup' ? 'a wake-up fired' : e.by === 'message' ? 'your message' : 'nothing left to wait for'})` });
        break;
    }
  }

  /**
   * Rewrites the typed prompt with the Refine model picked in the settings dialog, else the routed model
   * (`task-x.routing` for the prompt's task kind), else the active provider model. The webview puts the
   * result in the input, where Ctrl+Z restores the original.
   */
  private async refine(raw: string): Promise<void> {
    const text = raw.trim();
    const fail = (error: string) => this.post({ type: 'refined', error });
    if (!text) return fail('Type a prompt to refine.');
    if (text.length > MAX_INPUT_CHARS) return fail(`The prompt is longer than ${MAX_INPUT_CHARS} characters.`);
    try {
      const client = await this.getClient();
      const picked = toolModels().refine;
      const own = picked && isProviderRef(picked.kind) ? await providerConfigWith(this.ctx.secrets, picked.kind, picked.model).catch(() => undefined) : undefined;
      const provider = own ?? (await providerConfig(this.ctx.secrets).catch(() => undefined));
      const ed = this.editorContext(text);
      const focusPaths = [...(ed.mentions ?? []), ...(ed.activeFile ? [ed.activeFile] : [])];
      const r = await client.request<RefineResult>('prompt/refine', { prompt: text, provider, routes: own ? [] : await routes(this.ctx.secrets), focusPaths });
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
    // The chat on screen is busy: the request gets its own tab and runs beside it.
    if (this.runs.has(this.chatTabId)) {
      if (this.tabs.filter((t) => t.kind === 'chat').length >= MAX_CHAT_TABS) return void vscode.window.showWarningMessage('task-x: this chat is busy and no tab is free. Stop its task, or close a tab.');
      this.showTab(this.addTab(blankTab()).id);
    }
    await this.send(text, mode);
  }

  /** Shows an earlier task: in its open tab, else in the current tab when it is empty, else in a new tab. */
  async openTask(taskId: string): Promise<void> {
    const open = this.tabs.find((t) => t.kind === 'chat' && t.taskId === taskId);
    const cur = this.tabs.find((t) => t.id === this.chatTabId);
    const reuse = !open && cur && !cur.items.length && !cur.taskId && !this.runs.has(cur.id) ? cur : undefined;
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

  /**
   * Runs a request in a chat tab (`tabId`, else the current chat). Each tab runs its own task, beside
   * the others; a request to a tab whose task runs waits in that tab's queue.
   */
  private send(raw: string, modeOverride?: ChatMode, retry = false, images: ImageInput[] = [], tabId?: string): Promise<void> {
    const tab = tabId && this.tabs.some((t) => t.id === tabId && t.kind === 'chat') ? tabId : this.chat.id;
    return this.inTab(tab, () => this.sendIn(raw, modeOverride, retry, images));
  }

  /** `send` inside the tab's scope: `chat`, `items`, and `taskId` are that tab's for the whole run. */
  private async sendIn(raw: string, modeOverride: ChatMode | undefined, retry: boolean, images: ImageInput[]): Promise<void> {
    const text = raw.trim() || (images.length ? `Look at the attached image${images.length > 1 ? 's' : ''}.` : '');
    if (!text) return;
    const tab = this.chat.id;
    if (text.length > MAX_INPUT_CHARS) return void vscode.window.showWarningMessage(`task-x: the message is longer than ${MAX_INPUT_CHARS} characters.`);
    if (!images.length && this.screenCommand(text)) return;
    // This tab's task runs, or every run slot is taken: the request waits its turn.
    if (this.runs.has(tab) || this.runs.size >= MAX_PARALLEL_RUNS) {
      if (this.queue.length >= MAX_QUEUE) return void vscode.window.showWarningMessage(`task-x: only ${MAX_QUEUE} messages can wait. Stop a task or wait for one to finish.`);
      this.queue.push({ text, mode: modeOverride, images, tab });
      if (!this.runs.has(tab)) void vscode.window.showInformationMessage(`task-x: ${MAX_PARALLEL_RUNS} chats are running. This request starts when one finishes.`);
      return void this.postQueue();
    }
    // `/clear` (or `/new`): a fresh conversation in this chat, for either engine.
    if (!images.length && /^\/(clear|new)\s*$/i.test(text)) return this.clear();
    if (!images.length && engine() !== 'claudeCode' && (await this.sessionCommand(text))) return;
    // A chat idle for `session.freshAfterIdleMinutes` starts a new conversation: an old history is
    // sent again with every turn, and a new topic seldom needs it. The old one stays in History.
    const freshMin = sessionLimits().freshMin;
    const cur = this.chat;
    if (!retry && !this.night && freshMin && cur.taskId && cur.lastUsed && Date.now() - cur.lastUsed >= freshMin * 60_000 && !this.runs.has(tab)) {
      const idle = Date.now() - cur.lastUsed;
      const old = { ...cur, items: [...cur.items] };
      Object.assign(cur, { items: [], taskId: undefined, usage: undefined, tokens: undefined, todos: [], lastUsed: undefined });
      this.add({ kind: 'context', text: `New conversation: this chat was idle for ${fmtIdle(idle)}, so task-x started fresh to send fewer tokens (task-x.session.freshAfterIdleMinutes). The earlier conversation is in History.` });
      if (cur.id === this.activeTab) this.renderTab();
      await this.archive(old);
      if (this.runs.has(tab)) return this.sendIn(raw, modeOverride, retry, images);
    }
    // The run is registered before anything waits, so a second request to this tab queues behind it.
    const abort = new AbortController();
    const run: Run = { abort, started: Date.now(), cards: new Map(), approvals: new Map(), questions: new Map(), steering: [], stalled: false, lastActivity: Date.now() };
    this.runs.set(tab, run);
    if (this.night) this.nightTab = tab;
    this.runsChanged();
    const mode: ChatMode = modeOverride ?? 'agent';
    // A new request starts without the old plan.
    this.todos = [];
    this.postTodos();
    const sent = sentContext(this.editorContext(text));
    const request: Extract<Item, { kind: 'user' }> = { kind: 'user', text, mode, id: randomBytes(6).toString('hex'), ...(images.length ? { images: images.length } : {}), ...(sent.length ? { context: sent } : {}) };
    this.add(request);
    run.lastRequest = request;
    const before = this.lastSummary();
    // Loop Mode follows the tab of the newest request.
    if (!retry && (this.night || !this.nightTab)) {
      this.nightFailures = 0;
      clearTimeout(this.nightTimer);
      this.nightTimer = undefined;
      if (this.night) this.nightSince = Date.now();
      void this.setNightRequest(text);
    }
    let off = () => {};
    // Night mode with a loop model runs through task-x with that model, whatever the chat engine is.
    const worker = this.night ? nightModels().worker : undefined;
    // #9 `#` items and #12 the chat mode: the model gets their context; the message shows the user's words.
    // MCP resources (`#mcp:`) are read through the runtime, so it is connected first.
    await this.getClient().then((c) => this.wire(c), () => undefined);
    const claudeRun = engine() === 'claudeCode' && !worker;
    // The run is registered, so nothing may throw before the `try` below: a throw here would leave
    // the tab marked as running and every later message waiting behind it.
    let full = text;
    try {
      full = await withHashContext(withDocs(withChatMode(withAgent(text, claudeRun))));
    } catch (e) {
      this.log('warn', `context for the request failed, sending it without: ${describeError(e)}`);
    }
    if (claudeRun) {
      try {
        const task = this.night ? await this.nightTask(full, retry, abort.signal) : full;
        await this.sendClaude(text, mode, abort.signal, (o) => (off = o), task, images);
      } catch (e) {
        this.add({ kind: 'error', text: describeError(e), retryText: e instanceof RpcError && e.cancelled ? undefined : text });
      } finally {
        off();
        this.queue.push(...run.steering.splice(0).map((text) => ({ text, tab })));
        this.runs.delete(tab);
        this.markUsed(tab);
        this.runsChanged();
        await this.persistAfterRun();
        this.drain();
        this.afterRun(before, abort.signal, run.stalled);
      }
      return;
    }
    try {
      const client = await this.getClient();
      this.wire(client);
      this.taskId ??= `c${Date.now()}${randomBytes(3).toString('hex')}`;
      const taskId = this.taskId;
      const prompt = this.night ? await this.nightTask(full, retry, abort.signal) : full;
      const provider = worker && isProviderRef(worker.kind) ? await providerConfigWith(this.ctx.secrets, worker.kind, worker.model) : undefined;
      const params = await runParams(this.ctx.secrets, { prompt, mode, taskId, editor: this.editorContext(text), provider, images });
      const state: { assistant?: Extract<Item, { kind: 'assistant' }> } = {};
      // Bound to this run's tab: the event arrives outside the run's async context.
      off = client.onNotification(
        'agent/event',
        AsyncResource.bind((p: unknown) => {
          const { taskId: id, event } = p as AgentEventParams;
          if (id === taskId) this.onEvent(event, state);
        }),
      );
      let summary = await client.request<AgentSummary>('agent/run', params, { signal: abort.signal });
      // At the turn limit, the same task continues on its own a few times (task-x.agent.autoContinue).
      const auto = Math.min(5, Math.max(0, Math.floor(vscode.workspace.getConfiguration(SECTION).get<number>('agent.autoContinue', 2))));
      for (let n = 1; n <= auto && summary.status === 'maxIterations' && !abort.signal.aborted; n++) {
        this.add({ kind: 'context', text: `Reached the turn limit after ${summary.iterations} turns. Continuing automatically (${n} of ${auto}).` });
        state.assistant = undefined;
        summary = await client.request<AgentSummary>('agent/run', { ...params, prompt: CONTINUE_PROMPT, images: [] }, { signal: abort.signal });
      }
      this.add({ kind: 'summary', ...summaryText(summary) });
      this.noteOperations(summary);
      await this.commitTask(summary, text);
      this.finished(summary, text);
      void this.refreshChanges();
    } catch (e) {
      this.add({ kind: 'error', text: describeError(e), retryText: e instanceof RpcError && e.cancelled ? undefined : text });
    } finally {
      off();
      for (const it of this.items) if (it.kind === 'lane' && it.status === 'running') this.updateLane(it, { status: 'cancelled', endedMs: Date.now() });
      for (const t of this.tabs) {
        if (t.parent !== tab || t.status !== 'running') continue;
        t.status = 'cancelled';
        t.assistant = undefined;
      }
      this.closePreviews(tab);
      run.approvals.clear();
      run.cards.clear();
      // Steers the task never read are sent again as the next message, so nothing is lost.
      this.queue.push(...run.steering.splice(0).map((text) => ({ text, tab })));
      this.runs.delete(tab);
      this.markUsed(tab);
      this.runsChanged();
      await this.persistAfterRun();
      this.drain();
      this.afterRun(before, abort.signal, run.stalled);
    }
  }

  /** Saves after a run. A failed save is logged, not thrown: the waiting messages must still start. */
  private async persistAfterRun(): Promise<void> {
    try {
      await this.persist();
    } catch (e) {
      this.log('warn', `saving the chat failed: ${describeError(e)}`);
    }
  }

  private markUsed(tab: string): void {
    const t = this.tabs.find((x) => x.id === tab);
    if (t) t.lastUsed = Date.now();
  }

  /** Remembers which edit operations a request made; its message then offers "Restore to here". */
  private noteOperations(summary: AgentSummary): void {
    const r = this.run?.lastRequest;
    if (!r || !summary.operations?.length) return;
    r.operations = [...new Set([...(r.operations ?? []), ...summary.operations])];
    this.postChat({ type: 'restorable', id: r.id });
  }

  /**
   * Undoes the edits of one request and of every later request in the chat, newest first, so the
   * files are as they were before that request. Edits the user made since stay, with a note.
   */
  private async restoreTo(id: string): Promise<void> {
    // The message can be in any chat tab, not only the one a task runs in.
    const items = this.tabs.find((t) => t.items.some((i) => i.kind === 'user' && i.id === id))?.items ?? [];
    const at = items.findIndex((i) => i.kind === 'user' && i.id === id);
    if (at < 0) return;
    const ops = items
      .slice(at)
      .flatMap((i) => (i.kind === 'user' ? (i.operations ?? []) : []))
      .filter((op, k, all) => all.indexOf(op) === k)
      .sort((a, b) => b - a);
    if (!ops.length) return void vscode.window.showInformationMessage('task-x: this request and the ones after it made no edits.');
    const pick = await vscode.window.showWarningMessage(`Undo the edits of this request and every later one (${ops.length} edit${ops.length > 1 ? 's' : ''})?`, { modal: true }, 'Restore');
    if (pick !== 'Restore') return;
    const client = await this.getClient();
    const restored = new Set<string>();
    const kept: string[] = [];
    for (const op of ops) {
      try {
        const r = await client.request<{ restored: string[]; conflicts: { path: string }[] }>('edits/undo', { operation: op });
        for (const p of r.restored) restored.add(p);
        kept.push(...r.conflicts.map((c) => c.path));
        for (const it of this.allItems()) if (it.kind === 'file' && it.operation === op && r.restored.includes(it.path)) it.undone = true;
      } catch {
        // Already undone or no longer in the edit log.
      }
    }
    for (const i of items.slice(at)) if (i.kind === 'user') i.operations = undefined;
    await this.persist();
    this.renderTab();
    void this.refreshChanges();
    const msg = `Restored ${restored.size} file${restored.size === 1 ? '' : 's'} to before this request.${kept.length ? ` Kept ${[...new Set(kept)].join(', ')}: changed after the agent's edit.` : ''}`;
    (kept.length ? vscode.window.showWarningMessage : vscode.window.showInformationMessage)(msg);
  }

  /** The plan checklist of this chat's newest task. */
  private get todos(): Todo[] {
    return this.chat.todos ?? [];
  }
  private set todos(v: Todo[]) {
    this.chat.todos = v;
  }
  /** Shows the checklist of the chat on screen. */
  private postTodos(): void {
    const tab = this.tabs.find((t) => t.id === this.activeTab);
    this.post({ type: 'todos', todos: (tab?.kind === 'chat' ? tab.todos : this.tabs.find((t) => t.id === tab?.parent)?.todos) ?? [] });
  }

  /** Shows the messages waiting in the tab on screen: steers of its running task first, then its queued messages. */
  private postQueue(): void {
    const tab = this.activeTab;
    this.post({ type: 'queue', items: this.queue.filter((q) => q.tab === tab).map((q) => q.text), steering: this.runs.get(tab)?.steering ?? [] });
  }

  /** While a task runs, Enter steers it: the agent reads the message at its next step. Otherwise it is queued. */
  private async steer(text: string): Promise<void> {
    const t = text.trim();
    const run = this.run;
    if (!t || !this.taskId || !run || run.steering.length >= MAX_STEERS) return this.send(text);
    let accepted = false;
    try {
      accepted = await (await this.getClient()).request<boolean>('agent/steer', { taskId: this.taskId, text: t });
    } catch (e) {
      this.log('warn', `steer failed: ${describeError(e)}`);
    }
    if (!accepted) return this.send(text);
    run.steering.push(t);
    this.postQueue();
  }

  /**
   * Changes a message that still waits: a steer the agent has not read yet, or a queued message.
   * Empty text removes it. A steer the agent read in the meantime stays as it was, with a note.
   */
  private async editWaiting(kind: 'steer' | 'queue', index: number, raw: string): Promise<void> {
    const text = raw.trim().slice(0, MAX_INPUT_CHARS);
    if (kind === 'queue') {
      const mine = this.queue.filter((q) => q.tab === this.activeTab);
      const item = mine[index];
      if (item) {
        if (text) item.text = text;
        else this.queue.splice(this.queue.indexOf(item), 1);
      }
      return this.postQueue();
    }
    const steering = this.run?.steering ?? [];
    const old = steering[index];
    if (old === undefined || !this.taskId) return this.postQueue();
    let ok = false;
    try {
      ok = await (await this.getClient()).request<boolean>('agent/editSteer', { taskId: this.taskId, old, text });
    } catch (e) {
      this.log('warn', `editSteer failed: ${describeError(e)}`);
    }
    const at = steering.indexOf(old);
    if (ok && at >= 0) {
      if (text) steering[at] = text;
      else steering.splice(at, 1);
    } else if (!ok) vscode.window.showInformationMessage('task-x: the agent already read that message, so it can no longer change.');
    this.postQueue();
  }

  /**
   * Starts waiting messages once a run ends: the next one of each tab whose task is not running, oldest
   * first, up to the free run slots. `send` registers a run before it waits, so one tab never starts twice.
   */
  private drain(): void {
    for (const next of [...this.queue]) {
      if (this.runs.size >= MAX_PARALLEL_RUNS) break;
      if (this.runs.has(next.tab) || !this.tabs.some((t) => t.id === next.tab)) continue;
      this.queue.splice(this.queue.indexOf(next), 1);
      void this.send(next.text, next.mode, false, next.images, next.tab);
    }
    // Messages for a tab that was closed in the meantime are dropped.
    for (let i = this.queue.length - 1; i >= 0; i--) if (!this.tabs.some((t) => t.id === this.queue[i].tab)) this.queue.splice(i, 1);
    this.postQueue();
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
    if (when === 'off') return;
    const show = () => {
      const fn = kind === 'done' ? vscode.window.showInformationMessage : vscode.window.showWarningMessage;
      void fn(text, action).then((pick) => pick && vscode.commands.executeCommand(`${ChatViewProvider.viewId}.focus`));
    };
    if (when === 'always' || !vscode.window.state.focused || !this.view?.visible) show();
  }

  /** Files the `/` menu may open, by the key it sent to the webview; the webview never sends a path. */
  private slashFiles = new Map<string, string>();
  private mcpPrompts: { server: string; name: string; description: string; arguments: string[] }[] = [];
  private slashPlugins = new Map<string, PluginEntry & { source?: string; name?: string }>();

  /** Skills, commands, agents, and plugins for the `/` menu in the input. */
  private async postSkills(): Promise<void> {
    this.slashFiles.clear();
    this.slashPlugins.clear();
    const file = (p: string | undefined) => {
      if (!p || !path.isAbsolute(p)) return undefined;
      const key = String(this.slashFiles.size);
      this.slashFiles.set(key, p);
      return key;
    };
    const client = await this.getClient();
    const ask = async <T>(method: string, fallback: T): Promise<T> => {
      try {
        return await client.request<T>(method);
      } catch (e) {
        this.log('warn', `${method} failed: ${describeError(e)}`);
        return fallback;
      }
    };
    const [skills, agentList, pluginList] = await Promise.all([ask<SkillInfo[]>('skills/list', []), ask<AgentInfo[] | null>('agents/list', null), ask<PluginInfo[] | null>('plugins/list', null)]);
    const cat = await catalog(WorkspaceSync.root());
    const known = new Set(skills.map((s) => s.name));
    // #15: prompt templates and resources from the running MCP servers.
    let mcp: { prompts: { server: string; name: string; description: string; arguments: string[] }[]; resources: { server: string; uri: string; name: string; description: string }[] } = { prompts: [], resources: [] };
    try {
      mcp = await (await this.getClient()).request('mcp/catalog');
    } catch (e) {
      this.log('warn', `mcp/catalog failed: ${describeError(e)}`);
    }
    this.mcpPrompts = mcp.prompts;
    this.post({
      type: 'skills',
      skills: [
        ...skills
          .filter((s) => s.kind !== 'command')
          .map((s) => ({ name: s.name, description: s.description, params: s.params, source: skillOrigin(s.source), view: file(s.source) })),
        ...cat.pluginSkills.filter((s) => !known.has(s.name)).map((s) => ({ name: s.name, description: s.description, source: s.source, view: file(s.path) })),
      ],
      commands: skills
        .filter((s) => s.kind === 'command')
        .map((s) => ({ name: s.name, description: s.description, hint: s.argumentHint ?? '', source: skillOrigin(s.source) || (s.name.includes(':') ? s.name.split(':')[0] : ''), view: file(s.source) })),
      agents: agentList
        ? agentList.map((a) => ({ name: a.name, description: a.description, source: a.name.includes(':') ? a.name.split(':')[0] : skillOrigin(a.source), writes: a.writes, model: a.model, view: file(a.source) }))
        : cat.agents.map((a) => ({ name: a.name, description: a.description, source: a.source, view: file(a.path) })),
      modes: chatModes().map((m) => ({ name: m.name, description: m.description, view: file(m.path), active: m.name === activeChatMode()?.name })),
      mcpPrompts: mcp.prompts.map((p, i) => ({ key: String(i), name: `${p.server}:${p.name}`, description: p.description, params: p.arguments })),
      mcpResources: mcp.resources.map((r) => ({ name: `${r.server}:${r.name}`, token: `mcp:${r.server}:${r.uri}`, description: r.description || r.uri })),
      plugins: (pluginList
        ? pluginList.map((p) => ({ id: p.id, name: p.name, version: p.version, enabled: p.enabled, path: p.path, mcpServers: p.mcpServers, source: p.source, description: p.description, counts: { skills: p.skills, agents: p.agents, commands: p.commands } }))
        : cat.plugins.map((p) => ({ ...p, name: p.id.split('@')[0], source: 'claude', description: '', counts: undefined }))
      ).map((p) => {
        this.slashPlugins.set(p.id, p);
        return { id: p.id, version: p.version, enabled: p.enabled, description: p.description, source: p.source, counts: p.counts };
      }),
    });
  }

  /**
   * Turns a plugin on or off. Claude Code's plugins go through `claude plugin enable|disable`, so
   * Claude Code agrees; the others through `task-x.plugins.disabled`.
   */
  private async togglePlugin(p: PluginEntry & { source?: string; name?: string }, enable: boolean): Promise<boolean> {
    if (p.source === 'claude' && findClaude()) return setPlugin(p.id, enable);
    const c = vscode.workspace.getConfiguration(SECTION);
    const off = new Set(c.get<string[]>('plugins.disabled', []));
    for (const n of [p.id, p.name].filter((x): x is string => !!x)) enable ? off.delete(n) : off.add(n);
    await c.update('plugins.disabled', [...off].sort(), vscode.ConfigurationTarget.Global);
    return true;
  }

  /** The built-in tools for the Tools chip (#10), with the ones turned off for this window's chats. */
  private async postTools(): Promise<void> {
    const off = new Set(windowChoice().offTools ?? []);
    try {
      const list = await (await this.getClient()).request<{ name: string; description: string; permission: string }[]>('tools/list');
      this.post({ type: 'tools', tools: list.map((t) => ({ name: t.name, description: t.description.split('. ')[0], permission: t.permission, on: !off.has(t.name) })) });
    } catch (e) {
      this.log('warn', `tools/list failed: ${describeError(e)}`);
      this.post({ type: 'tools', tools: [] });
    }
  }

  /**
   * MCP servers for the chip: every server task-x found (Claude Code and its plugins, Claude Desktop,
   * VS Code, Copilot CLI, task-x settings), with where it came from and what the runtime reports.
   */
  private async postMcp(): Promise<void> {
    const origins = mcpOrigins(this.ctx);
    const off = new Set(disabledMcp());
    let status: { name: string; running: boolean; tools: string[]; error?: string; authRequired?: boolean; signedIn?: boolean }[] = [];
    try {
      status = (await (await this.getClient()).request<{ servers: typeof status }>('mcp/status')).servers;
    } catch (e) {
      this.log('warn', `mcp/status failed: ${describeError(e)}`);
    }
    const names = [...new Set([...Object.keys(origins), ...status.map((s) => s.name)])].sort((a, b) => a.localeCompare(b));
    this.post({
      type: 'mcp',
      servers: names.map((name) => {
        const s = status.find((x) => x.name === name);
        return { name, origin: origins[name] ?? 'task-x', enabled: !off.has(name), running: !!s?.running, tools: s?.tools.length ?? 0, error: s?.error ?? null, authRequired: !!s?.authRequired, signedIn: !!s?.signedIn };
      }),
    });
  }

  /** The MCP chip's actions: turn a server on or off, connect or disconnect it, reload all, add one, or show the details. */
  private async mcpAction(m: { action: string; name?: string; enable?: boolean }): Promise<void> {
    const name = typeof m.name === 'string' && /^[\w.-]{1,64}$/.test(m.name) ? m.name : undefined;
    switch (m.action) {
      case 'connect':
      case 'disconnect': {
        if (!name) return this.postMcp();
        try {
          const s = (await (await this.getClient()).request<{ servers: { name: string; authRequired?: boolean; signedIn?: boolean }[] }>('mcp/status')).servers.find((x) => x.name === name);
          const off = disabledMcp().includes(name);
          if (m.action === 'connect') {
            // Off: turn it on (the settings change starts it). Needs sign-in: the browser flow. Else: start the servers again.
            if (off) await setMcpDisabled(name, false);
            else if (s?.authRequired) await vscode.commands.executeCommand('task-x.mcpSignIn', name);
            else await vscode.commands.executeCommand('task-x.reloadMcp');
          } else if (s?.signedIn) await vscode.commands.executeCommand('task-x.mcpSignOut', name);
          else await setMcpDisabled(name, true);
        } catch (e) {
          vscode.window.showErrorMessage(`task-x: ${name}: ${describeError(e)}`);
        }
        setTimeout(() => void this.postMcp(), 1500);
        return this.postMcp();
      }
      case 'toggle':
        if (typeof m.name === 'string' && /^[\w.-]{1,64}$/.test(m.name)) await setMcpDisabled(m.name, m.enable !== true);
        // The settings change restarts the servers; the list shows the new state at once.
        setTimeout(() => void this.postMcp(), 1500);
        return this.postMcp();
      case 'reload':
        await vscode.commands.executeCommand('task-x.reloadMcp');
        return this.postMcp();
      case 'add':
        await vscode.commands.executeCommand('workbench.action.openSettingsJson', { revealSetting: { key: 'task-x.mcpServers', edit: true } });
        return;
      case 'details':
        await vscode.commands.executeCommand('task-x.mcpStatus');
        return;
    }
  }

  /** The `/` menu's actions: view a skill or agent, create one, and install or toggle plugins. */
  private async slashAction(m: { action: string; key?: string; enable?: boolean }): Promise<void> {
    const root = WorkspaceSync.root();
    const plugin = typeof m.key === 'string' ? this.slashPlugins.get(m.key) : undefined;
    switch (m.action) {
      case 'view': {
        const f = typeof m.key === 'string' ? this.slashFiles.get(m.key) : undefined;
        if (f) await vscode.window.showTextDocument(vscode.Uri.file(f));
        return;
      }
      case 'mcpPrompt': {
        // Fills in the prompt's arguments, then puts its text in the input to edit and send.
        const p = this.mcpPrompts[Number(m.key)];
        if (!p) return;
        const args: Record<string, string> = {};
        for (const a of p.arguments) {
          const v = await vscode.window.showInputBox({ title: `${p.server}:${p.name}`, prompt: a, ignoreFocusOut: true });
          if (v === undefined) return;
          if (v) args[a] = v;
        }
        try {
          const r = await (await this.getClient()).request<{ text: string }>('mcp/getPrompt', { server: p.server, name: p.name, arguments: args });
          this.post({ type: 'setInput', text: r.text });
        } catch (e) {
          vscode.window.showErrorMessage(`task-x: ${p.server}:${p.name}: ${describeError(e)}`);
        }
        return;
      }
      case 'mode':
        await setChatMode(m.key || undefined);
        this.refreshStatus();
        return this.postSkills();
      case 'newSkill':
        return newSkill(root);
      case 'newAgent':
        return newAgent(root);
      case 'newCommand':
        return newCommand(root);
      case 'installPlugin':
        await vscode.commands.executeCommand('task-x.installPlugin');
        return this.postSkills();
      case 'viewPlugin':
        if (plugin) await viewPlugin(plugin);
        return;
      case 'managePlugin':
        if (plugin && (await managePlugin(await this.getClient(), plugin.name ?? plugin.id, plugin.source ?? ''))) {
          await vscode.commands.executeCommand('task-x.reloadMcp');
          await this.postSkills();
        }
        return;
      case 'togglePlugin':
        if (plugin && (await this.togglePlugin(plugin, m.enable === true))) setTimeout(() => void this.postSkills(), 500);
        return;
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
      client.onNotification(
        'agent/event',
        // Bound to this run's tab: the event arrives outside the run's async context.
        AsyncResource.bind((p: unknown) => {
          const { taskId: id, event } = p as AgentEventParams;
          if (id === taskId) this.onEvent(event, state);
        }),
      ),
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
        subagentModel: claudeSubagentModel(),
        sessionId: sessions[taskId],
        maxBudgetUsd: budget > 0 ? budget : undefined,
        useTaskTools: c.get<boolean>('claudeCode.useTaskXTools', true),
        images,
        mcpServers: claudeMcpServers(this.ctx),
        // MCP servers turned off, and task-x tools turned off in the Tools chip (`mcp__task-x__<tool>`).
        disabledMcp: [...claudeDisabledMcp(this.ctx), ...(windowChoice().offTools ?? []).map((t) => `task-x__${t}`)],
        effort: effort(),
        ...claudePermissions(mode),
      },
      { signal },
    );
    // Read again: another tab's Claude Code run may have saved its session in the meantime.
    if (summary.sessionId) await this.ctx.workspaceState.update(CLAUDE_SESSIONS_KEY, { ...this.ctx.workspaceState.get<Record<string, string>>(CLAUDE_SESSIONS_KEY, {}), [taskId]: summary.sessionId });
    this.add({ kind: 'summary', ...summaryText(summary) });
    this.noteOperations(summary);
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
  <button type="button" id="indexBtn" class="pick index" hidden></button>
  <button type="button" role="tab" id="tabHistory" class="view-tab" aria-controls="history" title="History"><span class="codicon codicon-history" aria-hidden="true"></span><span class="view-label">History</span></button>
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
  <div id="tokenBar" class="token-bar" role="status" aria-live="polite" hidden></div>
  <div id="slash" role="listbox" aria-label="Skills" hidden></div>
  <div id="board" class="board" aria-label="Subtasks" hidden></div>
  <div id="todos" class="todos" aria-label="The agent's plan" hidden></div>
  <div id="queue" class="queue" aria-label="Queued messages" hidden></div>
  <div id="ctx" class="ctx" aria-label="Context sent with the message"></div>
  <div id="attachments" class="attachments" aria-label="Attached images" hidden></div>
  <label for="input" class="sr-only">Message</label>
  <textarea id="input" rows="1"></textarea>
  <div class="toolbar" role="toolbar" aria-label="Chat options">
    <button type="button" id="modelBtn" class="pick"></button>
    <button type="button" id="agentBtn" class="pick agent-pick"></button>
    <button type="button" id="nightPlanBtn" class="pick" hidden></button>
    <button type="button" id="nightLoopBtn" class="pick" hidden></button>
    <button type="button" id="settingsBtn" class="pick settings-pick"></button>
    <button type="button" id="effortBtn" class="pick effort-pick"></button>
    <button type="button" id="mcpBtn" class="pick mcp-pick"></button>
    <span class="toolbar-end">
    <span id="ctxMeter" class="meter" role="meter" aria-valuemin="0" aria-valuemax="100" hidden></span>
    <button type="button" id="refineBtn" class="action" title="Refine: rewrite the prompt with the routed model (task-x.routing), else the active model. Ctrl+Z restores your text." aria-label="Refine prompt"><span class="codicon codicon-sparkle" aria-hidden="true"></span></button>
    <button type="button" id="send" class="action primary"></button>
    </span>
  </div>
</form>
<script nonce="${nonce}" src="${hljs}"></script>
<script nonce="${nonce}" src="${js}"></script>
</body>
</html>`;
  }
}
