// task-x compare panel. Text is always inserted with textContent.
(() => {
  const vscode = acquireVsCodeApi();
  const grid = document.getElementById('grid');
  const el = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = text; return e; };
  const icon = (name) => { const i = el('span', `codicon codicon-${name}`); i.setAttribute('aria-hidden', 'true'); return i; };
  const button = (cls, content, onClick, title) => {
    const b = el('button', cls);
    b.type = 'button';
    for (const c of [].concat(content)) b.append(typeof c === 'string' ? document.createTextNode(c) : c);
    if (title) b.title = title;
    b.addEventListener('click', onClick);
    return b;
  };
  const STATUS = { completed: 'Done', failed: 'Failed', cancelled: 'Stopped', maxIterations: 'Turn limit', stuck: 'Stuck', providerError: 'Model error' };
  let data = null, kept;

  function card(c, i) {
    const ok = c.status === 'completed';
    const box = el('article', 'card' + (ok ? '' : ' bad') + (kept === c.name && kept ? ' kept' : ''));
    box.style.setProperty('--i', String(i));
    const head = el('header', 'head');
    const t = el('div', 'model');
    t.append(el('h2', null, c.model), el('span', 'kind', c.kind));
    head.append(t, el('span', 'badge ' + (ok ? 'ok' : 'warn'), STATUS[c.status] || c.status));
    box.append(head);
    const checks = c.validations || [];
    const failed = checks.filter((v) => !v.success).length;
    const stats = el('div', 'stats');
    const stat = (label, value, cls) => { const s = el('div', 'stat'); s.append(el('span', 'v ' + (cls || ''), value), el('span', 'l', label)); stats.append(s); };
    stat('files', String(c.files?.length ?? 0));
    const lines = el('div', 'stat');
    const v = el('span', 'v');
    v.append(el('span', 'add', `+${c.added ?? 0}`), ' ', el('span', 'del', `-${c.removed ?? 0}`));
    lines.append(v, el('span', 'l', 'lines'));
    stats.append(lines);
    stat('checks', checks.length ? (failed ? `${failed}/${checks.length} failed` : `${checks.length} passed`) : 'none run', checks.length ? (failed ? 'del' : 'add') : 'muted');
    stat('cost', c.cost || (c.tokens ? `${c.tokens} tok` : 'n/a'));
    box.append(stats);
    if (c.error) box.append(el('p', 'error', c.error));
    if (c.report) box.append(el('p', 'report', c.report.length > 1200 ? c.report.slice(0, 1199) + '…' : c.report));
    if (c.files?.length) {
      const ul = el('ul', 'files');
      for (const f of c.files.slice(0, 12)) ul.append(el('li', null, f));
      if (c.files.length > 12) ul.append(el('li', 'muted', `+${c.files.length - 12} more`));
      box.append(ul);
    }
    const acts = el('footer', 'acts');
    const done = data.resolved || kept !== undefined;
    if (kept === c.name && c.name) acts.append(el('span', 'kept-label', 'Kept'));
    else if (!done && c.name) {
      acts.append(button('btn', [icon('check'), 'Keep this'], () => vscode.postMessage({ type: 'keep', i }), 'Apply this result and discard the others'));
      acts.append(button('btn secondary', [icon('diff'), 'Diff'], () => vscode.postMessage({ type: 'diff', i })));
      if (c.dir) acts.append(button('action', icon('folder-opened'), () => vscode.postMessage({ type: 'open', i }), 'Open the worktree in a new window'));
    } else if (c.name && !done) acts.append(button('btn secondary', [icon('diff'), 'Diff'], () => vscode.postMessage({ type: 'diff', i })));
    box.append(acts);
    return box;
  }
  function render() {
    document.getElementById('prompt').textContent = data.prompt;
    grid.style.setProperty('--n', String(data.candidates.length));
    grid.replaceChildren(...data.candidates.map(card));
    document.getElementById('discardAll').disabled = data.resolved || kept !== undefined;
  }
  document.getElementById('discardAll').addEventListener('click', () => vscode.postMessage({ type: 'discardAll' }));
  window.addEventListener('message', ({ data: m }) => {
    if (m.type === 'data') { data = m; render(); }
    else if (m.type === 'resolved' && data) { kept = m.kept ?? null; data.resolved = true; render(); }
  });
  vscode.postMessage({ type: 'ready' });
})();
