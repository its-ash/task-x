/**
 * Loopback bridge from the runtime's `browser` tool to VS Code's integrated browser.
 *
 * VS Code drives its integrated browser through built-in language model tools (`open_browser_page`,
 * `read_page`, `click_element`, ...). Only the extension host can call them, so this serves
 * `POST /invoke { tool, input }` on 127.0.0.1 behind a random bearer token. The runtime finds it
 * through TASK_X_VSCODE_BROWSER and TASK_X_VSCODE_BROWSER_TOKEN. No debug port and no restart needed.
 */
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { type IncomingMessage, type ServerResponse, createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import * as vscode from 'vscode';

/** VS Code's browser tools; nothing else can be called through the bridge. */
export const BROWSER_TOOLS = new Set([
  'list_browser_pages',
  'open_browser_page',
  'navigate_page',
  'read_page',
  'click_element',
  'hover_element',
  'drag_element',
  'type_in_page',
  'handle_dialog',
  'screenshot_page',
  'run_playwright_code',
]);
const MAX_BODY = 1 << 20;
const UNAVAILABLE =
  "VS Code's integrated browser tools are not available: use VS Code 1.110 or later and turn on the setting workbench.browser.enableChatTools";

export interface BrowserHost {
  url: string;
  token: string;
  dispose(): void;
}

export async function startBrowserHost(log: (line: string) => void): Promise<BrowserHost> {
  const token = randomBytes(32).toString('hex');
  const expected = Buffer.from(`Bearer ${token}`);
  const server = createServer((req, res) => void handle(req, res, expected, log));
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => resolve());
  });
  const { port } = server.address() as AddressInfo;
  return { url: `http://127.0.0.1:${port}/`, token, dispose: () => server.close() };
}

function send(res: ServerResponse, status: number, body: unknown): void {
  res.writeHead(status, { 'content-type': 'application/json' });
  res.end(JSON.stringify(body));
}

async function readBody(req: IncomingMessage): Promise<string> {
  const chunks: Buffer[] = [];
  let size = 0;
  for await (const c of req as AsyncIterable<Buffer>) {
    size += c.length;
    if (size > MAX_BODY) throw new Error('the request is too large');
    chunks.push(c);
  }
  return Buffer.concat(chunks).toString('utf8');
}

function part(p: unknown): Record<string, unknown> {
  if (p instanceof vscode.LanguageModelTextPart) return { type: 'text', text: p.value };
  const d = p as { mimeType?: unknown; data?: unknown };
  return typeof d.mimeType === 'string' && d.data instanceof Uint8Array
    ? { type: d.mimeType.startsWith('image/') ? 'image' : 'data', mimeType: d.mimeType, base64: Buffer.from(d.data).toString('base64') }
    : { type: 'unknown' };
}

async function handle(req: IncomingMessage, res: ServerResponse, expected: Buffer, log: (line: string) => void): Promise<void> {
  const auth = Buffer.from(req.headers.authorization ?? '');
  if (auth.length !== expected.length || !timingSafeEqual(auth, expected)) return send(res, 401, { error: 'unauthorized' });
  if (req.method !== 'POST' || req.url !== '/invoke') return send(res, 404, { error: 'not found' });
  let body: { tool?: unknown; input?: unknown };
  try {
    body = JSON.parse(await readBody(req));
  } catch (e) {
    return send(res, 400, { error: `the body must be JSON { tool, input }: ${e instanceof Error ? e.message : String(e)}` });
  }
  const { tool, input } = body ?? {};
  if (typeof tool !== 'string' || !BROWSER_TOOLS.has(tool)) return send(res, 400, { error: `unsupported tool ${String(tool)}` });
  if (typeof input !== 'object' || input === null || Array.isArray(input)) return send(res, 400, { error: 'input must be an object' });
  if (!vscode.lm.tools.some((t) => t.name === tool)) return send(res, 503, { error: UNAVAILABLE });
  const cts = new vscode.CancellationTokenSource();
  res.on('close', () => res.writableEnded || cts.cancel());
  try {
    const r = await vscode.lm.invokeTool(tool, { input, toolInvocationToken: undefined }, cts.token);
    send(res, 200, { content: r.content.map(part) });
  } catch (e) {
    const msg = e instanceof Error ? e.message : String(e);
    log(`browser bridge: ${tool} failed: ${msg}`);
    send(res, 500, { error: msg });
  } finally {
    cts.dispose();
  }
}
