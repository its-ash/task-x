// Wire types mirroring runtime/src/provider/types.rs and docs/protocol.md.

export const PROTOCOL_VERSION = 1;

export const ErrorCode = {
  ParseError: -32700,
  InvalidRequest: -32600,
  MethodNotFound: -32601,
  InvalidParams: -32602,
  Internal: -32603,
  Provider: -32001,
  ProtocolMismatch: -32002,
  NotInitialized: -32003,
  RequestCancelled: -32800,
} as const;

export const PROVIDER_KINDS = ['anthropic', 'openai', 'openaiCompatible', 'ollama'] as const;
export type ProviderKind = (typeof PROVIDER_KINDS)[number];

export const PROVIDER_LABELS: Record<ProviderKind, string> = {
  anthropic: 'Anthropic Claude',
  openai: 'OpenAI',
  openaiCompatible: 'OpenAI-compatible',
  ollama: 'Ollama (local)',
};

/** Mirrors `ProviderKind::requires_api_key` in the runtime. */
export const requiresApiKey = (kind: ProviderKind) => kind !== 'ollama';

export const DEFAULT_BASE_URLS: Record<ProviderKind, string> = {
  anthropic: 'https://api.anthropic.com',
  openai: 'https://api.openai.com/v1',
  openaiCompatible: '',
  ollama: 'http://127.0.0.1:11434',
};

export interface ProviderConfig {
  kind: ProviderKind;
  model: string;
  baseUrl?: string;
  apiKey?: string;
  timeoutMs?: number;
}

export type ContentPart =
  | { type: 'text'; text: string }
  | { type: 'toolUse'; id: string; name: string; input: unknown }
  | { type: 'toolResult'; toolUseId: string; toolName?: string; content: string; isError?: boolean };

export interface Message {
  role: 'user' | 'assistant';
  content: ContentPart[];
}

export interface ToolSpec {
  name: string;
  description: string;
  inputSchema: Record<string, unknown>;
}

export interface ChatRequest {
  system?: string;
  messages: Message[];
  tools?: ToolSpec[];
  maxTokens?: number;
  temperature?: number;
}

export type StopReason = 'endTurn' | 'maxTokens' | 'toolUse' | 'stopSequence' | 'contentFilter' | 'unknown';

export interface Usage {
  inputTokens: number | null;
  outputTokens: number | null;
}

export type StreamEvent =
  | { type: 'textDelta'; text: string }
  | { type: 'toolCall'; id: string; name: string; input: unknown }
  | { type: 'usage'; usage: Usage }
  | { type: 'done'; stopReason: StopReason };

export interface ChatEventParams {
  requestId: number | string;
  event: StreamEvent;
}

export interface ChatOutcome {
  text: string;
  toolCalls: { id: string; name: string; input: unknown }[];
  stopReason: StopReason;
  usage: Usage;
}

export interface Capabilities {
  streaming: boolean;
  toolCalling: boolean;
  parallelToolCalls: boolean;
  systemPrompt: boolean;
  messageFormat: 'anthropicMessages' | 'openAiChat' | 'ollamaChat';
}

export interface TestReport {
  latencyMs: number;
  models: string[];
  modelAvailable: boolean | null;
  capabilities: Capabilities;
}

/** Result of `provider/contextWindow`. */
export interface ContextWindowResult {
  tokens: number;
}

export interface InitializeResult {
  protocolVersion: number;
  serverInfo: { name: string; version: string };
  providers: { kind: ProviderKind; capabilities: Capabilities; requiresApiKey: boolean }[];
}

export interface ProviderErrorData {
  kind: string;
  message: string;
  status?: number;
  retryable: boolean;
}

export interface ProjectCommand {
  argv: string[];
  cwd: string;
  source: string;
}

export interface WorkspaceSummary {
  project: {
    root: string;
    gitRoot: string | null;
    languages: { language: string; files: number }[];
    manifests: string[];
    packageManagers: string[];
    frameworks: string[];
    buildSystems: string[];
    testFrameworks: string[];
    commands: Record<'build' | 'test' | 'lint' | 'format' | 'typecheck', ProjectCommand[]>;
  };
  stats: {
    files: number;
    textFiles: number;
    contentBytes: number;
    symbols: number;
    skippedBinary: number;
    skippedLarge: number;
    skippedExcluded: number;
    skippedGenerated: number;
    reparsed: number;
    truncated: boolean;
  };
  instructions: { source: string; scope: 'user' | 'workspace' | 'directory'; dir: string; bytes: number; truncated: boolean }[];
  excludes: string[];
}

/** `graph/project` stats and the `graph` field of a ready `workspace/progress`. */
export interface GraphStats {
  files: number;
  edges: number;
  importEdges: number;
  referenceEdges: number;
  modules: number;
  cycles: number;
  orphans: number;
  hubs: { path: string; dependents: number }[];
  importCycles: string[][];
  buildMs: number;
}

/** `workspace/progress` notification: indexing status. */
export type WorkspaceProgress =
  | { phase: 'indexing'; files: number; parsed: number; elapsedMs: number }
  | { phase: 'graph'; files: number; elapsedMs: number }
  | { phase: 'ready'; stats: WorkspaceSummary['stats']; graph: GraphStats; elapsedMs: number }
  | { phase: 'updated'; stats: WorkspaceSummary['stats'] }
  | { phase: 'failed'; error: string };

export interface GraphView {
  nodes: { id: string; language: string | null; symbols: number; files: number; degree: number; inCycle: boolean }[];
  edges: { from: number; to: number; weight: number; kind: 'import' | 'reference' }[];
  total: number;
  stats: GraphStats;
}

export interface FileDependencies {
  path: string;
  dependsOn: { path: string; kind: 'import' | 'reference'; symbols: number }[];
  usedBy: { path: string; kind: 'import' | 'reference'; symbols: number }[];
  importCycle: string[] | null;
}

export interface Snippet {
  path: string;
  startLine: number;
  endLine: number;
  text: string;
  score: number;
  reason: string;
}

export interface ContextResult {
  retrieval: { terms: string[]; snippets: Snippet[]; filesConsidered: number; budgetTokens: number; usedTokens: number };
  instructions: { source: string; scope: 'user' | 'workspace' | 'directory'; dir: string; content: string; truncated: boolean }[];
}

export const CHAT_MODES = ['ask', 'plan', 'agent', 'team'] as const;
export type ChatMode = (typeof CHAT_MODES)[number];
/** The modes the chat offers: Agent only. Other modes still run when the runtime is asked for them. */
export const USER_MODES: readonly ChatMode[] = ['agent'];
export type ApprovalMode = 'readOnly' | 'askBeforeEdit' | 'askBeforeExecution' | 'autonomous';
export type ApprovalDecision = 'allow' | 'allowSession' | 'deny';

/** An image attached to a prompt: base64 `data` without a `data:` prefix. */
export interface ImageInput {
  mediaType: 'image/png' | 'image/jpeg' | 'image/gif' | 'image/webp';
  data: string;
}
export const IMAGE_TYPES: readonly ImageInput['mediaType'][] = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'];
/** At most this many images per prompt, each at most `MAX_IMAGE_B64` base64 characters (about 5 MB). */
export const MAX_IMAGES = 8;
export const MAX_IMAGE_B64 = 7_000_000;

/** The valid images of a webview message; anything else is dropped. */
export function imagesFrom(v: unknown): ImageInput[] {
  if (!Array.isArray(v)) return [];
  return v
    .filter((i): i is ImageInput => !!i && IMAGE_TYPES.includes(i.mediaType) && typeof i.data === 'string' && i.data.length > 0 && i.data.length <= MAX_IMAGE_B64 && /^[A-Za-z0-9+/=]+$/.test(i.data))
    .slice(0, MAX_IMAGES)
    .map((i) => ({ mediaType: i.mediaType, data: i.data }));
}

export interface EditorContext {
  activeFile?: string;
  selection?: { path: string; startLine: number; endLine: number; text: string };
  openFiles: string[];
  diagnostics: { path: string; line: number; severity: string; message: string; source?: string }[];
  /** Files and folders referenced with `@path` in the prompt. */
  mentions?: string[];
}

export type QuestionKind = 'radio' | 'checkbox';

/** A question the agent asks, with the options to pick from. */
export interface Question {
  id: string;
  text: string;
  kind: QuestionKind;
  options: string[];
}

export interface AskUserRequest {
  taskId: string;
  callId: string;
  questions: Question[];
}

/** Chosen option texts by question id; `answers` is null when the user dismissed the questions. */
export interface AskUserResult {
  answers: Record<string, string[]> | null;
}

export interface ApprovalRequest {
  taskId: string;
  callId: string;
  tool: string;
  permission: string;
  input: Record<string, unknown>;
  preview: string | null;
}

export interface ValidationRun {
  tool: string;
  command: string | null;
  success: boolean;
  summary: string;
}

export const ROLES = ['research', 'implementation', 'testing', 'review', 'documentation'] as const;
export type Role = (typeof ROLES)[number];

/** A price rule for cost estimates (USD per million tokens, matched by model id prefix). */
export interface PriceRule {
  model: string;
  inputPerMtok: number;
  outputPerMtok: number;
}

export interface Metrics {
  modelCalls: number;
  costUsd?: number;
  unpricedTokens?: number;
  subagents?: number;
  subagentCostUsd?: number;
  subagentTokens?: number;
  localTokens: number;
  billedTokens: number;
  [key: string]: unknown;
}

/** One result of `agent/bestOf`. */
export interface Candidate {
  name: string | null;
  kind: ProviderKind;
  model: string;
  status: TaskStatus;
  report?: string;
  error?: string;
  files?: string[];
  added?: number;
  removed?: number;
  validations?: ValidationRun[];
  costUsd?: number;
  tokens?: number;
  dir?: string | null;
  patch?: string;
}

export type TaskStatus = 'running' | 'completed' | 'cancelled' | 'maxIterations' | 'budgetExceeded' | 'deadlineExceeded' | 'providerError' | 'stuck' | 'failed';

/** `prompt/refine` result: the rewritten prompt and the model that wrote it. */
export interface RefineResult {
  text: string;
  kind: string;
  model: string;
  routed: boolean;
  usage?: Usage;
}

export interface AgentSummary {
  taskId: string;
  status: TaskStatus;
  chatMode: ChatMode;
  text: string;
  changedFiles: string[];
  operations: number[];
  validations: ValidationRun[];
  validationRan: boolean;
  skippedChecks?: string[];
  usage: Usage;
  metrics?: Metrics;
  iterations: number;
  toolCalls: number;
  error: string | null;
}

export type AgentEvent =
  | { type: 'started'; chatMode: ChatMode; enrichment: { kind: string; plan: string[]; instructions: string[]; sources: { path: string; startLine: number; endLine: number; reason: string }[]; ambiguity: string | null }; tools: string[]; skills?: { name: string; reason: string }[] }
  | { type: 'iteration'; n: number }
  | { type: 'textDelta'; text: string }
  | { type: 'assistantMessage'; text: string; toolCalls: number }
  | { type: 'retry'; attempt: number; reason: string; waitMs: number }
  | { type: 'toolStarted'; callId: string; tool: string; input: Record<string, unknown>; permission: string }
  | { type: 'toolFinished'; callId: string; tool: string; ok: boolean; output: string; background?: boolean }
  | { type: 'approvalRequested'; callId: string; tool: string }
  | { type: 'approvalResolved'; callId: string; decision: string }
  | { type: 'fileChanged'; path: string; diff: string; operation: number | null }
  | { type: 'steered'; text: string }
  | { type: 'usage'; usage: Usage; costUsd?: number }
  | { type: 'warning'; message: string }
  | { type: 'error'; error: ProviderErrorData }
  | { type: 'done'; summary: AgentSummary }
  | { type: 'subagent'; name: string; taskId: string; event: AgentEvent }
  | { type: 'modelSwitched'; from: { kind: string; model: string }; to: { kind: string; model: string }; reason: string }
  | { type: 'model'; kind: string; model: string; reason: string }
  | { type: 'compacted'; folded: number; pruned?: number; droppedRetrieval?: boolean; beforeTokens?: number; afterTokens?: number; windowTokens?: number }
  | { type: 'context'; usedTokens: number; windowTokens: number }
  | { type: 'hook'; event: string; command: string; blocked: boolean; message: string }
  | { type: 'subagentStarted'; name: string; taskId: string; role: Role; objective: string; kind: ProviderKind; model: string; dir: string | null; step: string | null }
  | { type: 'subagentFinished'; name: string; taskId: string; role: Role; status: TaskStatus; error?: string | null; costUsd?: number; unpricedTokens?: number; tokens?: number; files?: string[]; added?: number; removed?: number; worktree?: string | null }
  | { type: 'planStarted'; callId: string; maxParallel: number; steps: { id: string; role: Role; objective: string; dependsOn: string[] }[] }
  | { type: 'stepSkipped'; step: string; reason: string }
  | { type: 'merged'; step: string; name: string; ok: boolean; files?: string[]; error?: string }
  | { type: 'bestOfDone'; candidates: number; costUsd: number };

/** One entry of `skills/list`. */
export interface SkillInfo {
  name: string;
  description: string;
  source: string;
  triggers: string[];
  patterns: string[];
  params: string[];
  onDemand: boolean;
}

/** One entry of `skills/resolve`: a skill that applies to the prompt, with params filled in. */
export interface ResolvedSkill {
  name: string;
  reason: string;
  source: string;
  text: string;
}

export interface AgentEventParams {
  taskId: string;
  event: AgentEvent;
}
