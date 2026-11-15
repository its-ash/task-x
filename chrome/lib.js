// Pure helpers for the task-x Chrome extension: no `chrome.*` here, so Node can test them.

/** URL prefixes Chrome never lets an extension debug or script. */
const LOCKED = [
  'chrome://',
  'chrome-extension://',
  'chrome-search://',
  'chrome-untrusted://',
  'devtools://',
  'edge://',
  'brave://',
  'view-source:',
  'https://chrome.google.com/webstore',
  'https://chromewebstore.google.com',
  'https://microsoftedge.microsoft.com/addons',
];

/** A page the agent may open: http(s) with a host. Throws a message the model can act on. */
export function checkPageUrl(raw) {
  let url;
  try {
    url = new URL(String(raw ?? '').trim());
  } catch {
    throw new Error(`invalid url: ${raw}`);
  }
  if (!(url.protocol === 'http:' || url.protocol === 'https:') || !url.hostname) {
    throw new Error('the browser opens only http:// and https:// URLs with a host');
  }
  return url.href;
}

/** True when Chrome lets the debugger attach to a tab showing this URL. */
export function isDrivable(url) {
  const u = String(url ?? '');
  if (u === '' || u === 'about:blank') return true;
  return !LOCKED.some((p) => u.startsWith(p));
}

/** A tab id from the runtime (a string or a number), or null when none was given. */
export function parseTabId(v) {
  if (v === null || v === undefined || v === '') return null;
  const n = typeof v === 'number' ? v : Number(String(v).trim());
  if (!Number.isInteger(n) || n < 0) throw new Error(`'${v}' is not a tab id; list the tabs with the tabs action`);
  return n;
}

/**
 * The tab an action without a target runs on: the tab the agent used last, else the active tab
 * when the agent may use it. Throws when there is none.
 */
export function resolveTarget({ requested, lastTab, activeTab, exists, allowed }) {
  if (requested !== null && requested !== undefined) {
    if (!exists(requested)) throw new Error(`no tab ${requested}; list them with the tabs action`);
    if (!allowed(requested)) {
      throw new Error(`tab ${requested} is not shared with task-x; open a page with the open action, or the user can share the tab from the task-x side panel`);
    }
    return requested;
  }
  if (lastTab !== null && lastTab !== undefined && exists(lastTab) && allowed(lastTab)) return lastTab;
  if (activeTab !== null && activeTab !== undefined && allowed(activeTab)) return activeTab;
  throw new Error('no task-x tab yet; use the open action with a url, or the user can share a tab from the task-x side panel');
}

/** What the activity log shows for one request: the method and a short detail, never page content. */
export function describe(method, params = {}) {
  switch (method) {
    case 'cdp':
      return { method: params.method || 'cdp', detail: params.tabId !== undefined && params.tabId !== null ? `tab ${params.tabId}` : '' };
    case 'tabs.open':
      return { method: 'open', detail: String(params.url || '') };
    case 'tabs.close':
    case 'attach':
      return { method, detail: params.tabId ? `tab ${params.tabId}` : 'current tab' };
    default:
      return { method, detail: '' };
  }
}

/** Appends to a bounded log, newest last. */
export function pushLog(log, entry, max = 200) {
  log.push(entry);
  if (log.length > max) log.splice(0, log.length - max);
  return log;
}

/** Icon colours: green while task-x is connected and active, grey otherwise. */
export const ICON_ACTIVE = '#16a34a';
export const ICON_INACTIVE = '#9ca3af';

/** The toolbar icon's colour: green only when connected to the runtime and not paused. */
export function iconColor({ connected, paused }) {
  return connected && !paused ? ICON_ACTIVE : ICON_INACTIVE;
}

/** The settings in effect: every tab is shared by default; a value the user saved wins. */
export function withDefaults(saved) {
  return { allTabs: true, ...(saved && typeof saved === 'object' ? saved : {}) };
}

/** CDP calls that are only reads: shown dimmed in the log. */
export function isQuiet(cdpMethod) {
  return /^(Runtime\.evaluate|Page\.enable|DOM\.getDocument|Accessibility\.)/.test(String(cdpMethod || ''));
}
