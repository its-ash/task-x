// Agent loop through the real runtime: a mock model edits a file, the client approves the edit.
import assert from 'node:assert/strict';
import { mkdirSync, mkdtempSync, readFileSync, realpathSync, rmSync, writeFileSync } from 'node:fs';
import { type Server, createServer } from 'node:http';
import type { AddressInfo } from 'node:net';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { after, before, test } from 'node:test';
import type { AgentEventParams, AgentSummary, ApprovalRequest, AskUserRequest, FileDependencies, GraphView, ResolvedSkill } from '../protocol.js';
import { RuntimeProcess, findRuntime } from '../runtime.js';

let binary: string | undefined;
try {
  binary = findRuntime(path.resolve(__dirname, '..', '..'));
} catch {
  binary = undefined;
}

let server: Server;
let base = '';
let rt: RuntimeProcess;
let root = '';
const sse = (o: unknown) => `data: ${JSON.stringify(o)}\n\n`;

before(async () => {
  if (!binary) return;
  root = realpathSync(mkdtempSync(path.join(tmpdir(), 'anvil-agent-')));
  writeFileSync(path.join(root, 'app.txt'), 'color = red\n');
  mkdirSync(path.join(root, '.anvil', 'skills'), { recursive: true });
  writeFileSync(path.join(root, '.anvil', 'skills', 'anvil-test-greet.md'), '---\nname: anvil-test-greet\ndescription: Greet\ntriggers: say hello\nparams: who\n---\nGreet {{who}} warmly.\n');
  server = createServer((req, res) => {
    // The model list is not mocked; the runtime then uses its own window estimate.
    if (req.method === 'GET') return void res.writeHead(404).end();
    let body = '';
    req.on('data', (c) => (body += c));
    req.on('end', () => {
      const msgs = JSON.parse(body).messages as { role: string; content: string }[];
      const toolResults = msgs.filter((m) => m.role === 'tool').length;
      res.writeHead(200, { 'content-type': 'text/event-stream' });
      const asking = msgs.some((m) => m.role === 'user' && JSON.stringify(m.content).includes('ASK_TEST'));
      if (asking && toolResults === 0) {
        const args = JSON.stringify({ questions: [{ question: 'Which indent?', type: 'radio', options: ['tabs', 'spaces'] }] });
        res.write(sse({ choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'call_ask', function: { name: 'ask_user', arguments: args } }] }, finish_reason: 'tool_calls' }] }));
      } else if (asking) {
        res.write(sse({ choices: [{ index: 0, delta: { content: 'Using tabs.' }, finish_reason: 'stop' }] }));
      } else if (toolResults === 0) {
        const args = JSON.stringify({ path: 'app.txt', oldText: 'red', newText: 'blue' });
        res.write(sse({ choices: [{ index: 0, delta: { tool_calls: [{ index: 0, id: 'call_1', function: { name: 'replace_text', arguments: args } }] }, finish_reason: 'tool_calls' }] }));
      } else {
        res.write(sse({ choices: [{ index: 0, delta: { content: 'Changed the color to blue.' }, finish_reason: 'stop' }] }));
      }
      res.end('data: [DONE]\n\n');
    });
  });
  await new Promise<void>((r) => server.listen(0, '127.0.0.1', r));
  base = `http://127.0.0.1:${(server.address() as AddressInfo).port}/v1`;
  rt = (await RuntimeProcess.start(binary, { clientVersion: 'test', dataDir: path.join(root, '.data'), log: () => {}, onExit: () => {} })).runtime;
  await rt.client.request('workspace/open', { root });
});

after(async () => {
  await rt?.stop();
  server?.closeAllConnections();
  server?.close();
  if (root) rmSync(root, { recursive: true, force: true });
});

const run = (chatMode: string, taskId: string) =>
  rt.client.request<AgentSummary>('agent/run', {
    prompt: 'Make the color blue',
    taskId,
    chatMode,
    provider: { kind: 'openaiCompatible', model: 'mock', baseUrl: base, apiKey: 'k', timeoutMs: 5000 },
    policy: { mode: 'askBeforeEdit' },
  });

test('agent mode edits after approval and reports no validation', { skip: !binary && 'runtime not built' }, async () => {
  const approvals: ApprovalRequest[] = [];
  const types: string[] = [];
  rt.client.onRequest('agent/requestApproval', async (p) => {
    approvals.push(p as ApprovalRequest);
    return { decision: 'allow' };
  });
  const off = rt.client.onNotification('agent/event', (p) => types.push((p as AgentEventParams).event.type));
  const s = await run('agent', 'e2e1');
  off();
  assert.equal(s.status, 'completed');
  assert.deepEqual(s.changedFiles, ['app.txt']);
  assert.equal(s.validationRan, false);
  assert.equal(readFileSync(path.join(root, 'app.txt'), 'utf8'), 'color = blue\n');
  assert.equal(approvals[0].tool, 'replace_text');
  assert.match(approvals[0].preview ?? '', /\+ blue/);
  for (const t of ['started', 'toolStarted', 'fileChanged', 'toolFinished', 'done']) assert.ok(types.includes(t), `missing ${t}`);

  const undo = await rt.client.request<{ restored: string[] }>('edits/undo', { operation: s.operations[0] });
  assert.deepEqual(undo.restored, ['app.txt']);
  assert.equal(readFileSync(path.join(root, 'app.txt'), 'utf8'), 'color = red\n');
  const tasks = await rt.client.request<{ id: string }[]>('agent/tasks');
  assert.ok(tasks.some((t) => t.id === 'e2e1'));
});

test('agent asks the user a question and continues with the answer', { skip: !binary && 'runtime not built' }, async () => {
  const asked: AskUserRequest[] = [];
  rt.client.onRequest('agent/askUser', async (p) => {
    asked.push(p as AskUserRequest);
    return { answers: { q1: ['tabs'] } };
  });
  const s = await rt.client.request<AgentSummary>('agent/run', {
    prompt: 'ASK_TEST pick an indent style',
    taskId: 'e2eAsk',
    chatMode: 'agent',
    provider: { kind: 'openaiCompatible', model: 'mock', baseUrl: base, apiKey: 'k', timeoutMs: 5000 },
    policy: { mode: 'askBeforeEdit' },
  });
  assert.equal(s.status, 'completed');
  assert.equal(s.text, 'Using tabs.');
  assert.equal(asked.length, 1);
  assert.deepEqual(asked[0].questions, [{ id: 'q1', text: 'Which indent?', kind: 'radio', options: ['tabs', 'spaces'] }]);
});

test('review panel: pending changes, then undo all restores the original', { skip: !binary && 'runtime not built' }, async () => {
  rt.client.onRequest('agent/requestApproval', async () => ({ decision: 'allow' }));
  await run('agent', 'e2e3');
  const pending = await rt.client.request<{ path: string; added: number; removed: number; original: string }[]>('edits/pending');
  assert.deepEqual(pending.map((p) => [p.path, p.added, p.removed, p.original]), [['app.txt', 1, 1, 'color = red\n']]);
  const r = await rt.client.request<{ reverted: string[] }>('edits/revert', {});
  assert.deepEqual(r.reverted, ['app.txt']);
  assert.equal(readFileSync(path.join(root, 'app.txt'), 'utf8'), 'color = red\n');
  assert.deepEqual(await rt.client.request('edits/pending'), []);
});

test('ask mode never edits', { skip: !binary && 'runtime not built' }, async () => {
  const s = await run('ask', 'e2e2');
  assert.equal(s.status, 'completed');
  assert.deepEqual(s.changedFiles, []);
  assert.equal(readFileSync(path.join(root, 'app.txt'), 'utf8'), 'color = red\n');
});

test('skills/resolve expands a /name skill and a trigger skill', { skip: !binary && 'runtime not built' }, async () => {
  const resolve = async (prompt: string) => (await rt.client.request<{ active: ResolvedSkill[] }>('skills/resolve', { prompt })).active.filter((s) => s.name === 'anvil-test-greet');
  const [byName] = await resolve('/anvil-test-greet who=Ada now');
  assert.match(byName.reason, /^requested/);
  assert.equal(byName.text.trim(), 'Greet Ada warmly.');
  assert.ok(byName.source.endsWith(path.join('.anvil', 'skills', 'anvil-test-greet.md')));
  assert.equal((await resolve('please say hello'))[0]?.reason, "mentions 'say hello'");
  assert.equal((await resolve('rename a variable')).length, 0);
  await assert.rejects(rt.client.request('skills/resolve', {}));
});

test('checkpoints survive a runtime restart, and team methods validate input', { skip: !binary && 'runtime not built' }, async () => {
  rt.client.onRequest('agent/requestApproval', async () => ({ decision: 'allow' }));
  const s = await run('agent', 'e2e4');
  assert.equal(readFileSync(path.join(root, 'app.txt'), 'utf8'), 'color = blue\n');
  await rt.stop();
  rt = (await RuntimeProcess.start(binary!, { clientVersion: 'test', dataDir: path.join(root, '.data'), log: () => {}, onExit: () => {} })).runtime;
  await rt.client.request('workspace/open', { root });
  const pending = await rt.client.request<{ path: string }[]>('edits/pending');
  assert.deepEqual(pending.map((p) => p.path), ['app.txt']);
  await rt.client.request('edits/undo', { operation: s.operations[0] });
  assert.equal(readFileSync(path.join(root, 'app.txt'), 'utf8'), 'color = red\n');
  assert.equal(await rt.client.request('agent/cancelSubagent', { taskId: 'none' }), false);
  await assert.rejects(rt.client.request('worktrees/discard', { name: '../x' }));
});

test('re-indexing reports progress, and the project graph is served', { skip: !binary && 'runtime not built' }, async () => {
  writeFileSync(path.join(root, 'main.ts'), "import { helper } from './util';\nhelper();\n");
  writeFileSync(path.join(root, 'util.ts'), 'export function helper() {}\n');
  const phases: string[] = [];
  const off = rt.client.onNotification('workspace/progress', (p) => phases.push((p as { phase: string }).phase));
  await rt.client.request('workspace/refresh');
  off();
  assert.equal(phases[0], 'indexing');
  assert.equal(phases.at(-1), 'ready');
  const g = await rt.client.request<GraphView>('graph/project', { level: 'file' });
  const ids = g.nodes.map((n) => n.id);
  assert.ok(ids.includes('main.ts') && ids.includes('util.ts'));
  assert.ok(g.edges.some((e) => ids[e.from] === 'main.ts' && ids[e.to] === 'util.ts' && e.kind === 'import'));
  const deps = await rt.client.request<FileDependencies>('graph/file', { path: 'util.ts' });
  assert.deepEqual(deps.usedBy.map((d) => d.path), ['main.ts']);
  await assert.rejects(rt.client.request('graph/file', { path: 'missing.ts' }));
});
