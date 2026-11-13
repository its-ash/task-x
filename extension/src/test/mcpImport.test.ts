import assert from 'node:assert/strict';
import { join } from 'node:path';
import { test } from 'node:test';
import { expand, forClaude, importServers, normalize, type ImportOptions } from '../mcpImport.js';
import { imagesFrom } from '../protocol.js';

const home = '/home/me';
const root = '/work/app';
const files: Record<string, unknown> = {
  [join(home, 'Library', 'Application Support', 'Claude', 'claude_desktop_config.json')]: { mcpServers: { chrome: { command: 'npx', args: ['chrome-devtools-mcp@latest'] } } },
  [join(home, '.claude.json')]: {
    mcpServers: { figma: { type: 'http', url: 'http://127.0.0.1:3845/mcp' } },
    projects: { [root]: { mcpServers: { db: { command: 'db-mcp', env: { URL: '${DB_URL:-postgres://localhost}' } } } }, '/other': { mcpServers: { nope: { command: 'x' } } } },
  },
  [join(home, '.copilot', 'mcp-config.json')]: { mcpServers: { github: { type: 'http', url: 'https://api.githubcopilot.com/mcp/', headers: { Authorization: 'Bearer ${GH_TOKEN}' } } } },
  '/vscode/User/mcp.json': '{\n  // Copilot servers\n  "servers": { "playwright": { "type": "stdio", "command": "npx", "args": ["@playwright/mcp@latest"], }, "secret": { "command": "s", "env": { "KEY": "${input:key}" } } },\n  "inputs": []\n}',
  [join(root, '.vscode', 'mcp.json')]: { servers: { local: { command: 'node', args: ['${workspaceFolder}/mcp.js'] } } },
  [join(root, '.mcp.json')]: { mcpServers: { chrome: { command: 'chrome-mcp-project' } } },
};

const opts = (o: Partial<ImportOptions> = {}): ImportOptions => ({
  home,
  platform: 'darwin',
  vscodeUserDir: '/vscode/User',
  root,
  trusted: true,
  sources: ['claude', 'vscode', 'copilotCli'],
  env: { GH_TOKEN: 'ghp_1' },
  read: (p) => (p in files ? (typeof files[p] === 'string' ? (files[p] as string) : JSON.stringify(files[p])) : undefined),
  ...o,
});

test('imports the servers of Claude, Copilot, and VS Code', () => {
  const r = importServers(opts());
  assert.deepEqual(Object.keys(r.servers).sort(), ['chrome', 'db', 'figma', 'github', 'local', 'playwright']);
  assert.equal(r.servers.chrome.command, 'chrome-mcp-project', 'the workspace file overrides the user file');
  assert.equal(r.servers.db.env?.URL, 'postgres://localhost', 'Claude ${VAR:-default}');
  assert.equal(r.servers.github.headers?.Authorization, 'Bearer ghp_1');
  assert.equal(r.servers.local.args?.[0], `${root}/mcp.js`);
  assert.equal(r.servers.figma.url, 'http://127.0.0.1:3845/mcp');
  assert.ok(!('nope' in r.servers), 'another project of ~/.claude.json is not used');
  assert.ok(r.notes.some((n) => n.includes('"secret"') && n.includes('${input:')), 'a server that needs an input is skipped with a reason');
  assert.deepEqual(Object.keys(r.nonClaude).sort(), ['github', 'local', 'playwright'], 'Claude Code loads the Claude servers itself');
});

test('workspace files need trust, and sources can be turned off', () => {
  const untrusted = importServers(opts({ trusted: false }));
  assert.equal(untrusted.servers.chrome.command, 'npx');
  assert.ok(!('local' in untrusted.servers));
  assert.deepEqual(Object.keys(importServers(opts({ sources: ['copilotCli'] })).servers), ['github']);
  assert.ok(importServers(opts({ vscodeSettingServers: { old: { command: 'old-mcp' } } })).servers.old);
  const broken = importServers(opts({ read: (p) => (p.endsWith('.claude.json') ? '{ nope' : undefined) }));
  assert.ok(broken.notes.some((n) => n.startsWith('Claude Code: could not parse')));
});

test('normalizes one server entry', () => {
  const o = { home, root, env: { A: '1' } };
  assert.equal(normalize({ args: [] }, o), 'no command or url');
  assert.equal(normalize({ type: 'ws', url: 'ws://x' }, o), 'unsupported type "ws"');
  assert.deepEqual(normalize({ type: 'streamable-http', serverUrl: 'https://x.dev/mcp' }, o), { type: 'http', url: 'https://x.dev/mcp' });
  assert.equal(expand('${userHome}/x:${env:A}:${B}', o), '/home/me/x:1:');
  assert.equal(expand('${input:token}', o), undefined);
});

test('passes servers to Claude Code in its own format', () => {
  const c = forClaude({ a: { command: 'x', cwd: '/tmp', timeoutMs: 5 }, b: { url: 'https://x.dev/mcp' }, off: { command: 'y', disabled: true } });
  assert.deepEqual(c, { a: { command: 'x', type: 'stdio' }, b: { url: 'https://x.dev/mcp', type: 'http' } });
});

test('keeps only valid pasted images', () => {
  const ok = { mediaType: 'image/png', data: 'iVBORw0KGgo=' };
  assert.deepEqual(imagesFrom([ok, { mediaType: 'image/svg+xml', data: 'PHN2Zz4=' }, { mediaType: 'image/png', data: 'data:x;base64,AA' }, null, 'x']), [ok]);
  assert.equal(imagesFrom(Array(12).fill(ok)).length, 8);
  assert.deepEqual(imagesFrom(undefined), []);
});
