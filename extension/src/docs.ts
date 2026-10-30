/**
 * Library docs for `@docs` (#docs indexing): "Add Docs…" crawls a docs site into the runtime's index
 * (`docs/add`, with progress), "Remove Docs…" drops one, and `@docs:<name>` in a message tells the
 * agent to search that set with `docs_search`.
 */
import * as vscode from 'vscode';
import type { JsonRpcClient } from './rpc.js';
import { embeddingsProvider } from './runParams.js';

const NAME = /^[\w.-]{1,64}$/;
const DOCS_MENTION = /(^|\s)@docs:([\w.-]{1,64})/g;

export interface DocsSource {
  name: string;
  url: string;
  pages: number;
  chunks: number;
  semantic: boolean;
}

export const listDocs = async (client: JsonRpcClient): Promise<DocsSource[]> => client.request<DocsSource[]>('docs/list').catch(() => []);

/** Asks for a start URL and a name, then indexes the site with a progress notification. */
export async function addDocs(client: JsonRpcClient, secrets: vscode.SecretStorage): Promise<DocsSource | undefined> {
  const url = (await vscode.window.showInputBox({
    title: 'Add docs (1/3)',
    prompt: 'Start URL of the documentation. Pages under this URL are indexed.',
    placeHolder: 'https://docs.rs/serde/latest/serde/',
    ignoreFocusOut: true,
    validateInput: (s) => (/^https?:\/\/[^\s/]+\.[^\s]+$/i.test(s.trim()) ? undefined : 'Type an http(s) URL.'),
  }))?.trim();
  if (!url) return undefined;
  const guess = new URL(url).pathname.split('/').filter(Boolean)[0] ?? new URL(url).hostname.split('.')[0];
  const name = (await vscode.window.showInputBox({
    title: 'Add docs (2/3)',
    prompt: 'Short name, used as @docs:<name> in the chat',
    value: guess.replace(/[^\w.-]/g, '-').slice(0, 64),
    ignoreFocusOut: true,
    validateInput: (s) => (NAME.test(s.trim()) ? undefined : 'Use letters, digits, ".", "_", or "-" (up to 64).'),
  }))?.trim();
  if (!name) return undefined;
  const pages = await vscode.window.showQuickPick(['20', '40', '100', '300'], { title: 'Add docs (3/3)', placeHolder: 'How many pages at most?', ignoreFocusOut: true });
  if (!pages) return undefined;
  const embeddings = await embeddingsProvider(secrets);
  return vscode.window.withProgress({ location: vscode.ProgressLocation.Notification, title: `task-x: indexing ${name}`, cancellable: true }, async (progress, token) => {
    const off = client.onNotification('docs/progress', (p) => {
      const { name: n, pages: done, total } = p as { name: string; pages: number; total: number };
      if (n === name) progress.report({ message: `${done} of about ${total} pages` });
    });
    const abort = new AbortController();
    token.onCancellationRequested(() => abort.abort());
    try {
      const r = await client.request<{ name: string; url: string; pages: number; chunks: number; semantic: boolean; skipped: number }>('docs/add', { name, url, maxPages: Number(pages), embeddings }, { signal: abort.signal });
      vscode.window.showInformationMessage(
        `task-x: indexed ${r.pages} pages of ${r.name} (${r.chunks} passages, ${r.semantic ? 'semantic search' : 'keyword search: set task-x.embeddings for semantic search'}${r.skipped ? `, ${r.skipped} pages skipped` : ''}). Use @docs:${r.name} in the chat.`,
      );
      return { name: r.name, url: r.url, pages: r.pages, chunks: r.chunks, semantic: r.semantic };
    } catch (e) {
      if (!token.isCancellationRequested) vscode.window.showErrorMessage(`task-x: could not index ${url}: ${e instanceof Error ? e.message : String(e)}`);
      return undefined;
    } finally {
      off();
    }
  });
}

export async function removeDocs(client: JsonRpcClient): Promise<void> {
  const list = await listDocs(client);
  if (!list.length) return void vscode.window.showInformationMessage('task-x: no docs are indexed. Use "task-x: Add Docs…".');
  const pick = await vscode.window.showQuickPick(
    list.map((d) => ({ label: d.name, description: d.url, detail: `${d.pages} pages, ${d.chunks} passages, ${d.semantic ? 'semantic' : 'keyword'} search` })),
    { title: 'Remove docs' },
  );
  if (pick) await client.request('docs/remove', { name: pick.label });
}

/** `@docs:<name>` items of a message. */
export const docsMentions = (text: string): string[] => [...new Set([...text.matchAll(DOCS_MENTION)].map((m) => m[2]))];

/** The message with a note for each `@docs:<name>`: search that docs set before answering. */
export function withDocs(text: string): string {
  const names = docsMentions(text);
  if (!names.length) return text;
  return `${text}\n\n<docs>Answer from the indexed library docs: call docs_search with source ${names.map((n) => `"${n}"`).join(' or ')} (one call per question you need answered), and cite the page URLs you use.</docs>`;
}
