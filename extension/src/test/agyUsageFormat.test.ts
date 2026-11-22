import assert from 'node:assert/strict';
import { test } from 'node:test';
import { lowest, resetIn, usageLines, usageShort } from '../agyUsageFormat.js';
import type { AgyUsage } from '../protocol.js';

const now = Date.parse('2026-11-21T10:00:00Z');
const usage: AgyUsage = {
  description: 'Model quota',
  groups: [
    {
      name: 'Gemini Pro',
      buckets: [
        { id: 'pro-5h', name: '5 hour', window: '5h', remainingFraction: 0.42, resetTime: '2026-11-21T13:05:00Z' },
        { id: 'pro-week', name: 'Weekly', window: 'weekly', remainingFraction: 0.9, resetTime: '2026-11-24T14:00:00Z' },
      ],
    },
    { name: 'Gemini Flash', buckets: [{ id: 'flash-5h', name: '5 hour', window: '5h', remainingFraction: 0.98 }] },
  ],
  credits: { remaining: 1234.4 },
};

test('formats the time until a limit resets', () => {
  assert.equal(resetIn('2026-11-21T13:05:00Z', now), '3h 5m');
  assert.equal(resetIn('2026-11-24T14:00:00Z', now), '3d 4h');
  assert.equal(resetIn('2026-11-21T10:00:30Z', now), '1m');
  assert.equal(resetIn('2026-11-21T09:00:00Z', now), '', 'a past reset shows nothing');
  assert.equal(resetIn(undefined, now), '');
  assert.equal(resetIn('soon', now), '');
});

test('the short line shows the lowest share left per window, then the credits', () => {
  assert.equal(usageShort(usage), '5h 42% · week 90% · 1,234 credits');
  assert.equal(lowest(usage), 0.42);
  assert.equal(usageShort({ groups: [], credits: null }), 'no limits reported');
  assert.equal(lowest({ groups: [], credits: null }), undefined);
});

test('the detail lists every group with reset times', () => {
  assert.deepEqual(usageLines(usage, now), [
    'Gemini Pro: 5h 42% left (resets in 3h 5m) · week 90% left (resets in 3d 4h)',
    'Gemini Flash: 5h 98% left',
    'AI credits: 1,234',
  ]);
  assert.deepEqual(usageLines({ groups: [{ name: 'X', buckets: [{ id: 'a', name: 'a', window: '5h', remainingFraction: 7 }] }], credits: null }, now), ['X: 5h 100% left'], 'fractions are clamped');
});
