/**
 * The language-model tools other VS Code extensions register (`vscode.lm.tools`), served to the agent
 * as one MCP server named `vscode` (#14). It listens on 127.0.0.1 behind a random bearer token and
 * answers MCP over HTTP with JSON (initialize, tools/list, tools/call). Calls go through the agent's
 * approval like any MCP tool. VS Code's browser tools stay with task-x's own `browser` tool.
 */
import { randomBytes, timingSafeEqual } from 'node:crypto';
import { type IncomingMessage, type ServerResponse, createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import * as vscode from 'vscode';
import { BROWSER_TOOLS } from './vscodeBrowser.js';

export const LM_SERVER = 'vscode';
const MAX_BODY = 1 << 20;
const MAX_TOOLS = 128;
/** Tools task-x has itself, or that only make sense inside Copilot Chat. */
const SKIP = /^(copilot_|vscode_editFile|vscode_runInTerminal)/;

export interface LmToolsHost {
  url: string;
  token: string;
  dispose(): void;
}

/** MCP tool names allow letters, digits, `_`, and `-`; VS Code names can have more. */
const mcpName = (n: string) => n.replace(/[^\w-]/g, '_').slice(0, 64);

function tools(): vscode.LanguageModelToolInformation[] {
  return vscode.lm.tools.filter((t) => !BROWSER_TOOLS.has(t.name) && !SKIP.test(t.name)).slice(0, MAX_TOOLS);
}

function text(parts: readonly unknown[]): string {
  return parts
    .map((p) => (p instanceof vscode.LanguageModelTextPart ? p.value : p instanceof vscode.LanguageModelPromptTsxPart ? JSON.stringify(p.value) : ''))
    .filter(Boolean)
    .join('\n');
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

async function answer(msg: { id?: unknown; method?: unknown; params?: unknown }): Promise<unknown> {
  const p = (msg.params ?? {}) as { name?: unknown; arguments?: unknown };
  switch (msg.method) {
    case 'initialize':
      return { protocolVersion: '2025-06-18', capabilities: { tools: { listChanged: false } }, serverInfo: { name: 'task-x vscode tools', version: '1' } };
    case 'tools/list':
      return { tools: tools().map((t) => ({ name: mcpName(t.name), description: t.description, inputSchema: t.inputSchema ?? { type: 'object', properties: {} } })) };
    case 'tools/call': {
      const t = tools().find((x) => mcpName(x.name) === p.name);
      if (!t) return { isError: true, content: [{ type: 'text', text: `unknown tool ${String(p.name)}` }] };
      try {
        const cts = new vscode.CancellationTokenSource();
        const r = await vscode.lm.invokeTool(t.name, { input: (p.arguments ?? {}) as object, toolInvocationToken: undefined }, cts.token);
        return { content: [{ type: 'text', text: text(r.content) || '(no output)' }] };
      } catch (e) {
        return { isError: true, content: [{ type: 'text', text: e instanceof Error ? e.message : String(e) }] };
      }
    }
    case 'ping':
      return {};
    default:
      return undefined;
  }
}

function send(res: ServerResponse, status: number, body?: unknown): void {
  res.writeHead(status, body === undefined ? {} : { 'content-type': 'application/json', 'mcp-session-id': 'vscode' });
  res.end(body === undefined ? undefined : JSON.stringify(body));
}

export async function startLmToolsHost(log: (line: string) => void): Promise<LmToolsHost> {
  const token = randomBytes(32).toString('hex');
  const expected = Buffer.from(`Bearer ${token}`);
  const server = createServer(async (req, res) => {
    const auth = Buffer.from(req.headers.authorization ?? '');
    if (auth.length !== expected.length || !timingSafeEqual(auth, expected)) return send(res, 401, { error: 'unauthorized' });
    if (req.url !== '/mcp') return send(res, 404, { error: 'not found' });
    if (req.method === 'DELETE') return send(res, 200);
    if (req.method !== 'POST') return send(res, 405);
    try {
      const msg = JSON.parse(await readBody(req)) as { id?: unknown; method?: unknown; params?: unknown };
      // A notification gets no body.
      if (msg.id === undefined) return send(res, 202);
      const result = await answer(msg);
      send(res, 200, result === undefined ? { jsonrpc: '2.0', id: msg.id, error: { code: -32601, message: `unknown method ${String(msg.method)}` } } : { jsonrpc: '2.0', id: msg.id, result });
    } catch (e) {
      log(`vscode tools: ${e instanceof Error ? e.message : String(e)}`);
      send(res, 400, { jsonrpc: '2.0', id: null, error: { code: -32700, message: 'bad request' } });
    }
  });
  await new Promise<void>((resolve, reject) => {
    server.once('error', reject);
    server.listen(0, '127.0.0.1', () => resolve());
  });
  const { port } = server.address() as AddressInfo;
  return { url: `http://127.0.0.1:${port}/mcp`, token, dispose: () => server.close() };
}

/** The server entry for `mcp/configure` and Claude Code, when other extensions offer tools. */
export function lmToolsServer(host: LmToolsHost | undefined): Record<string, { type: string; url: string; headers: Record<string, string> }> {
  if (!host || !tools().length) return {};
  return { [LM_SERVER]: { type: 'http', url: host.url, headers: { Authorization: `Bearer ${host.token}` } } };
}
