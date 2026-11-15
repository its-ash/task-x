import assert from 'node:assert/strict';
import { test } from 'node:test';
import { portOrigin, portUrl, portWhere, portsStatusText, portsTooltip } from '../portsFormat.js';
import type { PortInfo } from '../protocol.js';

const vite: PortInfo = { port: 5173, addresses: ['*', '::'], pid: 501, name: 'node', command: 'node vite --port 5173', origin: 'taskX' };
const docs: PortInfo = { port: 8000, addresses: ['127.0.0.1'], pid: 77, name: 'python3', command: 'python3 -m http.server', origin: 'workspace', cwd: '/w/docs' };

test('describes ports for the status bar and the list', () => {
  assert.equal(portWhere(vite), 'every interface', 'wildcards collapse into one');
  assert.equal(portWhere(docs), '127.0.0.1');
  assert.equal(portUrl(vite), 'http://localhost:5173');
  assert.deepEqual([portOrigin(vite), portOrigin(docs)], ['started by task-x', 'runs in this workspace']);
  assert.equal(portsStatusText([vite]), '$(radio-tower) :5173');
  assert.equal(portsStatusText([vite, docs]), '$(radio-tower) 2 ports');
  assert.equal(portsStatusText([vite, { ...vite, pid: 502 }]), '$(radio-tower) :5173', 'one port, two processes');
  const tip = portsTooltip([vite, docs]);
  assert.ok(tip.includes('`5173` node (pid 501), started by task-x, on every interface'), tip);
  assert.ok(tip.includes('`8000` python3 (pid 77), runs in this workspace, on 127.0.0.1'), tip);
});
