// task-x project graph. Text is always inserted with textContent. The layout is a small force
// simulation (repulsion, edge springs, centering) drawn on a canvas; with reduced motion it
// settles before the first frame instead of animating.
(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const canvas = $('canvas'), ctx = canvas.getContext('2d'), side = $('side'), empty = $('empty');
  const el = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = text; return e; };
  const icon = (name) => { const i = el('span', `codicon codicon-${name}`); i.setAttribute('aria-hidden', 'true'); return i; };
  const reduced = matchMedia('(prefers-reduced-motion: reduce)').matches;
  const css = (v) => getComputedStyle(document.body).getPropertyValue(v).trim();
  const LANG_TOKEN = { rust: '--md-ext-rose', typescript: '--md-sys-color-primary', tsx: '--md-sys-color-primary', javascript: '--md-ext-amber', python: '--md-ext-green', go: '--md-ext-teal' };
  const saved = vscode.getState() || {};
  let level = saved.level || 'module', importsOnly = !!saved.importsOnly;
  let nodes = [], edges = [], selected = -1, hover = -1, alpha = 0, frame = 0;
  let view = { x: 0, y: 0, k: 1 };
  let colors = {};

  function readColors() {
    colors = {
      edge: css('--md-sys-color-outline-variant'),
      edgeImport: css('--md-sys-color-outline'),
      label: css('--md-sys-color-on-surface'),
      halo: css('--vscode-editor-background') || css('--md-sys-color-surface-container-lowest'),
      cycle: css('--md-sys-color-error'),
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
  // Index progress: the button spins and shows the file count while the workspace is scanned.
  function showIndex(ix) {
    const busy = ix?.state === 'indexing';
    reindex.disabled = busy;
    reindex.classList.toggle('busy', busy);
    reindex.querySelector('.codicon').className = `codicon ${busy ? 'codicon-loading codicon-modifier-spin' : 'codicon-refresh'}`;
    $('reindexText').textContent = !busy ? 'Reindex' : ix.phase === 'graph' ? 'Building graph…' : `Indexing ${ix.files.toLocaleString()} files…`;
    if (busy) $('summary').textContent = ix.phase === 'graph' ? 'Building the graph…' : `Indexing: ${ix.files.toLocaleString()} files seen, ${ix.parsed.toLocaleString()} parsed`;
    else if (ix?.state === 'failed') $('summary').textContent = `Indexing failed: ${ix.error}`;
  }

  // ---------- Layout ----------
  function size() {
    const r = canvas.getBoundingClientRect(), dpr = devicePixelRatio || 1;
    canvas.width = Math.max(1, r.width * dpr);
    canvas.height = Math.max(1, r.height * dpr);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    return r;
  }
  const radius = (n) => 4 + Math.min(14, Math.sqrt(n.degree) * 2 + (level === 'module' ? Math.sqrt(n.files) : 0));

  function setGraph(data) {
    const r = size();
    const old = new Map(nodes.map((n) => [n.id, n]));
    nodes = data.nodes.map((n, i) => {
      const prev = old.get(n.id);
      const a = (i / Math.max(1, data.nodes.length)) * Math.PI * 2, d = 40 + 8 * Math.sqrt(i);
      return { ...n, x: prev ? prev.x : Math.cos(a) * d, y: prev ? prev.y : Math.sin(a) * d, vx: 0, vy: 0, r: 0 };
    });
    for (const n of nodes) n.r = radius(n);
    edges = data.edges.map((e) => ({ ...e, a: nodes[e.from], b: nodes[e.to] }));
    selected = -1;
    alpha = 1;
    // Settle most of the layout before the first frame, fit it to the canvas, then let it finish moving.
    for (let i = 0; i < (reduced ? 400 : 160) && alpha > 0.02; i++) tick();
    fit(r);
    start();
  }

  function fit(r = canvas.getBoundingClientRect()) {
    if (!nodes.length) return;
    let x0 = Infinity, y0 = Infinity, x1 = -Infinity, y1 = -Infinity;
    for (const n of nodes) { x0 = Math.min(x0, n.x - n.r); y0 = Math.min(y0, n.y - n.r); x1 = Math.max(x1, n.x + n.r); y1 = Math.max(y1, n.y + n.r); }
    const pad = 48, w = Math.max(1, x1 - x0), h = Math.max(1, y1 - y0);
    const k = Math.min(2, Math.max(0.15, Math.min((r.width - pad * 2) / w, (r.height - pad * 2) / h)));
    view = { k, x: r.width / 2 - ((x0 + x1) / 2) * k, y: r.height / 2 - ((y0 + y1) / 2) * k };
  }

  function tick() {
    const n = nodes.length, strength = 900 * alpha;
    for (let i = 0; i < n; i++) {
      const a = nodes[i];
      for (let j = i + 1; j < n; j++) {
        const b = nodes[j];
        let dx = b.x - a.x, dy = b.y - a.y, d2 = dx * dx + dy * dy;
        if (d2 > 160000) continue;
        if (d2 < 1) { dx = Math.random() - 0.5; dy = Math.random() - 0.5; d2 = 1; }
        const f = strength / d2, d = Math.sqrt(d2);
        a.vx -= (dx / d) * f; a.vy -= (dy / d) * f;
        b.vx += (dx / d) * f; b.vy += (dy / d) * f;
      }
    }
    for (const e of edges) {
      const dx = e.b.x - e.a.x, dy = e.b.y - e.a.y, d = Math.sqrt(dx * dx + dy * dy) || 1;
      const rest = 50 + e.a.r + e.b.r, f = ((d - rest) / d) * 0.06 * alpha * (e.kind === 'import' ? 1.4 : 1);
      e.a.vx += dx * f; e.a.vy += dy * f;
      e.b.vx -= dx * f; e.b.vy -= dy * f;
    }
    for (const p of nodes) {
      if (p.fixed) { p.vx = p.vy = 0; continue; }
      p.vx -= p.x * 0.012 * alpha; p.vy -= p.y * 0.012 * alpha;
      p.vx *= 0.6; p.vy *= 0.6;
      p.x += p.vx; p.y += p.vy;
    }
    alpha *= 0.985;
  }

  function start() {
    if (frame) return;
    const step = () => {
      if (!reduced && alpha > 0.02) tick();
      draw();
      frame = alpha > 0.02 && !reduced ? requestAnimationFrame(step) : 0;
    };
    frame = requestAnimationFrame(step);
  }

  // ---------- Drawing ----------
  const toScreen = (n) => [n.x * view.k + view.x, n.y * view.k + view.y];
  const neighbors = (i) => new Set(edges.filter((e) => e.from === i || e.to === i).flatMap((e) => [e.from, e.to]));
  const label = (n) => (level === 'module' ? n.id : n.id.split('/').pop());

  function draw() {
    const r = canvas.getBoundingClientRect();
    ctx.clearRect(0, 0, r.width, r.height);
    const focus = selected >= 0 ? selected : hover;
    const near = focus >= 0 ? neighbors(focus) : null;
    ctx.lineCap = 'round';
    for (const e of edges) {
      const on = near && (e.from === focus || e.to === focus);
      const [x1, y1] = toScreen(e.a), [x2, y2] = toScreen(e.b);
      ctx.globalAlpha = near ? (on ? 0.95 : 0.08) : e.kind === 'import' ? 0.55 : 0.28;
      ctx.strokeStyle = on ? colors.select : e.kind === 'import' ? colors.edgeImport : colors.edge;
      ctx.lineWidth = Math.min(3, 0.6 + Math.log2(1 + e.weight) * 0.5) * (on ? 1.5 : 1);
      ctx.setLineDash(e.kind === 'import' ? [] : [3, 3]);
      ctx.beginPath(); ctx.moveTo(x1, y1); ctx.lineTo(x2, y2); ctx.stroke();
      if (e.kind === 'import' && (on || view.k > 0.9)) {
        // Arrowhead at the edge of the target node.
        const a = Math.atan2(y2 - y1, x2 - x1), tr = e.b.r * view.k + 2, tx = x2 - Math.cos(a) * tr, ty = y2 - Math.sin(a) * tr;
        ctx.setLineDash([]);
        ctx.beginPath(); ctx.moveTo(tx, ty);
        ctx.lineTo(tx - Math.cos(a - 0.45) * 7, ty - Math.sin(a - 0.45) * 7);
        ctx.lineTo(tx - Math.cos(a + 0.45) * 7, ty - Math.sin(a + 0.45) * 7);
        ctx.closePath(); ctx.fillStyle = ctx.strokeStyle; ctx.fill();
      }
    }
    ctx.setLineDash([]);
    const ranked = [...nodes.keys()].sort((a, b) => nodes[b].degree - nodes[a].degree);
    const labelled = new Set(ranked.slice(0, view.k > 1.4 ? 60 : view.k > 0.8 ? 25 : 12));
    nodes.forEach((n, i) => {
      const [x, y] = toScreen(n), rr = Math.max(2.5, n.r * view.k);
      ctx.globalAlpha = near && !near.has(i) ? 0.2 : 1;
      ctx.fillStyle = colors[n.language] || colors.other;
      ctx.beginPath(); ctx.arc(x, y, rr, 0, Math.PI * 2); ctx.fill();
      if (n.inCycle) { ctx.strokeStyle = colors.cycle; ctx.lineWidth = 2; ctx.stroke(); }
      if (i === focus) { ctx.strokeStyle = colors.select; ctx.lineWidth = 3; ctx.beginPath(); ctx.arc(x, y, rr + 4, 0, Math.PI * 2); ctx.stroke(); }
      if (labelled.has(i) || i === focus || (near && near.has(i))) {
        ctx.font = `${i === focus ? 600 : 500} 11px ${css('--vscode-font-family') || 'system-ui'}`;
        ctx.lineWidth = 3; ctx.strokeStyle = colors.halo; ctx.fillStyle = colors.label;
        ctx.strokeText(label(n), x + rr + 4, y + 4); ctx.fillText(label(n), x + rr + 4, y + 4);
      }
    });
    ctx.globalAlpha = 1;
  }

  // ---------- Interaction: drag nodes, pan, zoom, select ----------
  const hit = (mx, my) => {
    for (let i = nodes.length - 1; i >= 0; i--) {
      const [x, y] = toScreen(nodes[i]), rr = Math.max(6, nodes[i].r * view.k + 3);
      if ((mx - x) ** 2 + (my - y) ** 2 <= rr * rr) return i;
    }
    return -1;
  };
  let drag = null;
  canvas.addEventListener('pointerdown', (e) => {
    const i = hit(e.offsetX, e.offsetY);
    canvas.setPointerCapture(e.pointerId);
    drag = { i, x: e.offsetX, y: e.offsetY, moved: false, vx: view.x, vy: view.y };
    if (i >= 0) nodes[i].fixed = true;
  });
  canvas.addEventListener('pointermove', (e) => {
    if (!drag) {
      const h = hit(e.offsetX, e.offsetY);
      if (h !== hover) { hover = h; canvas.style.cursor = h >= 0 ? 'pointer' : 'grab'; canvas.title = h >= 0 ? nodes[h].id : ''; draw(); }
      return;
    }
    const dx = e.offsetX - drag.x, dy = e.offsetY - drag.y;
    if (Math.abs(dx) + Math.abs(dy) > 3) drag.moved = true;
    if (drag.i >= 0) {
      const n = nodes[drag.i];
      n.x = (e.offsetX - view.x) / view.k; n.y = (e.offsetY - view.y) / view.k;
      alpha = Math.max(alpha, 0.3); start();
    } else { view.x = drag.vx + dx; view.y = drag.vy + dy; canvas.style.cursor = 'grabbing'; }
    draw();
  });
  canvas.addEventListener('pointerup', () => {
    if (!drag) return;
    if (drag.i >= 0) nodes[drag.i].fixed = false;
    if (!drag.moved) select(drag.i);
    drag = null;
    canvas.style.cursor = 'grab';
  });
  canvas.addEventListener('dblclick', (e) => {
    const i = hit(e.offsetX, e.offsetY);
    if (i >= 0 && level === 'file') vscode.postMessage({ type: 'open', path: nodes[i].id });
    else if (i >= 0) { $('focus').value = nodes[i].id; setLevel('file'); load(); }
  });
  canvas.addEventListener('wheel', (e) => {
    e.preventDefault();
    const k = Math.min(4, Math.max(0.15, view.k * Math.exp(-e.deltaY * 0.0015)));
    view.x = e.offsetX - ((e.offsetX - view.x) * k) / view.k;
    view.y = e.offsetY - ((e.offsetY - view.y) * k) / view.k;
    view.k = k;
    draw();
  }, { passive: false });
  addEventListener('resize', () => { size(); fit(); draw(); });

  // ---------- Side panel ----------
  function select(i) {
    selected = i;
    draw();
    if (i < 0) return renderOverview();
    const n = nodes[i];
    if (level === 'file') { renderNode(n, null); vscode.postMessage({ type: 'select', id: n.id }); }
    else renderNode(n, null);
  }
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
      b.addEventListener('click', () => { $('focus').value = r.path; load(); });
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
    }
    const focusBtn = el('button', 'btn secondary'); focusBtn.type = 'button'; focusBtn.append(icon('target'), 'Focus');
    focusBtn.addEventListener('click', () => { $('focus').value = n.id; load(); });
    acts.append(focusBtn);
    side.append(acts);
    if (deps) {
      if (deps.importCycle) side.append(el('p', 'warn', `In an import cycle with ${deps.importCycle.length - 1} other file(s).`));
      side.append(list('Depends on', deps.dependsOn, 'out'), list('Used by', deps.usedBy, 'in'));
    } else if (level === 'file') side.append(el('p', 'muted', 'Loading dependencies…'));
  }
  let stats = null;
  function renderOverview() {
    side.replaceChildren();
    if (!stats) return;
    side.append(el('h2', null, 'Overview'));
    const hubs = el('section', 'group');
    hubs.append(el('h3', null, 'Most used files'));
    const ul = el('ul', 'links');
    for (const h of stats.hubs) {
      const b = el('button', 'link-row'); b.type = 'button';
      b.append(el('span', 'path', h.path), el('span', 'kind', `${h.dependents} users`));
      b.addEventListener('click', () => { $('focus').value = h.path; setLevel('file'); load(); });
      const li = el('li'); li.append(b); ul.append(li);
    }
    hubs.append(ul);
    side.append(hubs);
    const cyc = el('section', 'group');
    cyc.append(el('h3', null, `Import cycles (${stats.cycles})`));
    if (!stats.importCycles.length) cyc.append(el('p', 'muted', 'No import cycles.'));
    for (const c of stats.importCycles.slice(0, 8)) cyc.append(el('p', 'cycle', c.join(' → ') + ' → ' + c[0]));
    side.append(cyc);
    const legend = el('section', 'group');
    legend.append(el('h3', null, 'Legend'));
    const lg = el('div', 'legend');
    for (const l of ['typescript', 'javascript', 'rust', 'python', 'go']) { const s = el('span', 'swatch'); s.style.setProperty('--c', colors[l]); const row = el('span'); row.append(s, l); lg.append(row); }
    const solid = el('span'); solid.append(el('span', 'line'), 'import'); const dash = el('span'); dash.append(el('span', 'line dashed'), 'symbol use');
    lg.append(solid, dash);
    legend.append(lg, el('p', 'muted', 'Drag to move, scroll to zoom, double-click to open or drill in.'));
    side.append(legend);
  }
  function renderChips(s, total, shown) {
    const chips = $('chips');
    chips.replaceChildren();
    const add = (v, l, cls = '') => { const c = el('span', 'stat ' + cls); c.append(el('b', null, v), ` ${l}`); chips.append(c); };
    add(s.files.toLocaleString(), 'files'); add(s.modules.toLocaleString(), 'modules');
    add(s.importEdges.toLocaleString(), 'imports'); add(s.referenceEdges.toLocaleString(), 'symbol links');
    add(String(s.cycles), s.cycles === 1 ? 'cycle' : 'cycles', s.cycles ? 'bad' : '');
    $('summary').textContent = total > shown ? `Showing the ${shown} most connected of ${total} ${level === 'module' ? 'modules' : 'files'}` : `${shown} ${level === 'module' ? 'modules' : 'files'}`;
  }

  window.addEventListener('message', ({ data: m }) => {
    if (m.type === 'loading') { $('summary').textContent = 'Building the graph…'; document.body.classList.add('busy'); }
    else if (m.type === 'graph') {
      document.body.classList.remove('busy');
      readColors();
      setLevel(m.level);
      $('focus').value = m.focus;
      stats = m.data.stats;
      renderChips(stats, m.data.total, m.data.nodes.length);
      empty.hidden = m.data.nodes.length > 0;
      empty.textContent = m.focus ? `Nothing matches "${m.focus}".` : 'No files are indexed yet.';
      setGraph(m.data);
      renderOverview();
    } else if (m.type === 'details' && m.deps && selected >= 0 && nodes[selected]?.id === m.deps.path) renderNode(nodes[selected], m.deps);
    else if (m.type === 'index') showIndex(m.index);
    else if (m.type === 'error') { document.body.classList.remove('busy'); $('summary').textContent = m.text; }
  });
  new MutationObserver(() => { readColors(); draw(); }).observe(document.body, { attributes: true, attributeFilter: ['class'] });
  readColors();
  vscode.postMessage({ type: 'ready' });
})();
