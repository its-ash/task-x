// task-x logs webview. Log text is always inserted with textContent.
(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const log = $('log'), q = $('q'), level = $('level'), src = $('src'), count = $('count'), follow = $('follow'), wrap = $('wrap');
  const RANK = { trace: 0, debug: 1, info: 2, warn: 3, error: 4 };
  const MAX_ROWS = 5000;
  const saved = vscode.getState() || {};
  let entries = [];
  let following = saved.follow !== false;
  const tabs = [...level.querySelectorAll('.tab')];
  level.style.setProperty('--n', String(tabs.length));
  let minLevel = RANK[saved.level] != null ? saved.level : 'info';
  const setLevel = (l) => {
    minLevel = l;
    level.dataset.level = l;
    tabs.forEach((t, i) => {
      const on = t.dataset.level === l;
      t.setAttribute('aria-selected', String(on));
      t.tabIndex = on ? 0 : -1;
      if (on) level.style.setProperty('--sel', String(i));
    });
  };
  setLevel(minLevel);
  if (saved.q) q.value = saved.q;
  if (saved.wrap) document.body.classList.add('wrap');

  const save = () => vscode.setState({ level: minLevel, q: q.value, src: src.value, follow: following, wrap: document.body.classList.contains('wrap') });
  const pad = (n, w = 2) => String(n).padStart(w, '0');
  const time = (t) => { const d = new Date(t); return `${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}.${pad(d.getMilliseconds(), 3)}`; };
  const el = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = text; return e; };

  function match(e) {
    if (RANK[e.level] < RANK[minLevel]) return false;
    if (src.value && e.src !== src.value) return false;
    const s = q.value.trim().toLowerCase();
    return !s || e.msg.toLowerCase().includes(s) || e.src.toLowerCase().includes(s);
  }
  function row(e) {
    const r = el('div', `row ${e.level}`);
    r.append(el('span', 'time', time(e.t)), el('span', 'lvl', e.level.toUpperCase()), el('span', 'src', e.src), el('span', 'msg', e.msg));
    return r;
  }
  function sources() {
    const have = new Set([...src.options].map((o) => o.value));
    for (const s of new Set(entries.map((e) => e.src))) if (!have.has(s)) src.append(new Option(s, s));
    if (saved.src && have.has(saved.src)) { src.value = saved.src; saved.src = undefined; }
  }
  function status() {
    const shown = log.childElementCount;
    count.textContent = shown === entries.length ? `${entries.length}` : `${shown} of ${entries.length}`;
    if (!entries.length) log.replaceChildren(el('div', 'empty', 'No logs yet.'));
  }
  const bottom = () => { if (following) log.scrollTop = log.scrollHeight; };
  function render() {
    log.replaceChildren(...entries.filter(match).slice(-MAX_ROWS).map(row));
    status();
    bottom();
  }
  function append(list) {
    log.querySelector('.empty')?.remove();
    entries.push(...list);
    if (entries.length > MAX_ROWS) entries.splice(0, entries.length - MAX_ROWS);
    sources();
    for (const e of list) if (match(e)) log.append(row(e));
    while (log.childElementCount > MAX_ROWS) log.firstChild.remove();
    status();
    bottom();
  }
  const setFollow = (on) => {
    following = on;
    follow.classList.toggle('on', on);
    follow.setAttribute('aria-pressed', String(on));
    save();
    bottom();
  };

  q.addEventListener('input', () => { save(); render(); });
  for (const t of tabs) t.addEventListener('click', () => { if (t.dataset.level === minLevel) return; setLevel(t.dataset.level); save(); render(); });
  level.addEventListener('keydown', (e) => {
    const i = tabs.indexOf(document.activeElement);
    if (i < 0) return;
    const n = tabs.length;
    const j = { ArrowRight: (i + 1) % n, ArrowLeft: (i - 1 + n) % n, Home: 0, End: n - 1 }[e.key];
    if (j == null) return;
    e.preventDefault();
    tabs[j].focus();
    tabs[j].click();
  });
  src.addEventListener('change', () => { save(); render(); });
  follow.addEventListener('click', () => setFollow(!following));
  wrap.addEventListener('click', () => {
    const on = document.body.classList.toggle('wrap');
    wrap.classList.toggle('on', on);
    wrap.setAttribute('aria-pressed', String(on));
    save();
  });
  wrap.classList.toggle('on', document.body.classList.contains('wrap'));
  $('copy').addEventListener('click', () => vscode.postMessage({ type: 'copy' }));
  $('output').addEventListener('click', () => vscode.postMessage({ type: 'output' }));
  $('clear').addEventListener('click', () => vscode.postMessage({ type: 'clear' }));
  // Scrolling up pauses follow; scrolling back to the end resumes it.
  log.addEventListener('scroll', () => {
    const atEnd = log.scrollHeight - log.scrollTop - log.clientHeight < 8;
    if (atEnd !== following) setFollow(atEnd);
  });

  window.addEventListener('message', ({ data: m }) => {
    if (m.type === 'reset') { entries = m.entries.slice(-MAX_ROWS); sources(); render(); }
    else if (m.type === 'append') append(m.entries);
  });
  setFollow(following);
  vscode.postMessage({ type: 'ready' });
})();
