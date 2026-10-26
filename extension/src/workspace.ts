import path from 'node:path';
import * as vscode from 'vscode';
import { SECTION } from './config.js';
import type { ContextResult, GraphStats, WorkspaceProgress, WorkspaceSummary } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';

const DEBOUNCE_MS = 300;

/** What the index is doing, for the status bar and the chat. */
export type IndexState =
  | { state: 'none' }
  | { state: 'indexing'; files: number; parsed: number; elapsedMs: number; phase: 'indexing' | 'graph' }
  | { state: 'ready'; files: number; symbols: number; textFiles: number; graph?: GraphStats; elapsedMs?: number }
  | { state: 'failed'; error: string };
const MAX_BATCH = 2_000;

/** Keeps the runtime's workspace index in sync with the first workspace folder. */
export class WorkspaceSync implements vscode.Disposable {
  private readonly watcher: vscode.FileSystemWatcher;
  private pending = new Set<string>();
  private timer: NodeJS.Timeout | undefined;
  private client: JsonRpcClient | undefined;
  private readonly wired = new WeakSet<JsonRpcClient>();
  private readonly changed = new vscode.EventEmitter<IndexState>();
  /** Fires on every indexing progress update. */
  readonly onState = this.changed.event;
  state: IndexState = { state: 'none' };
  summary: WorkspaceSummary | undefined;

  constructor(private readonly log: vscode.LogOutputChannel) {
    this.watcher = vscode.workspace.createFileSystemWatcher('**/*');
    const queue = (u: vscode.Uri) => this.queue(u);
    this.watcher.onDidCreate(queue);
    this.watcher.onDidChange(queue);
    this.watcher.onDidDelete(queue);
  }

  static root(): string | undefined {
    const f = vscode.workspace.workspaceFolders?.[0];
    return f?.uri.scheme === 'file' ? f.uri.fsPath : undefined;
  }

  private setState(s: IndexState): void {
    this.state = s;
    this.changed.fire(s);
  }

  private onProgress(p: WorkspaceProgress): void {
    const graph = this.state.state === 'ready' ? this.state.graph : undefined;
    switch (p.phase) {
      case 'indexing':
        return this.setState({ state: 'indexing', phase: 'indexing', files: p.files, parsed: p.parsed, elapsedMs: p.elapsedMs });
      case 'graph':
        return this.setState({ state: 'indexing', phase: 'graph', files: p.files, parsed: p.files, elapsedMs: p.elapsedMs });
      case 'ready':
        return this.setState({ state: 'ready', files: p.stats.files, symbols: p.stats.symbols, textFiles: p.stats.textFiles, graph: p.graph, elapsedMs: p.elapsedMs });
      case 'updated':
        // Edits change counts but the graph is rebuilt only when someone asks for it.
        return this.setState({ state: 'ready', files: p.stats.files, symbols: p.stats.symbols, textFiles: p.stats.textFiles, graph });
      case 'failed':
        return this.setState({ state: 'failed', error: p.error });
    }
  }

  async open(client: JsonRpcClient): Promise<WorkspaceSummary | undefined> {
    this.client = client;
    if (!this.wired.has(client)) {
      this.wired.add(client);
      client.onNotification('workspace/progress', (p) => this.onProgress(p as WorkspaceProgress));
    }
    const root = WorkspaceSync.root();
    if (!root) return undefined;
    const c = vscode.workspace.getConfiguration(SECTION);
    const exclude = c.get<string[]>('index.exclude', []).filter((g) => typeof g === 'string' && g.trim());
    this.pending.clear();
    const started = Date.now();
    this.summary = await client.request<WorkspaceSummary>('workspace/open', { root, exclude });
    const s = this.summary.stats;
    this.log.info(`indexed ${s.files} files (${s.symbols} symbols, ${this.summary.instructions.length} instruction files) in ${Date.now() - started} ms`);
    return this.summary;
  }

  /** Root-relative path with `/` separators, or undefined when outside the workspace. */
  static relative(uri: vscode.Uri): string | undefined {
    const root = WorkspaceSync.root();
    if (!root || uri.scheme !== 'file') return undefined;
    const rel = path.relative(root, uri.fsPath);
    return rel && !rel.startsWith('..') && !path.isAbsolute(rel) ? rel.split(path.sep).join('/') : undefined;
  }

  async retrieve(query: string, focusPaths: string[], budgetTokens?: number): Promise<ContextResult> {
    if (!this.client || this.client.isClosed) throw new Error('The runtime is not running.');
    return this.client.request<ContextResult>('context/retrieve', { query, focusPaths, budgetTokens });
  }

  private queue(uri: vscode.Uri): void {
    if (uri.scheme !== 'file' || !this.client) return;
    this.pending.add(uri.fsPath);
    clearTimeout(this.timer);
    this.timer = setTimeout(() => this.flush(), DEBOUNCE_MS);
  }

  private flush(): void {
    if (!this.client || this.client.isClosed || !this.pending.size) return;
    const paths = [...this.pending];
    this.pending.clear();
    for (let i = 0; i < paths.length; i += MAX_BATCH) this.client.notify('workspace/filesChanged', { paths: paths.slice(i, i + MAX_BATCH) });
  }

  dispose(): void {
    this.changed.dispose();
    clearTimeout(this.timer);
    this.watcher.dispose();
  }
}

const fence = (text: string) => {
  const longest = Math.max(2, ...[...text.matchAll(/`+/g)].map((m) => m[0].length));
  return '`'.repeat(longest + 1);
};

export function renderSummary(s: WorkspaceSummary): string {
  const p = s.project;
  const list = (xs: string[]) => (xs.length ? xs.join(', ') : '—');
  const cmds = Object.entries(p.commands)
    .flatMap(([k, v]) => v.map((c) => `| ${k} | \`${c.argv.join(' ')}\` | ${c.cwd || '.'} | ${c.source} |`))
    .join('\n');
  const st = s.stats;
  return [
    `# task-x workspace context`,
    ``,
    `**Root:** ${p.root}${p.gitRoot ? `  \n**Git root:** ${p.gitRoot}` : ''}`,
    ``,
    `## Project`,
    `- Languages: ${list(p.languages.map((l) => `${l.language} (${l.files})`))}`,
    `- Manifests: ${list(p.manifests)}`,
    `- Package managers: ${list(p.packageManagers)}`,
    `- Frameworks: ${list(p.frameworks)}`,
    `- Build systems: ${list(p.buildSystems)}`,
    `- Test frameworks: ${list(p.testFrameworks)}`,
    ``,
    `### Detected commands`,
    cmds ? `| kind | command | cwd | from |\n|---|---|---|---|\n${cmds}` : '_None detected._',
    ``,
    `## Instructions (lowest → highest precedence)`,
    s.instructions.length
      ? s.instructions.map((i, n) => `${n + 1}. \`${i.source}\` — ${i.scope}${i.dir ? ` (applies to \`${i.dir}/\`)` : ''}, ${i.bytes} bytes${i.truncated ? ', truncated' : ''}`).join('\n')
      : '_No instruction files found._',
    ``,
    `## Index`,
    `- Files: ${st.files} (${st.textFiles} text, ${(st.contentBytes / 1024).toFixed(0)} KiB)`,
    `- Symbols: ${st.symbols}`,
    `- Skipped: ${st.skippedExcluded} excluded, ${st.skippedGenerated ?? 0} generated, ${st.skippedBinary} binary, ${st.skippedLarge} too large${st.truncated ? ' — **index truncated at its limit**' : ''}`,
    ``,
    `### Exclusions`,
    `Add patterns with the \`task-x.index.exclude\` setting or an \`.anvilignore\` file.`,
    ``,
    s.excludes.map((e) => `- \`${e}\``).join('\n'),
  ].join('\n');
}

export function renderRetrieval(query: string, r: ContextResult): string {
  const parts = [
    `# Context for: ${query.replace(/\n/g, ' ')}`,
    ``,
    `Terms: ${r.retrieval.terms.map((t) => `\`${t}\``).join(' ') || '—'} · ${r.retrieval.snippets.length} snippets from ${r.retrieval.filesConsidered} candidate files · ~${r.retrieval.usedTokens}/${r.retrieval.budgetTokens} tokens`,
    ``,
    `## Instructions applied`,
    r.instructions.length ? r.instructions.map((i) => `- \`${i.source}\` (${i.scope})`).join('\n') : '_None._',
    ``,
    `## Snippets`,
  ];
  for (const s of r.retrieval.snippets) {
    const f = fence(s.text);
    parts.push(`### ${s.path}:${s.startLine}-${s.endLine}`, `_${s.reason} · score ${s.score.toFixed(1)}_`, '', `${f}`, s.text, f, '');
  }
  return parts.join('\n');
}
