import assert from 'node:assert/strict';
import { PassThrough } from 'node:stream';
import { test } from 'node:test';
import { ErrorCode } from '../protocol.js';
import { JsonRpcClient, RpcError } from '../rpc.js';

type Msg = { id?: number; method?: string; params?: unknown };

function harness() {
  const toClient = new PassThrough();
  const fromClient = new PassThrough();
  const errors: string[] = [];
  const client = new JsonRpcClient(toClient, fromClient, (e) => errors.push(e));
  const sent: Msg[] = [];
  const queue: Msg[] = [];
  const waiters: ((m: Msg) => void)[] = [];
  let buf = '';
  fromClient.setEncoding('utf8');
  fromClient.on('data', (d: string) => {
    buf += d;
    for (let i; (i = buf.indexOf('\n')) >= 0; buf = buf.slice(i + 1)) {
      const m = JSON.parse(buf.slice(0, i)) as Msg;
      sent.push(m);
      const w = waiters.shift();
      if (w) w(m);
      else queue.push(m);
    }
  });
  const next = () => new Promise<Msg>((r) => (queue.length ? r(queue.shift()!) : waiters.push(r)));
  const reply = (o: object) => toClient.write(JSON.stringify({ jsonrpc: '2.0', ...o }) + '\n');
  return { client, toClient, sent, next, reply, errors };
}

test('resolves responses by id even when they arrive out of order', async () => {
  const h = harness();
  const a = h.client.request<string>('a');
  const b = h.client.request<string>('b');
  await h.next();
  await h.next();
  h.reply({ id: 2, result: 'B' });
  h.reply({ id: 1, result: 'A' });
  assert.deepEqual(await Promise.all([a, b]), ['A', 'B']);
  assert.deepEqual(h.sent.map((m) => m.method), ['a', 'b']);
});

test('reassembles messages split across chunks and dispatches notifications', async () => {
  const h = harness();
  const seen: unknown[] = [];
  h.client.onNotification('chat/event', (p) => seen.push(p));
  const line = JSON.stringify({ jsonrpc: '2.0', method: 'chat/event', params: { requestId: 1, event: { type: 'textDelta', text: 'hé' } } }) + '\n';
  h.toClient.write(line.slice(0, 10));
  h.toClient.write(line.slice(10));
  await new Promise((r) => setImmediate(r));
  assert.deepEqual(seen, [{ requestId: 1, event: { type: 'textDelta', text: 'hé' } }]);
});

test('maps error responses to RpcError with data', async () => {
  const h = harness();
  const p = h.client.request('chat/send');
  const m = await h.next();
  h.reply({ id: m.id, error: { code: ErrorCode.Provider, message: 'bad key', data: { kind: 'auth', retryable: false } } });
  await assert.rejects(p, (e: unknown) => e instanceof RpcError && e.code === ErrorCode.Provider && (e.data as { kind: string }).kind === 'auth');
});

test('abort sends $/cancelRequest and the runtime reply rejects as cancelled', async () => {
  const h = harness();
  const ac = new AbortController();
  const p = h.client.request('chat/send', {}, { signal: ac.signal });
  const req = await h.next();
  ac.abort();
  const cancel = await h.next();
  assert.deepEqual(cancel, { jsonrpc: '2.0', method: '$/cancelRequest', params: { id: req.id } });
  h.reply({ id: req.id, error: { code: ErrorCode.RequestCancelled, message: 'request cancelled' } });
  await assert.rejects(p, (e: unknown) => e instanceof RpcError && e.cancelled);
});

test('timeouts reject locally and notify the runtime', async () => {
  const h = harness();
  const p = h.client.request('slow', undefined, { timeoutMs: 20 });
  await h.next();
  await assert.rejects(p, /timed out/);
  assert.equal((await h.next()).method, '$/cancelRequest');
});

test('stream end rejects pending requests and later calls fail fast', async () => {
  const h = harness();
  const p = h.client.request('x');
  await h.next();
  h.toClient.end();
  await assert.rejects(p, /closed/);
  assert.ok(h.client.isClosed);
  await assert.rejects(h.client.request('y'), /closed/);
});

test('invalid JSON is reported without breaking the stream', async () => {
  const h = harness();
  const p = h.client.request<number>('x');
  await h.next();
  h.toClient.write('{oops\n');
  h.reply({ id: 1, result: 42 });
  assert.equal(await p, 42);
  assert.deepEqual(h.errors, ['runtime sent invalid JSON']);
});

test('answers runtime-to-client requests', async () => {
  const h = harness();
  h.client.onRequest('agent/requestApproval', async (p) => ({ decision: (p as { tool: string }).tool === 'read_file' ? 'allow' : 'deny' }));
  h.reply({ id: 'srv-1', method: 'agent/requestApproval', params: { tool: 'delete_file' } });
  assert.deepEqual(await h.next(), { jsonrpc: '2.0', id: 'srv-1', result: { decision: 'deny' } });
  h.reply({ id: 'srv-2', method: 'unknown/method' });
  assert.equal(((await h.next()) as { error: { code: number } }).error.code, ErrorCode.MethodNotFound);
});
