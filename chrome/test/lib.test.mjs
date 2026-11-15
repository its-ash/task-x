import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import { checkPageUrl, describe, ICON_ACTIVE, ICON_INACTIVE, iconColor, isDrivable, isQuiet, parseTabId, pushLog, resolveTarget, withDefaults } from '../lib.js';

test('only http(s) pages with a host are opened', () => {
  assert.equal(checkPageUrl(' https://example.com/a?b=1 '), 'https://example.com/a?b=1');
  for (const bad of ['file:///etc/passwd', 'javascript:alert(1)', 'chrome://settings', 'not a url', '']) {
    assert.throws(() => checkPageUrl(bad), bad);
  }
});

test("Chrome's own pages are never driven", () => {
  assert.ok(isDrivable('https://github.com/'));
  assert.ok(isDrivable('about:blank'));
  for (const url of ['chrome://extensions', 'chrome-extension://abc/x.html', 'https://chromewebstore.google.com/detail/x', 'devtools://devtools/x', 'view-source:https://a.test']) {
    assert.ok(!isDrivable(url), url);
  }
});

test('tab ids come as numbers or strings', () => {
  assert.equal(parseTabId('42'), 42);
  assert.equal(parseTabId(7), 7);
  assert.equal(parseTabId(null), null);
  assert.equal(parseTabId(''), null);
  assert.throws(() => parseTabId('tab-1'), /not a tab id/);
});

test('the default tab is the last one used, then the active one, and only when allowed', () => {
  const exists = (id) => [1, 2, 3].includes(id);
  const shared = new Set([1, 2]);
  const allowed = (id) => shared.has(id);
  const pick = (o) => resolveTarget({ exists, allowed, lastTab: null, activeTab: null, requested: null, ...o });
  assert.equal(pick({ requested: 2 }), 2);
  assert.throws(() => pick({ requested: 3 }), /not shared with task-x/);
  assert.throws(() => pick({ requested: 9 }), /no tab 9/);
  assert.equal(pick({ lastTab: 1, activeTab: 2 }), 1);
  assert.equal(pick({ lastTab: 9, activeTab: 2 }), 2, 'a closed last tab falls back to the active one');
  assert.throws(() => pick({ activeTab: 3 }), /no task-x tab yet/, 'an active tab nobody shared is not used');
});

test('the activity log names methods, never page content', () => {
  assert.deepEqual(describe('cdp', { tabId: '4', method: 'Runtime.evaluate', params: { expression: 'secret' } }), { method: 'Runtime.evaluate', detail: 'tab 4' });
  assert.deepEqual(describe('tabs.open', { url: 'https://a.test/' }), { method: 'open', detail: 'https://a.test/' });
  assert.deepEqual(describe('attach', {}), { method: 'attach', detail: 'current tab' });
  const log = [];
  for (let i = 0; i < 5; i++) pushLog(log, i, 3);
  assert.deepEqual(log, [2, 3, 4]);
  assert.ok(isQuiet('Runtime.evaluate') && !isQuiet('Input.dispatchMouseEvent'));
});

test('the icon is green only while connected and active', () => {
  assert.equal(iconColor({ connected: true, paused: false }), ICON_ACTIVE);
  assert.equal(iconColor({ connected: true, paused: true }), ICON_INACTIVE, 'paused is not active');
  assert.equal(iconColor({ connected: false, paused: false }), ICON_INACTIVE);
  assert.notEqual(ICON_ACTIVE, ICON_INACTIVE);
});

test('every tab is shared by default, unless the user turned it off', () => {
  assert.equal(withDefaults().allTabs, true);
  assert.equal(withDefaults(undefined).allTabs, true);
  assert.equal(withDefaults({}).allTabs, true);
  assert.equal(withDefaults({ allTabs: false }).allTabs, false, 'a saved choice wins');
});

test('the manifest icons exist', () => {
  const m = JSON.parse(readFileSync(new URL('../manifest.json', import.meta.url), 'utf8'));
  for (const file of [...Object.values(m.icons), ...Object.values(m.action.default_icon)]) {
    assert.ok(readFileSync(new URL(`../${file}`, import.meta.url)).length > 0, file);
  }
});

test('the manifest has the key, the native host, and the debugger', () => {
  const m = JSON.parse(readFileSync(new URL('../manifest.json', import.meta.url), 'utf8'));
  assert.equal(m.manifest_version, 3);
  assert.ok(m.key.length > 300, 'the key fixes the extension id the host manifest allows');
  for (const p of ['debugger', 'nativeMessaging', 'tabs', 'tabGroups', 'sidePanel', 'storage', 'scripting']) {
    assert.ok(m.permissions.includes(p), p);
  }
  assert.deepEqual(m.host_permissions, ['<all_urls>']);
});

test('the extension id the runtime allows is the one the manifest key gives', async () => {
  const { createHash } = await import('node:crypto');
  const m = JSON.parse(readFileSync(new URL('../manifest.json', import.meta.url), 'utf8'));
  const hex = createHash('sha256').update(Buffer.from(m.key, 'base64')).digest('hex').slice(0, 32);
  const id = [...hex].map((c) => String.fromCharCode(97 + parseInt(c, 16))).join('');
  const rust = readFileSync(new URL('../../runtime/src/chrome.rs', import.meta.url), 'utf8');
  assert.ok(rust.includes(`EXTENSION_ID: &str = "${id}"`), `runtime/src/chrome.rs must allow ${id}`);
});
