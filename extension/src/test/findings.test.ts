import assert from 'node:assert/strict';
import { test } from 'node:test';
import { parseFindings } from '../findings.js';

test('parses findings blocks and skips unsafe or malformed entries', () => {
  const text = [
    'Two problems.',
    '```anvil-findings',
    JSON.stringify([
      { path: './src/a.ts', line: 12, severity: 'high', message: 'Null dereference' },
      { path: 'src/b.ts', line: 0, severity: 'low', message: 'Unused import' },
      { path: '../etc/passwd', line: 1, message: 'outside' },
      { path: 'src/c.ts', message: '' },
    ]),
    '```',
    '```anvil-findings',
    'not json',
    '```',
  ].join('\n');
  assert.deepEqual(parseFindings(text), [
    { path: 'src/a.ts', line: 12, severity: 'error', message: 'Null dereference' },
    { path: 'src/b.ts', line: 1, severity: 'info', message: 'Unused import' },
  ]);
  assert.deepEqual(parseFindings('no block here'), []);
});
