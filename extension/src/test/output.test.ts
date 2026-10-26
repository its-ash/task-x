import assert from 'node:assert/strict';
import { test } from 'node:test';
import { diffStats, formatOutput } from '../output.js';

test('formats command results like a terminal transcript', () => {
  const raw = JSON.stringify({ result: { argv: ['npm', 'run', 'test', 'my file'], cwd: 'web', stdout: 'ok\n', stderr: 'warn\n', exitCode: 1, durationMs: 42, sandboxed: true } });
  assert.equal(formatOutput(raw), "$ npm run test 'my file'    # in web\nok\nwarn\n[exit 1, 42 ms, sandboxed]");
});

test('formats errors, diffs, plain text, and other JSON', () => {
  assert.equal(formatOutput(JSON.stringify({ error: { kind: 'timeout', message: 'too slow' } })), 'timeout: too slow');
  assert.equal(formatOutput(JSON.stringify({ diff: '+a' })), '+a');
  assert.equal(formatOutput('plain output'), 'plain output');
  assert.equal(formatOutput(JSON.stringify({ n: 1 })), '{\n  "n": 1\n}');
  assert.match(formatOutput(JSON.stringify({ result: { argv: ['x'], timedOut: true, durationMs: 5 } })), /\[timed out, 5 ms\]$/);
});

test('counts added and removed lines of a unified diff', () => {
  const diff = '--- a/a.txt\n+++ b/a.txt\n@@ -1,3 +1,2 @@\n-old\n-older\n+new\n keep\n';
  assert.deepEqual(diffStats(diff), { added: 1, removed: 2 });
  assert.deepEqual(diffStats('--- /dev/null\n+++ b/c.txt\n@@ -0,0 +1 @@\n+one\n'), { added: 1, removed: 0 });
});
