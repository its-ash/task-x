import assert from 'node:assert/strict';
import { test } from 'node:test';
import type { ProcInfo } from '../protocol.js';
import { duration, endLine, groupProcs, procDescription, shortCommand, toTerminal } from '../terminalsFormat.js';

const dev: ProcInfo = { id: 'p1', taskId: 't1', command: 'npm run dev', cwd: 'web', pid: 10, startedMs: 1_000, status: 'running' };
const test1: ProcInfo = { id: 'p2', taskId: 't1-sub1', command: 'cargo test', cwd: '.', pid: 11, startedMs: 5_000, endedMs: 65_000, status: 'failed', exitCode: 101 };
const other: ProcInfo = { id: 'p3', command: 'make', cwd: '.', pid: 12, startedMs: 3_000, endedMs: 4_000, status: 'exited', exitCode: 0 };

test('describes commands for the Terminals view', () => {
  assert.equal(duration(65_000), '1:05');
  assert.equal(duration(3_725_000), '1:02:05');
  assert.equal(procDescription(dev, 13_000), 'running · 0:12 · web');
  assert.equal(procDescription(test1), 'exit 101 · 1:00', 'a failure shows its exit code; the workspace root is not named');
  assert.equal(shortCommand('a'.repeat(80), 10), `${'a'.repeat(9)}…`);
  assert.equal(shortCommand('set -e\nmake'), 'set -e …', 'a script shows its first line');
});

test('writes terminal text', () => {
  assert.equal(toTerminal('a\nb\r\nc'), 'a\r\nb\r\nc', 'every line starts at the left edge, once');
  assert.match(endLine(test1), /failed with exit code 101.*after 1:00/);
  assert.match(endLine(other), /done in 0:01/);
  assert.equal(endLine(dev), '', 'a running command has no end line');
});

test('groups commands by chat, newest first', () => {
  const titleOf = (id: string) => (id === 't1' || id.startsWith('t1-') ? 'Fix the build' : undefined);
  const groups = groupProcs([dev, test1, other], titleOf);
  assert.deepEqual(
    groups.map((g) => [g.title, g.procs.map((p) => p.id)]),
    [
      ['Fix the build', ['p2', 'p1']],
      ['Other', ['p3']],
    ],
  );
});
