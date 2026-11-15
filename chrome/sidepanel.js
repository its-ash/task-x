// The task-x side panel: connection status, the tabs task-x may drive, controls, and activity.

const $ = (id) => document.getElementById(id);
let current = null;

function ask(msg) {
  return chrome.runtime.sendMessage(msg).then((r) => r?.state && render(r.state));
}

function time(t) {
  return new Date(t).toLocaleTimeString([], { hour: '2-digit', minute: '2-digit', second: '2-digit' });
}

async function render(s) {
  current = s;
  const pill = $('status');
  if (s.paused) {
    pill.textContent = 'Paused';
    pill.className = 'pill warn';
  } else if (s.status.connected) {
    pill.textContent = 'Connected';
    pill.className = 'pill ok';
  } else {
    pill.textContent = 'Not connected';
    pill.className = 'pill bad';
  }
  $('setup').hidden = s.status.connected;
  $('error').textContent = s.status.error || '';
  $('ext-id').textContent = s.extensionId;
  $('pause').textContent = s.paused ? 'Resume' : 'Pause';
  $('all-tabs').checked = s.allTabs;
  $('version').textContent = `task-x for Chrome ${s.version}${s.status.host ? ` · runtime ${s.status.host.version}` : ''}`;

  const [active] = await chrome.tabs.query({ active: true, currentWindow: true });
  const sharedHere = active && s.agentTabs.includes(active.id);
  $('share').textContent = sharedHere ? 'Stop sharing this tab' : 'Share this tab';

  const list = $('tabs');
  list.replaceChildren();
  const tabs = (await Promise.all(s.agentTabs.map((id) => chrome.tabs.get(id).catch(() => null)))).filter(Boolean);
  $('no-tabs').hidden = tabs.length > 0;
  for (const t of tabs) {
    const li = document.createElement('li');
    const title = document.createElement('span');
    title.className = 'title';
    title.textContent = t.title || t.url;
    title.title = t.url;
    const tag = document.createElement('span');
    const live = s.attached.includes(t.id);
    tag.className = live ? 'tag live' : 'tag';
    tag.textContent = live ? 'driving' : t.id === s.lastTab ? 'last used' : 'shared';
    li.append(title, tag);
    li.onclick = () => {
      chrome.tabs.update(t.id, { active: true });
      chrome.windows.update(t.windowId, { focused: true });
    };
    list.append(li);
  }

  const log = $('log');
  log.replaceChildren();
  for (const e of [...s.log].reverse()) {
    const li = document.createElement('li');
    li.className = !e.ok ? 'fail' : e.quiet ? 'quiet' : '';
    const when = document.createElement('span');
    when.className = 'time';
    when.textContent = time(e.t);
    li.append(when, `${e.method}${e.detail ? ` ${e.detail}` : ''}${e.error ? ` — ${e.error}` : ''}`);
    log.append(li);
  }
}

$('reconnect').onclick = () => ask({ type: 'reconnect' });
$('share').onclick = async () => {
  const [active] = await chrome.tabs.query({ active: true, currentWindow: true });
  if (active) ask({ type: 'share', tabId: active.id });
};
$('pause').onclick = () => ask({ type: 'pause', paused: !current?.paused });
$('all-tabs').onchange = (e) => ask({ type: 'allTabs', value: e.target.checked });
$('clear').onclick = () => ask({ type: 'clearLog' });

chrome.runtime.onMessage.addListener((msg) => {
  if (msg?.type === 'state') render(msg.state);
});
chrome.tabs.onActivated.addListener(() => current && render(current));

ask({ type: 'get' });
