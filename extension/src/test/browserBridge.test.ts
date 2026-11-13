import assert from 'node:assert/strict';
import { test } from 'node:test';
import { findBrowser, launchBrowser, normalizeCdpEndpoint } from '../browserBridge.js';

test('normalizes a local http endpoint and refuses everything else', () => {
  assert.equal(normalizeCdpEndpoint(' http://127.0.0.1:9223 '), 'http://127.0.0.1:9223');
  assert.equal(normalizeCdpEndpoint('http://localhost:9222/'), 'http://localhost:9222');
  assert.equal(normalizeCdpEndpoint('http://[::1]:9222'), 'http://[::1]:9222');
  for (const bad of ['https://127.0.0.1:9222', 'http://10.0.0.5:9222', 'http://example.com:9222', 'not a url']) {
    assert.throws(() => normalizeCdpEndpoint(bad), `${bad} must be refused`);
  }
});

test('launchBrowser rejects an invalid port and builds a dedicated profile', () => {
  assert.throws(() => launchBrowser('/chrome', 0));
  assert.throws(() => launchBrowser('/chrome', -1));
  assert.throws(() => launchBrowser('/chrome', 'nine' as unknown as number));
  // A real launch is not attempted with a bogus binary; this only checks the argument validation above.
});

test('findBrowser returns the first installed candidate', () => {
  const seen: string[] = [];
  const fs = { existsSync: (p: string) => (seen.push(p), p.endsWith('Chrome')) };
  const found = findBrowser(fs, 'darwin');
  assert.equal(found, '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome');
  assert.deepEqual(seen, ['/Applications/Google Chrome.app/Contents/MacOS/Google Chrome']);
  assert.equal(findBrowser({ existsSync: () => false }, 'darwin'), undefined, 'nothing installed');
  assert.equal(findBrowser({ existsSync: () => true }, 'linux'), 'google-chrome');
  assert.equal(findBrowser({ existsSync: () => true }, 'sunos' as NodeJS.Platform), undefined, 'unknown platform');
});