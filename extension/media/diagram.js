(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const NS = 'http://www.w3.org/2000/svg';
  const NODE_W = 168;
  const NODE_H = 52;
  const GRID = 12;

  const svg = $('svg');
  const viewport = $('viewport');
  const edgesLayer = $('edgesLayer');
  const draftLayer = $('draftLayer');
  const nodesLayer = $('nodesLayer');
  const inlineEdit = $('inlineEdit');
  const statusBar = $('statusBar');
  const mermaidText = $('mermaidText');

  const SHAPE_SYNTAX = {
    rect: ['[', ']'],
    round: ['(', ')'],
    stadium: ['([', '])'],
    diamond: ['{', '}'],
    cylinder: ['[(', ')]'],
    subroutine: ['[[', ']]'],
    trapezoid: ['[/', '/]'],
  };

  let graph = { direction: 'LR', nodes: [], edges: [] };
  let sel = null; // { kind: 'node' | 'edge', id: string }
  let pan = { x: 40, y: 40, scale: 1 };
  let undoStack = [];
  let redoStack = [];
  let seq = 1;
  let drag = null; // { mode: 'node' | 'pan' | 'connect', ... }

  const svgEl = (tag, attrs = {}) => {
    const e = document.createElementNS(NS, tag);
    for (const [k, v] of Object.entries(attrs)) e.setAttribute(k, String(v));
    return e;
  };
  const snap = (v) => Math.round(v / GRID) * GRID;
  const cloneGraph = () => JSON.parse(JSON.stringify(graph));
  const setStatus = (text) => { statusBar.textContent = text || ''; };

  function pushUndo() {
    undoStack.push(cloneGraph());
    if (undoStack.length > 60) undoStack.shift();
    redoStack = [];
    syncHistoryButtons();
  }
  function syncHistoryButtons() {
    $('undoBtn').disabled = undoStack.length === 0;
    $('redoBtn').disabled = redoStack.length === 0;
    $('delBtn').disabled = !sel;
  }

  function sanitizeId(raw, fallback) {
    const clean = String(raw || '').replace(/[^a-zA-Z0-9_]/g, '_').replace(/^(\d)/, '_$1');
    return clean || fallback;
  }
  function escapeLabel(text) {
    return String(text || '').replace(/"/g, '&quot;').replace(/\r?\n/g, ' ').trim();
  }
  function toMermaid(g = graph) {
    const dir = g.direction === 'TD' ? 'TD' : 'LR';
    const lines = [`flowchart ${dir}`];
    const used = new Map();
    const seen = new Set();
    g.nodes.forEach((n, i) => {
      let id = sanitizeId(n.id, `N${i + 1}`);
      let k = 2;
      while (seen.has(id)) id = `${sanitizeId(n.id, `N${i + 1}`)}_${k++}`;
      seen.add(id);
      used.set(n.id, id);
      const [o, c] = SHAPE_SYNTAX[n.shape] || SHAPE_SYNTAX.rect;
      lines.push(`    ${id}${o}"${escapeLabel(n.label || id)}"${c}`);
    });
    for (const e of g.edges) {
      const from = used.get(e.from);
      const to = used.get(e.to);
      if (!from || !to) continue;
      const arrow = e.style === 'dashed' ? '-.->' : e.style === 'thick' ? '==>' : e.style === 'open' ? '---' : '-->';
      const label = escapeLabel(e.label);
      lines.push(label ? `    ${from} ${arrow}|"${label}"| ${to}` : `    ${from} ${arrow} ${to}`);
    }
    for (const n of g.nodes) {
      if (n.color && /^#[0-9a-fA-F]{3,8}$/.test(n.color)) {
        const id = used.get(n.id);
        if (id) lines.push(`    style ${id} stroke:${n.color},stroke-width:2px`);
      }
    }
    return lines.join('\n');
  }

  function parseMermaidLocal(src) {
    const lines = src.replace(/\r\n?/g, '\n').split('\n');
    const first = (lines.find((l) => l.trim() && !l.trim().startsWith('%%')) || '').trim();
    if (/^sequenceDiagram\b/.test(first)) {
      const nmap = new Map();
      const edges = [];
      const ensure = (id, label) => {
        if (!nmap.has(id)) nmap.set(id, { id, label: label || id, shape: 'round', x: 0, y: 0 });
        else if (label && nmap.get(id).label === id) nmap.get(id).label = label;
      };
      let ei = 0;
      for (const raw of lines) {
        const l = raw.trim();
        const pm = /^(?:participant|actor)\s+([a-zA-Z_][\w-]*)(?:\s+as\s+(.+))?$/.exec(l);
        if (pm) { ensure(pm[1], pm[2]?.trim()); continue; }
        const mm = /^([a-zA-Z_][\w-]*?)\s*(-->>|->>|-->|->|--x|-x|--\)|-\))\s*([a-zA-Z_][\w-]*)\s*:\s*(.*)$/.exec(l);
        if (mm) {
          ensure(mm[1]); ensure(mm[3]);
          edges.push({ id: `e${++ei}`, from: mm[1], to: mm[3], label: mm[4].trim(), style: mm[2].startsWith('--') ? 'dashed' : 'solid' });
        }
      }
      return autoLayout({ direction: 'LR', nodes: [...nmap.values()], edges });
    }
    const dm = /^(?:flowchart|graph)\s+(TD|TB|BT|LR|RL)\b/i.exec(first);
    const direction = dm && (dm[1].toUpperCase() === 'TD' || dm[1].toUpperCase() === 'TB' || dm[1].toUpperCase() === 'BT') ? 'TD' : 'LR';
    const nmap = new Map();
    const edges = [];
    const unq = (t) => { const s = t.trim(); return (s.startsWith('"') && s.endsWith('"')) || (s.startsWith("'") && s.endsWith("'")) ? s.slice(1, -1).replace(/&quot;/g, '"') : s; };
    const parseTok = (tok) => {
      const s = tok.trim();
      const pats = [
        [/^([a-zA-Z_][\w-]*)\s*\(\[([\s\S]+)\]\)$/, 'stadium'],
        [/^([a-zA-Z_][\w-]*)\s*\[\(([\s\S]+)\)\]$/, 'cylinder'],
        [/^([a-zA-Z_][\w-]*)\s*\[\[([\s\S]+)\]\]$/, 'subroutine'],
        [/^([a-zA-Z_][\w-]*)\s*\[\/([\s\S]+)\/\]$/, 'trapezoid'],
        [/^([a-zA-Z_][\w-]*)\s*\{([\s\S]+)\}$/, 'diamond'],
        [/^([a-zA-Z_][\w-]*)\s*\(([\s\S]+)\)$/, 'round'],
        [/^([a-zA-Z_][\w-]*)\s*\[([\s\S]+)\]$/, 'rect'],
      ];
      for (const [re, shape] of pats) { const m = re.exec(s); if (m) return { id: m[1], label: unq(m[2]), shape }; }
      const b = /^([a-zA-Z_][\w-]*)$/.exec(s);
      return b ? { id: b[1] } : null;
    };
    const upsert = (t) => {
      const cur = nmap.get(t.id);
      if (cur) { if (t.label) cur.label = t.label; if (t.shape) cur.shape = t.shape; return; }
      nmap.set(t.id, { id: t.id, label: t.label || t.id, shape: t.shape || 'rect', x: 0, y: 0 });
    };
    const edgeRe = /^(.+?)\s*(-->|-\.->|==>|---|--[^>]+-->|-\.[^>]+-\.->|==[^>]+==>)\s*(?:\|([^|]*)\|\s*)?(.+)$/;
    let ei = 0;
    for (const raw of lines) {
      const l = raw.trim().replace(/;$/, '');
      if (!l || l.startsWith('%%') || /^(?:flowchart|graph|subgraph|end|classDef|class|style|linkStyle|click|direction)\b/i.test(l)) continue;
      const em = edgeRe.exec(l);
      if (em) {
        const a = parseTok(em[1]), b = parseTok(em[4]);
        if (a && b) {
          upsert(a); upsert(b);
          const arr = em[2];
          let label = (em[3] || '').trim();
          let style = 'solid';
          if (arr.includes('-.')) { style = 'dashed'; if (!label) label = arr.replace(/^-\.\s*/, '').replace(/\s*-\.->$/, '').trim(); }
          else if (arr.includes('==')) { style = 'thick'; if (!label) label = arr.replace(/^==\s*/, '').replace(/\s*==>$/, '').trim(); }
          else if (arr === '---') style = 'open';
          else if (!label && arr.length > 3) label = arr.replace(/^--\s*/, '').replace(/\s*-->$/, '').trim();
          edges.push({ id: `e${++ei}`, from: a.id, to: b.id, label: unq(label), style });
          continue;
        }
      }
      const single = parseTok(l);
      if (single) upsert(single);
    }
    return autoLayout({ direction, nodes: [...nmap.values()], edges });
  }

  function autoLayout(g) {
    const nodes = g.nodes.map((n) => ({ ...n }));
    const edges = g.edges.map((e) => ({ ...e }));
    if (!nodes.length) return { direction: g.direction, nodes, edges };
    const indeg = new Map(nodes.map((n) => [n.id, 0]));
    const adj = new Map(nodes.map((n) => [n.id, []]));
    for (const e of edges) {
      if (!adj.has(e.from) || !indeg.has(e.to) || e.from === e.to) continue;
      adj.get(e.from).push(e.to);
      indeg.set(e.to, indeg.get(e.to) + 1);
    }
    const layer = new Map();
    const q = nodes.filter((n) => indeg.get(n.id) === 0).map((n) => n.id);
    if (!q.length) q.push(nodes[0].id);
    const seen = new Set();
    while (q.length) {
      const id = q.shift();
      if (seen.has(id)) continue;
      seen.add(id);
      const cur = layer.get(id) ?? 0;
      layer.set(id, cur);
      for (const nx of adj.get(id)) {
        layer.set(nx, Math.max(layer.get(nx) ?? 0, cur + 1));
        const d = indeg.get(nx) - 1;
        indeg.set(nx, d);
        if (d <= 0 && !seen.has(nx)) q.push(nx);
      }
    }
    const groups = new Map();
    for (const n of nodes) {
      const l = layer.get(n.id) ?? 0;
      if (!groups.has(l)) groups.set(l, []);
      groups.get(l).push(n);
    }
    const horiz = g.direction === 'LR';
    [...groups.keys()].sort((a, b) => a - b).forEach((l, li) => {
      groups.get(l).forEach((n, gi) => {
        n.x = horiz ? 60 + li * (NODE_W + 76) : 60 + gi * (NODE_W + 64);
        n.y = horiz ? 60 + gi * (NODE_H + 56) : 60 + li * (NODE_H + 68);
      });
    });
    return { direction: g.direction, nodes, edges };
  }

  function screenToWorld(clientX, clientY) {
    const r = svg.getBoundingClientRect();
    return { x: (clientX - r.left - pan.x) / pan.scale, y: (clientY - r.top - pan.y) / pan.scale };
  }
  function worldToScreen(wx, wy) {
    return { x: wx * pan.scale + pan.x, y: wy * pan.scale + pan.y };
  }
  function applyTransform() {
    viewport.setAttribute('transform', `translate(${pan.x} ${pan.y}) scale(${pan.scale})`);
    $('zoomVal').textContent = `${Math.round(pan.scale * 100)}%`;
  }
  function fitToView() {
    if (!graph.nodes.length) { pan = { x: 40, y: 40, scale: 1 }; applyTransform(); return; }
    const r = svg.getBoundingClientRect();
    let minX = Infinity, minY = Infinity, maxX = -Infinity, maxY = -Infinity;
    for (const n of graph.nodes) {
      minX = Math.min(minX, n.x); minY = Math.min(minY, n.y);
      maxX = Math.max(maxX, n.x + NODE_W); maxY = Math.max(maxY, n.y + NODE_H);
    }
    const pad = 48;
    const w = Math.max(1, maxX - minX + pad * 2);
    const h = Math.max(1, maxY - minY + pad * 2);
    const scale = Math.max(0.25, Math.min(1.25, Math.min(r.width / w, r.height / h)));
    pan = {
      scale,
      x: (r.width - (maxX - minX) * scale) / 2 - minX * scale,
      y: (r.height - (maxY - minY) * scale) / 2 - minY * scale,
    };
    applyTransform();
  }

  function nodeCenter(n) { return { x: n.x + NODE_W / 2, y: n.y + NODE_H / 2 }; }
  function edgeAnchors(a, b) {
    const ca = nodeCenter(a), cb = nodeCenter(b);
    const dx = cb.x - ca.x, dy = cb.y - ca.y;
    if (Math.abs(dx) >= Math.abs(dy)) {
      return dx >= 0
        ? [{ x: a.x + NODE_W, y: ca.y }, { x: b.x, y: cb.y }, 'h']
        : [{ x: a.x, y: ca.y }, { x: b.x + NODE_W, y: cb.y }, 'h'];
    }
    return dy >= 0
      ? [{ x: ca.x, y: a.y + NODE_H }, { x: cb.x, y: b.y }, 'v']
      : [{ x: ca.x, y: a.y }, { x: cb.x, y: b.y + NODE_H }, 'v'];
  }
  function curvePath(p1, p2, axis) {
    if (axis === 'h') {
      const c = Math.max(40, Math.abs(p2.x - p1.x) * 0.45);
      const dir = p2.x >= p1.x ? 1 : -1;
      return `M ${p1.x} ${p1.y} C ${p1.x + c * dir} ${p1.y}, ${p2.x - c * dir} ${p2.y}, ${p2.x} ${p2.y}`;
    }
    const c = Math.max(36, Math.abs(p2.y - p1.y) * 0.45);
    const dir = p2.y >= p1.y ? 1 : -1;
    return `M ${p1.x} ${p1.y} C ${p1.x} ${p1.y + c * dir}, ${p2.x} ${p2.y - c * dir}, ${p2.x} ${p2.y}`;
  }

  function shapeElement(n) {
    const w = NODE_W, h = NODE_H;
    let el;
    switch (n.shape) {
      case 'round':
        el = svgEl('rect', { class: 'node-shape', x: 0, y: 0, width: w, height: h, rx: 10 });
        break;
      case 'stadium':
        el = svgEl('rect', { class: 'node-shape', x: 0, y: 0, width: w, height: h, rx: h / 2 });
        break;
      case 'diamond':
        el = svgEl('polygon', { class: 'node-shape', points: `${w / 2},0 ${w},${h / 2} ${w / 2},${h} 0,${h / 2}` });
        break;
      case 'cylinder':
        el = svgEl('path', { class: 'node-shape', d: `M 0 10 C 0 2, ${w} 2, ${w} 10 L ${w} ${h - 10} C ${w} ${h - 2}, 0 ${h - 2}, 0 ${h - 10} Z M 0 10 C 0 18, ${w} 18, ${w} 10` });
        break;
      case 'subroutine':
        el = svgEl('path', { class: 'node-shape', d: `M 0 0 H ${w} V ${h} H 0 Z M 10 0 V ${h} M ${w - 10} 0 V ${h}` });
        break;
      case 'trapezoid':
        el = svgEl('polygon', { class: 'node-shape', points: `14,0 ${w},0 ${w - 14},${h} 0,${h}` });
        break;
      default:
        el = svgEl('rect', { class: 'node-shape', x: 0, y: 0, width: w, height: h, rx: 4 });
    }
    if (n.color) el.style.stroke = n.color;
    return el;
  }

  function render() {
    applyTransform();
    edgesLayer.replaceChildren();
    nodesLayer.replaceChildren();
    const byId = new Map(graph.nodes.map((n) => [n.id, n]));

    for (const e of graph.edges) {
      const a = byId.get(e.from), b = byId.get(e.to);
      if (!a || !b) continue;
      const isSel = sel?.kind === 'edge' && sel.id === e.id;
      const g = svgEl('g', { class: `flow-edge ${e.style}${isSel ? ' selected' : ''}`, 'data-id': e.id });
      const [p1, p2, axis] = edgeAnchors(a, b);
      const d = curvePath(p1, p2, axis);
      const hit = svgEl('path', { class: 'edge-hit', d });
      const path = svgEl('path', { class: 'edge-path', d });
      if (e.style !== 'open') path.setAttribute('marker-end', isSel ? 'url(#arrow-sel)' : 'url(#arrow)');
      g.append(hit, path);

      const mx = (p1.x + p2.x) / 2, my = (p1.y + p2.y) / 2;
      if (e.label) {
        const tw = Math.min(200, Math.max(36, e.label.length * 6.8 + 16));
        const bg = svgEl('rect', { class: 'edge-label-bg', x: mx - tw / 2, y: my - 11, width: tw, height: 22 });
        const txt = svgEl('text', { class: 'edge-label', x: mx, y: my });
        txt.textContent = e.label.length > 26 ? `${e.label.slice(0, 25)}…` : e.label;
        bg.addEventListener('dblclick', (ev) => { ev.stopPropagation(); startInlineEdit('edge', e.id, mx, my, e.label); });
        g.append(bg, txt);
      }
      hit.addEventListener('pointerdown', (ev) => { ev.stopPropagation(); select({ kind: 'edge', id: e.id }); });
      hit.addEventListener('dblclick', (ev) => { ev.stopPropagation(); startInlineEdit('edge', e.id, mx, my, e.label); });
      edgesLayer.append(g);
    }

    for (const n of graph.nodes) {
      const isSel = sel?.kind === 'node' && sel.id === n.id;
      const g = svgEl('g', { class: `flow-node${isSel ? ' selected' : ''}`, transform: `translate(${n.x} ${n.y})`, 'data-id': n.id });
      g.append(shapeElement(n));
      const label = svgEl('text', { class: 'node-text', x: NODE_W / 2, y: NODE_H / 2 });
      const text = n.label || n.id;
      label.textContent = text.length > 22 ? `${text.slice(0, 21)}…` : text;
      g.append(label);

      const ports = [
        { cx: NODE_W, cy: NODE_H / 2 },
        { cx: 0, cy: NODE_H / 2 },
        { cx: NODE_W / 2, cy: NODE_H },
        { cx: NODE_W / 2, cy: 0 },
      ];
      for (const pt of ports) {
        const c = svgEl('circle', { class: 'port', cx: pt.cx, cy: pt.cy, r: 5 });
        c.addEventListener('pointerdown', (ev) => {
          ev.stopPropagation();
          select({ kind: 'node', id: n.id });
          svg.classList.add('connecting');
          drag = { mode: 'connect', from: n.id, sx: n.x + pt.cx, sy: n.y + pt.cy };
        });
        g.append(c);
      }

      g.addEventListener('pointerdown', (ev) => {
        if (ev.target.classList.contains('port')) return;
        ev.stopPropagation();
        select({ kind: 'node', id: n.id });
        const w = screenToWorld(ev.clientX, ev.clientY);
        drag = { mode: 'node', id: n.id, dx: w.x - n.x, dy: w.y - n.y, moved: false };
      });
      g.addEventListener('dblclick', (ev) => {
        ev.stopPropagation();
        startInlineEdit('node', n.id, n.x + NODE_W / 2, n.y + NODE_H / 2, n.label);
      });
      nodesLayer.append(g);
    }

    $('dirBtn').textContent = graph.direction;
    if (document.activeElement !== mermaidText) mermaidText.value = toMermaid(graph);
    syncInspector();
    syncHistoryButtons();
  }

  function select(next) {
    sel = next;
    render();
  }

  function syncInspector() {
    const empty = $('inspEmpty'), nForm = $('inspNode'), eForm = $('inspEdge');
    empty.hidden = !!sel;
    nForm.hidden = sel?.kind !== 'node';
    eForm.hidden = sel?.kind !== 'edge';
    if (sel?.kind === 'node') {
      const n = graph.nodes.find((x) => x.id === sel.id);
      if (!n) return;
      $('nodeLabel').value = n.label;
      $('nodeId').value = n.id;
      $('nodeShape').value = n.shape;
      $('nodeColors').querySelectorAll('.swatch').forEach((s) => s.classList.toggle('active', (s.dataset.color || '') === (n.color || '')));
    } else if (sel?.kind === 'edge') {
      const e = graph.edges.find((x) => x.id === sel.id);
      if (!e) return;
      $('edgeLabel').value = e.label;
      $('edgeStyle').value = e.style;
    }
  }

  function startInlineEdit(kind, id, wx, wy, currentText) {
    const p = worldToScreen(wx, wy);
    inlineEdit.value = currentText || '';
    inlineEdit.style.left = `${Math.max(8, p.x - 75)}px`;
    inlineEdit.style.top = `${Math.max(8, p.y - 14)}px`;
    inlineEdit.hidden = false;
    inlineEdit.focus();
    inlineEdit.select();
    const finish = (commit) => {
      if (inlineEdit.hidden) return;
      inlineEdit.hidden = true;
      inlineEdit.onblur = null;
      inlineEdit.onkeydown = null;
      if (!commit) return;
      const val = inlineEdit.value.trim();
      pushUndo();
      if (kind === 'node') {
        const n = graph.nodes.find((x) => x.id === id);
        if (n && val) n.label = val;
      } else {
        const e = graph.edges.find((x) => x.id === id);
        if (e) e.label = val;
      }
      render();
    };
    inlineEdit.onblur = () => finish(true);
    inlineEdit.onkeydown = (ev) => {
      if (ev.key === 'Enter') { ev.preventDefault(); finish(true); }
      else if (ev.key === 'Escape') { ev.preventDefault(); finish(false); }
    };
  }

  function addNode(shape = 'rect', wx, wy) {
    pushUndo();
    let id = `Node_${seq++}`;
    while (graph.nodes.some((n) => n.id === id)) id = `Node_${seq++}`;
    const r = svg.getBoundingClientRect();
    const pos = wx != null && wy != null ? { x: wx - NODE_W / 2, y: wy - NODE_H / 2 } : screenToWorld(r.left + r.width / 2, r.top + r.height / 2);
    const node = { id, label: `Step ${graph.nodes.length + 1}`, shape, x: snap(pos.x), y: snap(pos.y) };
    graph.nodes.push(node);
    sel = { kind: 'node', id: node.id };
    render();
    startInlineEdit('node', node.id, node.x + NODE_W / 2, node.y + NODE_H / 2, node.label);
  }

  function deleteSelected() {
    if (!sel) return;
    pushUndo();
    if (sel.kind === 'node') {
      graph.nodes = graph.nodes.filter((n) => n.id !== sel.id);
      graph.edges = graph.edges.filter((e) => e.from !== sel.id && e.to !== sel.id);
    } else {
      graph.edges = graph.edges.filter((e) => e.id !== sel.id);
    }
    sel = null;
    render();
  }

  // Canvas pointer events (pan, node drag, port connect)
  svg.addEventListener('pointerdown', (e) => {
    if (e.target.closest('.flow-node') || e.target.closest('.flow-edge')) return;
    sel = null;
    render();
    svg.classList.add('panning');
    drag = { mode: 'pan', sx: e.clientX, sy: e.clientY, px: pan.x, py: pan.y };
  });
  svg.addEventListener('dblclick', (e) => {
    if (e.target.closest('.flow-node') || e.target.closest('.flow-edge')) return;
    const w = screenToWorld(e.clientX, e.clientY);
    addNode('rect', w.x, w.y);
  });
  window.addEventListener('pointermove', (e) => {
    if (!drag) return;
    if (drag.mode === 'pan') {
      pan.x = drag.px + (e.clientX - drag.sx);
      pan.y = drag.py + (e.clientY - drag.sy);
      applyTransform();
    } else if (drag.mode === 'node') {
      if (!drag.moved) { pushUndo(); drag.moved = true; }
      const n = graph.nodes.find((x) => x.id === drag.id);
      if (!n) return;
      const w = screenToWorld(e.clientX, e.clientY);
      n.x = snap(w.x - drag.dx);
      n.y = snap(w.y - drag.dy);
      render();
    } else if (drag.mode === 'connect') {
      const w = screenToWorld(e.clientX, e.clientY);
      draftLayer.replaceChildren(svgEl('path', {
        class: 'draft-edge',
        d: `M ${drag.sx} ${drag.sy} L ${w.x} ${w.y}`,
        'marker-end': 'url(#arrow-sel)',
      }));
    }
  });
  window.addEventListener('pointerup', (e) => {
    if (!drag) return;
    if (drag.mode === 'connect') {
      draftLayer.replaceChildren();
      svg.classList.remove('connecting');
      const target = document.elementFromPoint(e.clientX, e.clientY)?.closest('.flow-node');
      const toId = target?.getAttribute('data-id');
      if (toId && toId !== drag.from) {
        pushUndo();
        const edge = { id: `e${Date.now().toString(36)}${seq++}`, from: drag.from, to: toId, label: '', style: 'solid' };
        graph.edges.push(edge);
        sel = { kind: 'edge', id: edge.id };
        render();
      }
    }
    svg.classList.remove('panning');
    drag = null;
  });

  // Wheel zoom
  svg.addEventListener('wheel', (e) => {
    e.preventDefault();
    const r = svg.getBoundingClientRect();
    const mx = e.clientX - r.left, my = e.clientY - r.top;
    const factor = e.deltaY < 0 ? 1.12 : 0.89;
    const next = Math.max(0.25, Math.min(3, pan.scale * factor));
    pan.x = mx - ((mx - pan.x) * next) / pan.scale;
    pan.y = my - ((my - pan.y) * next) / pan.scale;
    pan.scale = next;
    applyTransform();
  }, { passive: false });

  // Palette drag & drop + click
  document.querySelectorAll('.pal-item').forEach((item) => {
    item.addEventListener('click', () => addNode(item.dataset.shape));
    item.addEventListener('dragstart', (e) => e.dataTransfer.setData('text/plain', item.dataset.shape));
  });
  $('canvasWrap').addEventListener('dragover', (e) => e.preventDefault());
  $('canvasWrap').addEventListener('drop', (e) => {
    e.preventDefault();
    const shape = e.dataTransfer.getData('text/plain') || 'rect';
    const w = screenToWorld(e.clientX, e.clientY);
    addNode(shape, w.x, w.y);
  });

  // Inspector bindings
  $('nodeLabel').addEventListener('input', (e) => {
    if (sel?.kind !== 'node') return;
    const n = graph.nodes.find((x) => x.id === sel.id);
    if (n) { n.label = e.target.value; render(); }
  });
  $('nodeId').addEventListener('change', (e) => {
    if (sel?.kind !== 'node') return;
    const n = graph.nodes.find((x) => x.id === sel.id);
    const next = sanitizeId(e.target.value, n?.id || 'Node');
    if (!n || next === n.id || graph.nodes.some((x) => x.id === next)) { e.target.value = n?.id || ''; return; }
    pushUndo();
    const old = n.id;
    n.id = next;
    for (const ed of graph.edges) {
      if (ed.from === old) ed.from = next;
      if (ed.to === old) ed.to = next;
    }
    sel = { kind: 'node', id: next };
    render();
  });
  $('nodeShape').addEventListener('change', (e) => {
    if (sel?.kind !== 'node') return;
    pushUndo();
    const n = graph.nodes.find((x) => x.id === sel.id);
    if (n) { n.shape = e.target.value; render(); }
  });
  $('nodeColors').addEventListener('click', (e) => {
    const b = e.target.closest('.swatch');
    if (!b || sel?.kind !== 'node') return;
    pushUndo();
    const n = graph.nodes.find((x) => x.id === sel.id);
    if (n) { n.color = b.dataset.color || undefined; render(); }
  });
  $('edgeLabel').addEventListener('input', (e) => {
    if (sel?.kind !== 'edge') return;
    const ed = graph.edges.find((x) => x.id === sel.id);
    if (ed) { ed.label = e.target.value; render(); }
  });
  $('edgeStyle').addEventListener('change', (e) => {
    if (sel?.kind !== 'edge') return;
    pushUndo();
    const ed = graph.edges.find((x) => x.id === sel.id);
    if (ed) { ed.style = e.target.value; render(); }
  });

  // Toolbar actions
  $('autoBtn').addEventListener('click', () => {
    vscode.postMessage({ type: 'autoFlow', level: $('autoLevel').value, direction: graph.direction });
  });
  $('aiBtn').addEventListener('click', () => { $('aiPrompt').value = ''; $('aiDialog').showModal(); $('aiPrompt').focus(); });
  $('aiCancel').addEventListener('click', () => $('aiDialog').close());
  $('aiForm').addEventListener('submit', (e) => {
    e.preventDefault();
    const prompt = $('aiPrompt').value.trim();
    if (prompt) vscode.postMessage({ type: 'askAgent', prompt });
    $('aiDialog').close();
  });
  $('addNodeBtn').addEventListener('click', () => addNode('rect'));
  $('layoutBtn').addEventListener('click', () => { pushUndo(); graph = autoLayout(graph); render(); fitToView(); });
  $('dirBtn').addEventListener('click', () => {
    pushUndo();
    graph.direction = graph.direction === 'LR' ? 'TD' : 'LR';
    graph = autoLayout(graph);
    render();
    fitToView();
  });
  $('undoBtn').addEventListener('click', () => {
    if (!undoStack.length) return;
    redoStack.push(cloneGraph());
    graph = undoStack.pop();
    sel = null;
    render();
  });
  $('redoBtn').addEventListener('click', () => {
    if (!redoStack.length) return;
    undoStack.push(cloneGraph());
    graph = redoStack.pop();
    sel = null;
    render();
  });
  $('delBtn').addEventListener('click', deleteSelected);
  $('zoomOutBtn').addEventListener('click', () => { pan.scale = Math.max(0.25, pan.scale - 0.15); applyTransform(); });
  $('zoomInBtn').addEventListener('click', () => { pan.scale = Math.min(3, pan.scale + 0.15); applyTransform(); });
  $('fitBtn').addEventListener('click', fitToView);
  $('codeToggleBtn').addEventListener('click', () => {
    const d = $('codeDrawer');
    d.hidden = !d.hidden;
    if (!d.hidden) mermaidText.value = toMermaid(graph);
  });
  $('closeCodeBtn').addEventListener('click', () => { $('codeDrawer').hidden = true; });
  $('applyCodeBtn').addEventListener('click', () => {
    pushUndo();
    graph = parseMermaidLocal(mermaidText.value);
    sel = null;
    render();
    fitToView();
    setStatus('Synced Mermaid code to canvas');
  });
  $('copyBtn').addEventListener('click', () => vscode.postMessage({ type: 'copyText', text: toMermaid(graph) }));
  $('chatBtn').addEventListener('click', () => vscode.postMessage({ type: 'sendToChat', code: toMermaid(graph) }));
  $('saveBtn').addEventListener('click', () => vscode.postMessage({ type: 'saveMermaid', code: toMermaid(graph) }));
  $('svgBtn').addEventListener('click', () => {
    const clone = svg.cloneNode(true);
    clone.querySelector('.grid-bg')?.remove();
    clone.querySelectorAll('.port, .edge-hit').forEach((n) => n.remove());
    vscode.postMessage({ type: 'exportSvg', svg: new XMLSerializer().serializeToString(clone) });
  });

  // Keyboard shortcuts
  window.addEventListener('keydown', (e) => {
    if (e.target.matches('input, textarea, select')) return;
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'z') {
      e.preventDefault();
      if (e.shiftKey) $('redoBtn').click();
      else $('undoBtn').click();
    } else if (e.key === 'Delete' || e.key === 'Backspace') {
      if (sel) { e.preventDefault(); deleteSelected(); }
    }
  });

  window.addEventListener('message', (ev) => {
    const m = ev.data;
    if (m.type === 'load' && m.graph) {
      graph = m.graph;
      undoStack = [];
      redoStack = [];
      sel = null;
      render();
      requestAnimationFrame(fitToView);
    } else if (m.type === 'status') {
      setStatus(m.text);
    }
  });

  vscode.postMessage({ type: 'ready' });
})();
