// Drives the real Rust runtime through the extension's client against a local mock provider.
// Skipped when the runtime has not been built (run `cargo build` in runtime/ first).

import assert from 'node:assert/strict';
import { type Server, createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import path from 'node:path';
import { after, before, test } from 'node:test';
import type { ChatEventParams, ChatOutcome, ContextResult, ProviderConfig, TestReport, WorkspaceSummary } from '../protocol.js';
import { RpcError } from '../rpc.js';
import { RuntimeProcess, findRuntime } from '../runtime.js';

const extRoot = path.resolve(__dirname, '..', '..');
let binary: string | undefined;
try {
  binary = findRuntime(extRoot);
} catch {
  binary = undefined;
}

let server: Server;
let base = '';
let rt: RuntimeProcess;

const sse = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;

before(async () => {
  if (!binary) return;
  server = createServer((req, res) => {
    if (req.url === '/v1/models') {
      res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify({ data: [{ id: 'mock' }] }));
      return;
    }
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      const prompt = JSON.parse(body).messages.at(-1).content as string;
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      res.write(sse({ choices: [{ index: 0, delta: { content: 'echo: ' } }] }));
      if (prompt === 'hang') return; // Leave the stream open to exercise cancellation.
      res.write(sse({ choices: [{ index: 0, delta: { content: prompt }, finish_reason: 'stop' }] }));
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`;
  rt = (await RuntimeProcess.start(binary, { clientVersion: 'test', log: () => {}, onExit: () => {} })).runtime;
});

after(async () => {
  await rt?.stop();
  server?.closeAllConnections();
  server?.close();
});

const provider = (): ProviderConfig => ({ kind: 'openaiCompatible', model: 'mock', baseUrl: base, apiKey: 'k', timeoutMs: 5000 });
const chat = (text: string) => ({ provider: provider(), request: { messages: [{ role: 'user', content: [{ type: 'text', text }] }] } });

test('streams deltas and resolves the aggregated outcome', { skip: !binary && 'runtime not built' }, async () => {
  const deltas: string[] = [];
  const { id, result } = rt.client.start<ChatOutcome>('chat/send', chat('hello'));
  const off = rt.client.onNotification('chat/event', (p) => {
    const { requestId, event } = p as ChatEventParams;
    if (requestId === id && event.type === 'textDelta') deltas.push(event.text);
  });
  const out = await result;
  off();
  assert.equal(out.text, 'echo: hello');
  assert.deepEqual(deltas, ['echo: ', 'hello']);
  assert.equal(out.stopReason, 'endTurn');
});

test('abort cancels an in-flight stream', { skip: !binary && 'runtime not built' }, async () => {
  const ac = new AbortController();
  const { id, result } = rt.client.start<ChatOutcome>('chat/send', chat('hang'), { signal: ac.signal });
  rt.client.onNotification('chat/event', (p) => (p as ChatEventParams).requestId === id && ac.abort());
  await assert.rejects(result, (e: unknown) => e instanceof RpcError && e.cancelled);
});

test('provider/test reports reachability and model availability', { skip: !binary && 'runtime not built' }, async () => {
  const r = await rt.client.request<TestReport>('provider/test', { provider: provider() });
  assert.equal(r.modelAvailable, true);
  assert.deepEqual(r.models, ['mock']);
});

test('switching provider kind needs no runtime change', { skip: !binary && 'runtime not built' }, async () => {
  await assert.rejects(
    rt.client.request('chat/send', { ...chat('x'), provider: { kind: 'anthropic', model: 'm' } }),
    (e: unknown) => e instanceof RpcError && (e.data as { kind: string }).kind === 'auth',
  );
});

test('indexes a workspace and retrieves relevant snippets with instructions', { skip: !binary && 'runtime not built' }, async () => {
  const { mkdtempSync, writeFileSync, mkdirSync, rmSync, realpathSync } = await import('node:fs');
  const { tmpdir } = await import('node:os');
  const root = realpathSync(mkdtempSync(path.join(tmpdir(), 'anvil-e2e-')));
  try {
    mkdirSync(path.join(root, 'src'));
    writeFileSync(path.join(root, 'CLAUDE.md'), 'Use tabs.');
    writeFileSync(path.join(root, 'AGENTS.md'), 'Not sent.');
    writeFileSync(path.join(root, '.env'), 'TOKEN=secret');
    writeFileSync(path.join(root, 'package.json'), JSON.stringify({ scripts: { test: 'node --test' }, devDependencies: { typescript: '5' } }));
    writeFileSync(path.join(root, 'src', 'billing.ts'), 'export function computeInvoiceTotal(items: number[]) {\n  return items.reduce((a, b) => a + b, 0);\n}\n');
    writeFileSync(path.join(root, 'src', 'other.ts'), 'export const unrelated = 1;\n');
    const s = await rt.client.request<WorkspaceSummary>('workspace/open', { root });
    assert.deepEqual(s.project.packageManagers, ['npm']);
    assert.deepEqual(s.instructions.map((i) => i.source), ['CLAUDE.md'], 'only CLAUDE.md is loaded');

    const r = await rt.client.request<ContextResult>('context/retrieve', { query: 'computeInvoiceTotal returns NaN', focusPaths: [] });
    assert.equal(r.retrieval.snippets[0].path, 'src/billing.ts');
    assert.equal(r.instructions.at(-1)?.content, 'Use tabs.', 'workspace instructions have the highest precedence');
    assert.ok(!JSON.stringify(r).includes('TOKEN=secret'));

    writeFileSync(path.join(root, 'src', 'tax.ts'), 'export function applySalesTax(x: number) { return x * 1.2; }\n');
    rt.client.notify('workspace/filesChanged', { paths: [path.join(root, 'src', 'tax.ts')] });
    await new Promise((res) => setTimeout(res, 200));
    const r2 = await rt.client.request<ContextResult>('context/retrieve', { query: 'applySalesTax' });
    assert.equal(r2.retrieval.snippets[0].path, 'src/tax.ts');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});
