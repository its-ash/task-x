// task-x Chrome extension: the service worker.
//
// It connects to the task-x runtime through the native messaging host (`com.taskx.chrome`,
// registered by `task-x-runtime chrome-install`). The host forwards requests from every running
// runtime; this worker answers them: tab work itself, and page work as CDP commands it relays to
// a tab with chrome.debugger. The protocol is in docs/chrome-bridge.md.
//
// The agent may drive every tab by default ("Let task-x use every tab" in the side panel); with
// that off, only tabs it opened (they go in the "task-x · automated" tab group) and tabs the
// user shared. Pause stops every request until the user resumes.

import { checkPageUrl, describe, iconColor, isDrivable, isQuiet, parseTabId, pushLog, resolveTarget, withDefaults } from './lib.js';

const HOST = 'com.taskx.chrome';
/** Tabs the agent opens are grouped under this title, so automated tabs are easy to tell apart. */
const GROUP_TITLE = 'task-x · automated';
const DEBUGGER_VERSION = '1.3';
const RECONNECT_ALARM = 'task-x-reconnect';
const VERSION = chrome.runtime.getManifest().version;

let port = null;
/** Connection state the side panel shows. */
const status = { connected: false, error: '', host: null, since: 0 };
/** Tabs the debugger is attached to. */
const attached = new Set();
/** In-memory copy of the session state; saved to storage.session on change. */
let state = { agentTabs: [], lastTab: null, groups: {}, paused: false, log: [] };
let settings = withDefaults();
const ready = load();

async function load() {
  const s = await chrome.storage.session.get('state');
  if (s.state) state = { ...state, ...s.state };
  const l = await chrome.storage.local.get('settings');
  settings = withDefaults(l.settings);
}

function save() {
  chrome.storage.session.set({ state }).catch(() => {});
  broadcast();
}

/** Pushes the current state to open side panels. */
function broadcast() {
  chrome.runtime.sendMessage({ type: 'state', state: view() }).catch(() => {});
  chrome.action.setBadgeText({ text: state.paused ? '❚❚' : status.connected ? '' : '!' }).catch(() => {});
  chrome.action.setBadgeBackgroundColor({ color: state.paused ? '#b45309' : '#b91c1c' }).catch(() => {});
  drawIcon(iconColor({ connected: status.connected, paused: state.paused }));
}

let drawnColor = null;

/**
 * Draws the toolbar icon in `color`: the rounded square with an "x" of icons/icon.svg. Green while
 * connected and active, grey otherwise. Without OffscreenCanvas the static icon stays.
 */
function drawIcon(color) {
  if (color === drawnColor) return;
  try {
    const imageData = {};
    for (const size of [16, 32]) {
      const c = new OffscreenCanvas(size, size);
      const g = c.getContext('2d');
      const s = size / 128;
      g.scale(s, s);
      g.fillStyle = color;
      g.beginPath();
      g.roundRect(4, 4, 120, 120, 28);
      g.fill();
      g.strokeStyle = '#ffffff';
      g.lineWidth = 18;
      g.lineCap = 'round';
      g.beginPath();
      g.moveTo(40, 40);
      g.lineTo(88, 88);
      g.moveTo(88, 40);
      g.lineTo(40, 88);
      g.stroke();
      imageData[size] = g.getImageData(0, 0, size, size);
    }
    chrome.action.setIcon({ imageData }).then(() => (drawnColor = color), () => {});
  } catch {
    // No OffscreenCanvas (or no roundRect): the manifest's icon stays.
  }
}

function view() {
  return {
    status: { ...status },
    paused: state.paused,
    allTabs: settings.allTabs,
    agentTabs: state.agentTabs,
    lastTab: state.lastTab,
    attached: [...attached],
    log: state.log.slice(-100),
    version: VERSION,
    extensionId: chrome.runtime.id,
  };
}

function log(method, detail, ok, error) {
  pushLog(state.log, { t: Date.now(), method, detail, ok, error: error || '', quiet: ok && isQuiet(method) });
}

// ---- Connection to the runtime -------------------------------------------------------------

function connect() {
  if (port) return;
  try {
    port = chrome.runtime.connectNative(HOST);
  } catch (e) {
    status.error = String(e.message || e);
    port = null;
    broadcast();
    return;
  }
  status.error = '';
  port.onMessage.addListener(onHostMessage);
  port.onDisconnect.addListener(() => {
    status.error = chrome.runtime.lastError?.message || 'the task-x host closed';
    status.connected = false;
    status.host = null;
    port = null;
    broadcast();
  });
}

function send(msg) {
  try {
    port?.postMessage(msg);
  } catch {
    // The port closed while a request ran; the runtime sees the hub close.
  }
}

async function onHostMessage(msg) {
  await ready;
  if (msg.event === 'host.ready') {
    status.connected = true;
    status.host = msg.params || null;
    status.since = Date.now();
    broadcast();
    return;
  }
  if (msg.id === undefined || msg.id === null) return;
  const { method, detail } = describe(msg.method, msg.params || {});
  try {
    if (state.paused && msg.method !== 'ping') throw new Error('the user paused task-x in Chrome; ask them to resume it from the task-x side panel');
    const handler = HANDLERS[msg.method];
    if (!handler) throw new Error(`unknown method ${msg.method}`);
    const result = await handler(msg.params || {});
    send({ id: msg.id, result });
    log(method, detail, true);
  } catch (e) {
    const message = String(e?.message || e);
    send({ id: msg.id, error: { message } });
    log(method, detail, false, message);
  }
  save();
}

// ---- Tabs the agent may use ----------------------------------------------------------------

function allowed(tabId) {
  return settings.allTabs || state.agentTabs.includes(tabId);
}

function tabInfo(t) {
  return { id: String(t.id), url: t.url || t.pendingUrl || '', title: t.title || '', active: !!t.active, windowId: t.windowId, agent: allowed(t.id) };
}

function share(tabId) {
  if (!state.agentTabs.includes(tabId)) state.agentTabs.push(tabId);
}

async function unshare(tabId) {
  state.agentTabs = state.agentTabs.filter((t) => t !== tabId);
  if (state.lastTab === tabId) state.lastTab = null;
  await detach(tabId);
}

/** Puts a tab in the window's task-x group, making the group when the window has none. */
async function group(tab) {
  try {
    const known = state.groups[tab.windowId];
    let groupId = null;
    if (known !== undefined) {
      try {
        await chrome.tabGroups.get(known);
        groupId = known;
      } catch {
        groupId = null;
      }
    }
    groupId = await chrome.tabs.group(groupId === null ? { tabIds: [tab.id], createProperties: { windowId: tab.windowId } } : { tabIds: [tab.id], groupId });
    state.groups[tab.windowId] = groupId;
    await chrome.tabGroups.update(groupId, { title: GROUP_TITLE, color: 'purple' });
  } catch {
    // Grouping is a convenience: a popup window cannot hold groups.
  }
}

/** Waits until the tab finishes loading, or the time runs out. */
function waitLoaded(tabId, timeoutMs) {
  return new Promise((resolve) => {
    let done = false;
    const finish = () => {
      if (done) return;
      done = true;
      chrome.tabs.onUpdated.removeListener(listener);
      clearTimeout(timer);
      resolve();
    };
    const listener = (id, change) => {
      if (id === tabId && change.status === 'complete') finish();
    };
    const timer = setTimeout(finish, Math.max(500, Math.min(timeoutMs || 5000, 30000)));
    chrome.tabs.onUpdated.addListener(listener);
    chrome.tabs.get(tabId).then((t) => t.status === 'complete' && finish(), finish);
  });
}

async function attach(tabId) {
  if (attached.has(tabId)) return;
  const tab = await chrome.tabs.get(tabId);
  if (!isDrivable(tab.url || tab.pendingUrl)) {
    throw new Error(`Chrome does not let extensions drive ${tab.url}; open an http(s) page instead`);
  }
  try {
    await chrome.debugger.attach({ tabId }, DEBUGGER_VERSION);
  } catch (e) {
    // Attached already, by this worker before it restarted.
    if (!/already attached/i.test(String(e.message))) throw e;
  }
  attached.add(tabId);
}

async function detach(tabId) {
  if (!attached.delete(tabId)) return;
  await chrome.debugger.detach({ tabId }).catch(() => {});
}

async function activeTabId() {
  const [t] = await chrome.tabs.query({ active: true, lastFocusedWindow: true });
  return t?.id ?? null;
}

/** The tab a request names, or the default one; checked against what the agent may use. */
async function target(raw) {
  const requested = parseTabId(raw);
  const known = new Set((await chrome.tabs.query({})).map((t) => t.id));
  return resolveTarget({
    requested,
    lastTab: state.lastTab,
    activeTab: await activeTabId(),
    exists: (id) => known.has(id),
    allowed,
  });
}

// ---- Requests from the runtime --------------------------------------------------------------

const HANDLERS = {
  async ping() {
    return { version: VERSION, paused: state.paused };
  },

  async 'tabs.list'() {
    const all = await chrome.tabs.query({});
    const tabs = all.filter((t) => allowed(t.id)).map(tabInfo);
    return {
      tabs,
      count: tabs.length,
      hidden: all.length - tabs.length,
      note: settings.allTabs ? '' : 'Only tabs task-x opened or the user shared are listed; open a page with the open action.',
    };
  },

  async 'tabs.open'({ url, timeoutMs }) {
    const href = checkPageUrl(url);
    const tab = await chrome.tabs.create({ url: href, active: true });
    share(tab.id);
    state.lastTab = tab.id;
    await group(tab);
    await waitLoaded(tab.id, timeoutMs);
    return tabInfo(await chrome.tabs.get(tab.id));
  },

  async 'tabs.close'({ tabId }) {
    const id = await target(tabId);
    await detach(id);
    await chrome.tabs.remove(id);
    state.agentTabs = state.agentTabs.filter((t) => t !== id);
    if (state.lastTab === id) state.lastTab = null;
    return { closed: String(id) };
  },

  async attach({ tabId }) {
    const id = await target(tabId);
    await attach(id);
    state.lastTab = id;
    const t = await chrome.tabs.get(id);
    return { tabId: String(id), url: t.url || '', title: t.title || '' };
  },

  async cdp({ tabId, method, params }) {
    if (typeof method !== 'string' || !method) throw new Error('cdp needs a method');
    const id = await target(tabId);
    await attach(id);
    try {
      return (await chrome.debugger.sendCommand({ tabId: id }, method, params || {})) ?? {};
    } catch (e) {
      throw new Error(`${method}: ${e.message || e}`);
    }
  },
};

// ---- Chrome events -----------------------------------------------------------------------------

chrome.debugger.onDetach.addListener(({ tabId }, reason) => {
  if (tabId === undefined || !attached.delete(tabId)) return;
  // The user pressed Cancel on Chrome's "is debugging this browser" bar: stop using the tab.
  if (reason === 'canceled_by_user') {
    state.agentTabs = state.agentTabs.filter((t) => t !== tabId);
    log('detach', `tab ${tabId}: cancelled by the user`, true);
  }
  save();
});

chrome.tabs.onRemoved.addListener((tabId) => {
  attached.delete(tabId);
  if (!state.agentTabs.includes(tabId) && state.lastTab !== tabId) return;
  state.agentTabs = state.agentTabs.filter((t) => t !== tabId);
  if (state.lastTab === tabId) state.lastTab = null;
  save();
});

chrome.tabs.onActivated.addListener(broadcast);

async function toggleShare(tabId) {
  await ready;
  if (tabId === null || tabId === undefined) return;
  if (state.agentTabs.includes(tabId)) await unshare(tabId);
  else share(tabId);
  save();
}

async function setPaused(paused) {
  await ready;
  state.paused = paused;
  if (paused) for (const t of [...attached]) await detach(t);
  log(paused ? 'pause' : 'resume', '', true);
  save();
}

chrome.commands.onCommand.addListener(async (command) => {
  if (command === 'share-tab') await toggleShare(await activeTabId());
  if (command === 'pause') await setPaused(!state.paused);
});

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === 'task-x-share' && tab?.id !== undefined) await toggleShare(tab.id);
});

chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  (async () => {
    await ready;
    switch (msg?.type) {
      case 'get':
        break;
      case 'reconnect':
        port?.disconnect();
        port = null;
        status.connected = false;
        connect();
        break;
      case 'share':
        await toggleShare(msg.tabId ?? (await activeTabId()));
        break;
      case 'pause':
        await setPaused(!!msg.paused);
        break;
      case 'allTabs':
        settings.allTabs = !!msg.value;
        await chrome.storage.local.set({ settings });
        log('setting', `every tab: ${settings.allTabs ? 'on' : 'off'}`, true);
        save();
        break;
      case 'clearLog':
        state.log = [];
        save();
        break;
      default:
        return reply({ error: 'unknown message' });
    }
    reply({ state: view() });
  })();
  return true;
});

chrome.alarms.onAlarm.addListener((a) => {
  if (a.name === RECONNECT_ALARM && !port) connect();
});

function setup() {
  chrome.sidePanel.setPanelBehavior({ openPanelOnActionClick: true }).catch(() => {});
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({ id: 'task-x-share', title: 'Share this tab with task-x (or stop)', contexts: ['page', 'action'] });
  });
  chrome.alarms.create(RECONNECT_ALARM, { periodInMinutes: 1 });
}

chrome.runtime.onInstalled.addListener(setup);
chrome.runtime.onStartup.addListener(setup);

// An open native port keeps this worker alive; connect whenever it starts.
ready.then(() => {
  connect();
  broadcast();
});
