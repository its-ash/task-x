// task-x workspace index view: structured summary of indexed files, modules, symbols, and dependencies.
// Text is always inserted with textContent.
(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const nodeList = $('nodeList'), listHead = $('listHead'), side = $('side'), empty = $('empty');
  const el = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = text; return e; };
  const icon = (name) => { const i = el('span', `codicon codicon-${name}`); i.setAttribute('aria-hidden', 'true'); return i; };
  const css = (v) => getComputedStyle(document.body).getPropertyValue(v).trim();
  const LANG_TOKEN = { rust: '--md-ext-rose', typescript: '--md-sys-color-primary', tsx: '--md-sys-color-primary', javascript: '--md-ext-amber', python: '--md-ext-green', go: '--md-ext-teal' };
  const saved = vscode.getState() || {};
  let level = saved.level || 'module', importsOnly = !!saved.importsOnly;
  let nodes = [], selected = -1, stats = null;
  let colors = {};

  function readColors() {
    colors = {
      select: css('--md-sys-color-primary'),
      other: css('--md-sys-color-on-surface-variant'),
    };
    for (const [l, t] of Object.entries(LANG_TOKEN)) colors[l] = css(t);
  }

  // ---------- Controls ----------
  const tabs = [...document.querySelectorAll('#level .tab')];
  const levelBox = $('level');
  levelBox.style.setProperty('--n', String(tabs.length));
  function setLevel(l) {
    level = l;
    tabs.forEach((t, i) => {
      const on = t.dataset.level === l;
      t.setAttribute('aria-selected', String(on));
      t.tabIndex = on ? 0 : -1;
      if (on) levelBox.style.setProperty('--sel', String(i));
    });
  }
  setLevel(level);
  $('imports').checked = importsOnly;
  const save = () => vscode.setState({ level, importsOnly });
  const load = () => { save(); vscode.postMessage({ type: 'load', level, importsOnly, focus: $('focus').value.trim() || undefined }); };
  for (const t of tabs) t.addEventListener('click', () => { if (t.dataset.level !== level) { setLevel(t.dataset.level); load(); } });
  levelBox.addEventListener('keydown', (e) => {
    const i = tabs.indexOf(document.activeElement);
    const j = { ArrowRight: (i + 1) % tabs.length, ArrowLeft: (i - 1 + tabs.length) % tabs.length }[e.key];
    if (i < 0 || j == null) return;
    e.preventDefault();
    tabs[j].focus();
    tabs[j].click();
  });
  $('imports').addEventListener('change', (e) => { importsOnly = e.target.checked; load(); });
  $('focus').addEventListener('keydown', (e) => { if (e.key === 'Enter') load(); });
  $('focus').addEventListener('search', load);
  const reindex = $('reindex');
  reindex.addEventListener('click', () => vscode.postMessage({ type: 'reindex' }));

  function showIndex(ix) {
    const busy = ix?.state === 'indexing';
    reindex.disabled = busy;
    reindex.classList.toggle('busy', busy);
    reindex.querySelector('.codicon').className = `codicon ${busy ? 'codicon-loading codicon-modifier-spin' : 'codicon-refresh'}`;
    $('reindexText').textContent = !busy ? 'Reindex' : ix.phase === 'graph' ? 'Updating index…' : `Indexing ${ix.files.toLocaleString()} files…`;
    if (busy) $('summary').textContent = ix.phase === 'graph' ? 'Updating workspace index…' : `Indexing: ${ix.files.toLocaleString()} files seen, ${ix.parsed.toLocaleString()} parsed`;
    else if (ix?.state === 'failed') $('summary').textContent = `Indexing failed: ${ix.error}`;
  }

  // ---------- Indexed list ----------
  function renderList(total) {
    nodeList.replaceChildren();
    listHead.replaceChildren();
    const label = level === 'module' ? 'Modules' : 'Indexed files';
    const title = el('span', 'list-title', total > nodes.length ? `${label} (top ${nodes.length} of ${total})` : `${label} (${nodes.length})`);
    listHead.append(title, el('span', 'list-meta-head', level === 'module' ? 'Files · Symbols · Links' : 'Symbols · Links'));
    nodes.forEach((n, i) => {
      const li = el('li');
      const row = el('button', 'node-row' + (i === selected ? ' selected' : ''));
      row.type = 'button';
      const sw = el('span', 'swatch');
      sw.style.setProperty('--c', colors[n.language] || colors.other);
      const name = el('span', 'node-path', n.id);
      const meta = el('span', 'node-meta');
      if (n.language) meta.append(el('span', 'pill lang', n.language));
      if (level === 'module') meta.append(el('span', 'pill', `${n.files} ${n.files === 1 ? 'file' : 'files'}`));
      meta.append(el('span', 'pill', `${n.symbols} sym`), el('span', 'pill', `${n.degree} links`));
      if (n.inCycle) meta.append(el('span', 'pill bad', 'cycle'));
      row.append(sw, name, meta);
      row.addEventListener('click', () => select(i));
      row.addEventListener('dblclick', () => {
        if (level === 'file') vscode.postMessage({ type: 'open', path: n.id });
        else { $('focus').value = n.id; setLevel('file'); load(); }
      });
      li.append(row);
      nodeList.append(li);
    });
  }

  function select(i) {
    selected = i;
    nodeList.querySelectorAll('.node-row').forEach((r, idx) => r.classList.toggle('selected', idx === i));
    if (i < 0 || !nodes[i]) return renderOverview();
    const n = nodes[i];
    if (level === 'file') { renderNode(n, null); vscode.postMessage({ type: 'select', id: n.id }); }
    else renderNode(n, null);
  }

  // ---------- Side panel ----------
  function list(title, rows, key) {
    const box = el('section', 'group');
    box.append(el('h3', null, `${title} (${rows.length})`));
    if (!rows.length) box.append(el('p', 'muted', 'None'));
    const ul = el('ul', 'links');
    for (const r of rows.slice(0, 40)) {
      const li = el('li');
      const b = el('button', 'link-row');
      b.type = 'button';
      b.append(el('span', 'path', r.path), el('span', 'kind ' + r.kind, r.kind === 'import' ? 'import' : `${r.symbols} sym`));
      b.addEventListener('click', () => { $('focus').value = r.path; setLevel('file'); load(); });
      li.append(b);
      ul.append(li);
    }
    if (rows.length > 40) ul.append(el('li', 'muted', `+${rows.length - 40} more`));
    box.append(ul);
    box.dataset.key = key;
    return box;
  }

  function renderNode(n, deps) {
    side.replaceChildren();
    const head = el('div', 'side-head');
    const sw = el('span', 'swatch');
    sw.style.setProperty('--c', colors[n.language] || colors.other);
    head.append(sw, el('h2', null, n.id));
    side.append(head);
    const meta = el('div', 'chips');
    for (const t of [n.language || 'other', `${n.symbols} symbols`, level === 'module' ? `${n.files} files` : `${n.degree} links`]) meta.append(el('span', 'chip', t));
    if (n.inCycle) meta.append(el('span', 'chip bad', 'import cycle'));
    side.append(meta);
    const acts = el('div', 'acts');
    if (level === 'file') {
      const open = el('button', 'btn'); open.type = 'button'; open.append(icon('go-to-file'), 'Open file');
      open.addEventListener('click', () => vscode.postMessage({ type: 'open', path: n.id }));
      acts.append(open);
    } else {
      const drill = el('button', 'btn'); drill.type = 'button'; drill.append(icon('folder-opened'), 'View files');
      drill.addEventListener('click', () => { $('focus').value = n.id; setLevel('file'); load(); });
      acts.append(drill);
    }
    const back = el('button', 'btn secondary'); back.type = 'button'; back.append(icon('list-flat'), 'Overview');
    back.addEventListener('click', () => select(-1));
    acts.append(back);
    side.append(acts);
    if (deps) {
      if (deps.importCycle) side.append(el('p', 'warn', `In an import cycle with ${deps.importCycle.length - 1} other file(s).`));
      side.append(list('Depends on', deps.dependsOn, 'out'), list('Used by', deps.usedBy, 'in'));
    } else if (level === 'file') side.append(el('p', 'muted', 'Loading dependencies…'));
  }

  function renderOverview() {
    side.replaceChildren();
    if (!stats) return;
    side.append(el('h2', null, 'Index Overview'));
    const byLang = new Map();
    for (const n of nodes) {
      const l = n.language || 'other';
      byLang.set(l, (byLang.get(l) || 0) + (level === 'module' ? (n.files || 1) : 1));
    }
    if (byLang.size) {
      const sec = el('section', 'group');
      sec.append(el('h3', null, 'Languages'));
      const lg = el('div', 'legend');
      for (const [l, count] of [...byLang.entries()].sort((a, b) => b[1] - a[1]).slice(0, 8)) {
        const s = el('span', 'swatch');
        s.style.setProperty('--c', colors[l] || colors.other);
        const row = el('span');
        row.append(s, `${l} (${count})`);
        lg.append(row);
      }
      sec.append(lg);
      side.append(sec);
    }
    const hubs = el('section', 'group');
    hubs.append(el('h3', null, 'Most referenced files'));
    if (!stats.hubs?.length) hubs.append(el('p', 'muted', 'No cross-file links yet.'));
    const ul = el('ul', 'links');
    for (const h of stats.hubs || []) {
      const b = el('button', 'link-row'); b.type = 'button';
      b.append(el('span', 'path', h.path), el('span', 'kind', `${h.dependents} refs`));
      b.addEventListener('click', () => { $('focus').value = h.path; setLevel('file'); load(); });
      const li = el('li'); li.append(b); ul.append(li);
    }
    hubs.append(ul);
    side.append(hubs);
    const cyc = el('section', 'group');
    cyc.append(el('h3', null, `Import cycles (${stats.cycles})`));
    if (!stats.importCycles?.length) cyc.append(el('p', 'muted', 'No import cycles detected.'));
    for (const c of (stats.importCycles || []).slice(0, 8)) cyc.append(el('p', 'cycle', c.join(' → ') + ' → ' + c[0]));
    side.append(cyc);
  }

  function renderChips(s, total, shown, skipped = [], skippedCount = 0) {
    const chips = $('chips');
    chips.replaceChildren();
    const totalSymbols = nodes.reduce((acc, n) => acc + (n.symbols || 0), 0);
    const add = (v, l, cls = '') => { const c = el('span', 'stat ' + cls); c.append(el('b', null, v), ` ${l}`); chips.append(c); };
    add(s.files.toLocaleString(), 'indexed files');
    add(s.modules.toLocaleString(), 'modules');
    if (totalSymbols) add(totalSymbols.toLocaleString(), 'symbols');
    add(s.importEdges.toLocaleString(), 'imports');
    add(s.referenceEdges.toLocaleString(), 'symbol links');
    add(String(s.cycles), s.cycles === 1 ? 'cycle' : 'cycles', s.cycles ? 'bad' : '');
    if (skippedCount > 0) {
      const d = el('details', 'stat skipped');
      const sum = el('summary');
      sum.append(el('span', 'codicon codicon-eye-closed'), el('b', null, skippedCount.toLocaleString()), ` ${skippedCount === 1 ? 'folder' : 'folders'} skipped`);
      const listEl = el('ul', 'skipped-list');
      for (const x of skipped) {
        const li = el('li');
        li.append(el('code', null, x.path + '/'), el('span', 'muted', x.reason));
        listEl.append(li);
      }
      if (skippedCount > skipped.length) listEl.append(el('li', 'muted', `and ${skippedCount - skipped.length} more`));
      d.append(sum, listEl);
      chips.append(d);
    }
    $('summary').textContent = `${s.files.toLocaleString()} indexed files across ${s.modules.toLocaleString()} modules` + (total > shown ? ` · showing top ${shown}` : '');
  }

  window.addEventListener('message', ({ data: m }) => {
    if (m.type === 'loading') { $('summary').textContent = 'Loading workspace index…'; document.body.classList.add('busy'); }
    else if (m.type === 'graph') {
      document.body.classList.remove('busy');
      readColors();
      setLevel(m.level);
      $('focus').value = m.focus;
      stats = m.data.stats;
      nodes = [...(m.data.nodes || [])].sort((a, b) => (b.degree - a.degree) || (b.symbols - a.symbols));
      selected = -1;
      renderChips(stats, m.data.total, nodes.length, m.data.skippedDirs ?? [], m.data.skippedDirCount ?? 0);
      empty.hidden = nodes.length > 0;
      empty.textContent = m.focus ? `Nothing matches "${m.focus}".` : 'No files are indexed yet.';
      renderList(m.data.total);
      renderOverview();
    } else if (m.type === 'details' && m.deps && selected >= 0 && nodes[selected]?.id === m.deps.path) renderNode(nodes[selected], m.deps);
    else if (m.type === 'index') showIndex(m.index);
    else if (m.type === 'error') { document.body.classList.remove('busy'); $('summary').textContent = m.text; }
  });
  new MutationObserver(() => { readColors(); }).observe(document.body, { attributes: true, attributeFilter: ['class'] });
  readColors();
  vscode.postMessage({ type: 'ready' });
})();
