import * as vscode from 'vscode';
import { SECTION } from './config.js';
import type { ChatMode } from './protocol.js';
import { WorkspaceSync } from './workspace.js';

/**
 * Links above functions, methods, and classes of workspace files: "Explain", "Write tests", and
 * "Fix N problems" when the editor reports errors or warnings inside it. Each sends a request to
 * the chat that names the symbol and its lines. `task-x.codeLens.enabled` turns them off.
 */

const SYMBOL_KINDS = new Set([vscode.SymbolKind.Function, vscode.SymbolKind.Method, vscode.SymbolKind.Constructor, vscode.SymbolKind.Class]);
/** Files longer than this get no links: symbol lookups on them are slow. */
const MAX_LINES = 20_000;
/** At most this many symbols per file get links. */
const MAX_SYMBOLS = 150;
/** Symbols shorter than this (one-line getters, small helpers) get none. */
const MIN_LINES = 3;
const MAX_PROBLEMS = 10;

type Target = { uri: vscode.Uri; range: vscode.Range; name: string; kind: string };

const where = (t: Target) => `${WorkspaceSync.relative(t.uri) ?? t.uri.fsPath}:${t.range.start.line + 1}-${t.range.end.line + 1}`;
const kindName = (k: vscode.SymbolKind) => (k === vscode.SymbolKind.Class ? 'class' : k === vscode.SymbolKind.Method || k === vscode.SymbolKind.Constructor ? 'method' : 'function');

function problemsIn(uri: vscode.Uri, range: vscode.Range): vscode.Diagnostic[] {
  return vscode.languages
    .getDiagnostics(uri)
    .filter((d) => d.severity <= vscode.DiagnosticSeverity.Warning && d.source !== 'task-x review' && range.contains(d.range.start));
}

/** Functions, methods, and classes, nested ones too (methods inside classes). */
function flatten(symbols: vscode.DocumentSymbol[], out: vscode.DocumentSymbol[] = [], depth = 0): vscode.DocumentSymbol[] {
  for (const s of symbols) {
    if (out.length >= MAX_SYMBOLS) break;
    if (SYMBOL_KINDS.has(s.kind) && s.range.end.line - s.range.start.line + 1 >= MIN_LINES) out.push(s);
    if (depth < 2 && s.children?.length) flatten(s.children, out, depth + 1);
  }
  return out;
}

export class SymbolLens implements vscode.CodeLensProvider, vscode.Disposable {
  private readonly changed = new vscode.EventEmitter<void>();
  readonly onDidChangeCodeLenses = this.changed.event;
  private readonly subs: vscode.Disposable[];
  private timer?: NodeJS.Timeout;

  constructor(private readonly submit: (text: string, mode?: ChatMode) => Promise<void>) {
    this.subs = [
      vscode.languages.registerCodeLensProvider({ scheme: 'file' }, this),
      // "Fix N problems" follows the editor's diagnostics, which change in bursts while typing.
      vscode.languages.onDidChangeDiagnostics(() => {
        clearTimeout(this.timer);
        this.timer = setTimeout(() => this.changed.fire(), 600);
      }),
      vscode.workspace.onDidChangeConfiguration((e) => e.affectsConfiguration(`${SECTION}.codeLens`) && this.changed.fire()),
      vscode.commands.registerCommand('task-x.lens.explain', (t: Target) => this.explain(t)),
      vscode.commands.registerCommand('task-x.lens.tests', (t: Target) => this.tests(t)),
      vscode.commands.registerCommand('task-x.lens.fix', (t: Target) => this.fix(t)),
      this.changed,
    ];
  }

  dispose(): void {
    clearTimeout(this.timer);
    for (const d of this.subs) d.dispose();
  }

  async provideCodeLenses(doc: vscode.TextDocument, token: vscode.CancellationToken): Promise<vscode.CodeLens[]> {
    if (!vscode.workspace.getConfiguration(SECTION).get<boolean>('codeLens.enabled', true)) return [];
    if (doc.lineCount > MAX_LINES || !WorkspaceSync.relative(doc.uri)) return [];
    let symbols: vscode.DocumentSymbol[] | undefined;
    try {
      symbols = await vscode.commands.executeCommand<vscode.DocumentSymbol[]>('vscode.executeDocumentSymbolProvider', doc.uri);
    } catch {
      return [];
    }
    // Some providers answer with flat SymbolInformation (no `children`, no `selectionRange`): skipped.
    if (token.isCancellationRequested || !Array.isArray(symbols) || !symbols.every((s) => 'selectionRange' in s)) return [];
    const lenses: vscode.CodeLens[] = [];
    for (const s of flatten(symbols)) {
      const t: Target = { uri: doc.uri, range: s.range, name: s.name, kind: kindName(s.kind) };
      const at = new vscode.Range(s.selectionRange.start, s.selectionRange.start);
      lenses.push(new vscode.CodeLens(at, { title: '$(sparkle) Explain', command: 'task-x.lens.explain', arguments: [t], tooltip: `Ask task-x to explain ${s.name}` }));
      if (t.kind !== 'class') lenses.push(new vscode.CodeLens(at, { title: 'Write tests', command: 'task-x.lens.tests', arguments: [t], tooltip: `Ask task-x to write tests for ${s.name}` }));
      const n = problemsIn(doc.uri, s.range).length;
      if (n) lenses.push(new vscode.CodeLens(at, { title: `$(wrench) Fix ${n} problem${n === 1 ? '' : 's'}`, command: 'task-x.lens.fix', arguments: [t], tooltip: `Ask task-x to fix the problems in ${s.name}` }));
    }
    return lenses;
  }

  private explain(t: Target): Promise<void> {
    return this.submit(
      `Explain the ${t.kind} \`${t.name}\` in ${where(t)}: what it does, how it is used (find its references), and anything surprising about it. Do not edit files.`,
      'ask',
    );
  }

  private tests(t: Target): Promise<void> {
    return this.submit(
      `Write unit tests for the ${t.kind} \`${t.name}\` in ${where(t)}. Follow the project's existing test framework, location, and style; cover edge cases and error paths; then run the tests and fix failures.`,
      'agent',
    );
  }

  private fix(t: Target): Promise<void> {
    const list = problemsIn(t.uri, t.range)
      .slice(0, MAX_PROBLEMS)
      .map((d) => `- line ${d.range.start.line + 1}: ${d.message.split('\n')[0]}${d.source ? ` (${d.source}${d.code ? ` ${typeof d.code === 'object' ? d.code.value : d.code}` : ''})` : ''}`);
    if (!list.length) return Promise.resolve(void vscode.window.showInformationMessage(`task-x: ${t.name} has no problems now.`));
    return this.submit(
      `Fix these problems in the ${t.kind} \`${t.name}\` (${where(t)}):\n${list.join('\n')}\n\nFind the root cause, make the smallest correct change, and run the relevant check.`,
      'agent',
    );
  }
}
