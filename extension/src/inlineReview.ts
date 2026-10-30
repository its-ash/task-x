/**
 * Shows the agent's pending edits inside the editor: changed lines get a green background, removed
 * lines a red marker, and each change a CodeLens row: Keep file, Undo change, Undo file, Diff.
 * "Undo change" puts back the original lines of that one change; the rest stays for review.
 */
import * as vscode from 'vscode';
import { WorkspaceSync } from './workspace.js';

/** Files above this many lines get only the file-level lenses: the diff would be slow. */
const MAX_LINES = 8000;

export interface Hunk {
  /** First changed line in the current text (0-based), and how many lines it has now. */
  start: number;
  count: number;
  /** The original lines this change replaced. */
  before: string[];
}

/** Line changes between two texts, as hunks in the new text (an LCS diff over lines). */
export function hunks(original: string, current: string): Hunk[] {
  const a = original.split('\n');
  const b = current.split('\n');
  // Common start and end first: most edits touch a small part of the file.
  let s = 0;
  while (s < a.length && s < b.length && a[s] === b[s]) s++;
  let ea = a.length, eb = b.length;
  while (ea > s && eb > s && a[ea - 1] === b[eb - 1]) {
    ea--;
    eb--;
  }
  const A = a.slice(s, ea), B = b.slice(s, eb);
  const n = A.length, m = B.length;
  if (!n && !m) return [];
  // Too large a middle for the table: one hunk for all of it.
  if (n * m > 4_000_000) return [{ start: s, count: m, before: A }];
  const dp = Array.from({ length: n + 1 }, () => new Uint32Array(m + 1));
  for (let i = n - 1; i >= 0; i--) for (let j = m - 1; j >= 0; j--) dp[i][j] = A[i] === B[j] ? dp[i + 1][j + 1] + 1 : Math.max(dp[i + 1][j], dp[i][j + 1]);
  const out: Hunk[] = [];
  let cur: Hunk | undefined;
  const flush = () => {
    if (cur) out.push(cur);
    cur = undefined;
  };
  let i = 0, j = 0;
  while (i < n || j < m) {
    if (i < n && j < m && A[i] === B[j]) {
      flush();
      i++;
      j++;
    } else if (j < m && (i >= n || dp[i][j + 1] >= dp[i + 1][j])) {
      cur ??= { start: s + j, count: 0, before: [] };
      cur.count++;
      j++;
    } else {
      cur ??= { start: s + j, count: 0, before: [] };
      cur.before.push(A[i]);
      i++;
    }
  }
  flush();
  return out;
}

export interface ReviewActions {
  keep(rel: string): Promise<void>;
  undoFile(rel: string): Promise<void>;
  diff(rel: string): Promise<void>;
}

export class InlineReview implements vscode.CodeLensProvider, vscode.Disposable {
  private originals = new Map<string, string | null>();
  private readonly added = vscode.window.createTextEditorDecorationType({
    isWholeLine: true,
    backgroundColor: new vscode.ThemeColor('diffEditor.insertedLineBackground'),
    overviewRulerColor: new vscode.ThemeColor('editorOverviewRuler.addedForeground'),
    overviewRulerLane: vscode.OverviewRulerLane.Left,
  });
  private readonly removed = vscode.window.createTextEditorDecorationType({
    isWholeLine: true,
    borderColor: new vscode.ThemeColor('diffEditor.removedTextBorder'),
    borderStyle: 'solid none none none',
    borderWidth: '2px 0 0 0',
    overviewRulerColor: new vscode.ThemeColor('editorOverviewRuler.deletedForeground'),
    overviewRulerLane: vscode.OverviewRulerLane.Left,
  });
  private readonly changed = new vscode.EventEmitter<void>();
  readonly onDidChangeCodeLenses = this.changed.event;
  private readonly subs: vscode.Disposable[];

  constructor(private readonly actions: ReviewActions) {
    this.subs = [
      vscode.languages.registerCodeLensProvider({ scheme: 'file' }, this),
      vscode.window.onDidChangeVisibleTextEditors(() => this.decorate()),
      vscode.workspace.onDidChangeTextDocument((e) => {
        if (this.relOf(e.document)) {
          this.decorate();
          this.changed.fire();
        }
      }),
      vscode.commands.registerCommand('task-x.review.keep', (rel: string) => this.actions.keep(rel)),
      vscode.commands.registerCommand('task-x.review.undoFile', (rel: string) => this.actions.undoFile(rel)),
      vscode.commands.registerCommand('task-x.review.diff', (rel: string) => this.actions.diff(rel)),
      vscode.commands.registerCommand('task-x.review.undoHunk', (uri: vscode.Uri, h: Hunk) => this.undoHunk(uri, h)),
      this.added,
      this.removed,
      this.changed,
    ];
  }

  /** The pending files with their original text (null for a file the agent created). */
  update(files: { path: string; original: string | null }[]): void {
    this.originals = new Map(files.map((f) => [f.path, f.original]));
    this.decorate();
    this.changed.fire();
  }

  private relOf(doc: vscode.TextDocument): string | undefined {
    const rel = doc.uri.scheme === 'file' ? WorkspaceSync.relative(doc.uri) : undefined;
    return rel && this.originals.has(rel) ? rel : undefined;
  }

  private hunksOf(doc: vscode.TextDocument): Hunk[] | undefined {
    const rel = this.relOf(doc);
    if (!rel || doc.lineCount > MAX_LINES) return undefined;
    return hunks(this.originals.get(rel) ?? '', doc.getText().replace(/\r\n/g, '\n'));
  }

  private decorate(): void {
    for (const ed of vscode.window.visibleTextEditors) {
      const hs = this.hunksOf(ed.document) ?? [];
      const last = Math.max(0, ed.document.lineCount - 1);
      ed.setDecorations(this.added, hs.filter((h) => h.count).map((h) => new vscode.Range(h.start, 0, Math.min(last, h.start + h.count - 1), 0)));
      // A change that only removed lines shows as a red line where they were.
      ed.setDecorations(this.removed, hs.filter((h) => !h.count && h.before.length).map((h) => {
        const line = Math.min(last, h.start);
        return { range: new vscode.Range(line, 0, line, 0), hoverMessage: `task-x removed ${h.before.length} line${h.before.length > 1 ? 's' : ''}` };
      }));
    }
  }

  provideCodeLenses(doc: vscode.TextDocument): vscode.CodeLens[] {
    const rel = this.relOf(doc);
    if (!rel) return [];
    const top = new vscode.Range(0, 0, 0, 0);
    const lenses = [
      new vscode.CodeLens(top, { title: '$(check) Keep file', command: 'task-x.review.keep', arguments: [rel], tooltip: 'Keep every task-x change in this file' }),
      new vscode.CodeLens(top, { title: '$(discard) Undo file', command: 'task-x.review.undoFile', arguments: [rel], tooltip: 'Put back the file as it was before task-x' }),
      new vscode.CodeLens(top, { title: '$(diff) Diff', command: 'task-x.review.diff', arguments: [rel] }),
    ];
    const hs = this.hunksOf(doc);
    for (const h of hs ?? []) {
      const r = new vscode.Range(Math.min(h.start, Math.max(0, doc.lineCount - 1)), 0, Math.min(h.start, Math.max(0, doc.lineCount - 1)), 0);
      const what = h.count && h.before.length ? `changed ${h.count} line${h.count > 1 ? 's' : ''}` : h.count ? `added ${h.count} line${h.count > 1 ? 's' : ''}` : `removed ${h.before.length} line${h.before.length > 1 ? 's' : ''}`;
      lenses.push(new vscode.CodeLens(r, { title: `$(discard) Undo change (${what})`, command: 'task-x.review.undoHunk', arguments: [doc.uri, h], tooltip: 'Put back the original lines of this change only' }));
    }
    return lenses;
  }

  /** Replaces one change with its original lines, as a normal edit the user can undo with Ctrl+Z. */
  private async undoHunk(uri: vscode.Uri, h: Hunk): Promise<void> {
    const doc = await vscode.workspace.openTextDocument(uri);
    const eol = doc.eol === vscode.EndOfLine.CRLF ? '\r\n' : '\n';
    const edit = new vscode.WorkspaceEdit();
    const text = h.before.map((l) => l + eol).join('');
    if (h.count) {
      const end = h.start + h.count;
      const range = end < doc.lineCount ? new vscode.Range(h.start, 0, end, 0) : new vscode.Range(h.start, 0, doc.lineCount - 1, doc.lineAt(doc.lineCount - 1).text.length);
      edit.replace(uri, range, end < doc.lineCount ? text : text.replace(/\r?\n$/, ''));
    } else {
      edit.insert(uri, new vscode.Position(Math.min(h.start, doc.lineCount), 0), text);
    }
    if (await vscode.workspace.applyEdit(edit)) await doc.save();
  }

  dispose(): void {
    for (const s of this.subs) s.dispose();
  }
}
