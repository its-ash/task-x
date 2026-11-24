(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const NS = 'http://www.w3.org/2000/svg';
  const MIN_NODE_W = 168;
  const MAX_NODE_W = 280;
  const NODE_W = 168;
  const NODE_H = 56;
  const GAP_X = 84;
  const GAP_Y = 72;
  const GRID = 12;

  function nodeW(n) {
    const len = String(n?.label || n?.id || '').length;
    const pad = n?.shape === 'diamond' ? 76 : n?.shape === 'subroutine' ? 56 : 44;
    return Math.max(MIN_NODE_W, Math.min(MAX_NODE_W, Math.round(len * 7.4 + pad)));
  }

  const svg = $('svg');
  const viewport = $('viewport');
  const groupsLayer = $('groupsLayer');
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
    const emitNode = (n, i, indent) => {
      let id = sanitizeId(n.id, `N${i + 1}`);
      let k = 2;
      while (seen.has(id)) id = `${sanitizeId(n.id, `N${i + 1}`)}_${k++}`;
      seen.add(id);
      used.set(n.id, id);
      const [o, c] = SHAPE_SYNTAX[n.shape] || SHAPE_SYNTAX.rect;
      lines.push(`${indent}${id}${o}"${escapeLabel(n.label || id)}"${c}`);
    };
    if (g.groups?.length) {
      const grouped = new Set();
      for (const grp of g.groups) {
        const members = g.nodes.filter((n) => n.group === grp.id);
        if (!members.length) continue;
        const gid = sanitizeId(grp.id, 'Group');
        lines.push(`    subgraph ${gid}["${escapeLabel(grp.label || gid)}"]`);
        for (const n of members) {
          grouped.add(n.id);
          emitNode(n, g.nodes.indexOf(n), '        ');
        }
        lines.push('    end');
      }
      g.nodes.forEach((n, i) => { if (!grouped.has(n.id)) emitNode(n, i, '    '); });
    } else {
      g.nodes.forEach((n, i) => emitNode(n, i, '    '));
    }
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
    const gmap = new Map();
    const gstack = [];
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
      const curGroup = gstack[gstack.length - 1];
      const cur = nmap.get(t.id);
      if (cur) {
        if (t.label) cur.label = t.label;
        if (t.shape) cur.shape = t.shape;
        if (curGroup && !cur.group) cur.group = curGroup;
        return;
      }
      nmap.set(t.id, { id: t.id, label: t.label || t.id, shape: t.shape || 'rect', x: 0, y: 0, ...(curGroup ? { group: curGroup } : {}) });
    };
    const arrowRe = /\s*(-->|-\.->|==>|---|--[^-\n>]+-->|-\.[^.\n>]+-\.->|==[^=\n>]+==>)\s*(?:\|([^|]*)\|\s*)?/;
    const parseList = (p) => p.split(/\s*&\s*/).map(parseTok).filter(Boolean);
    let ei = 0, gi = 1;
    for (const raw of lines) {
      const l = raw.trim().replace(/;$/, '');
      if (!l || l.startsWith('%%')) continue;
      const sm = /^subgraph\s+(.+)$/i.exec(l);
      if (sm) {
        const rest = sm[1].trim();
        const bm = /^([a-zA-Z_][\w-]*)\s*\[([\s\S]+)\]$/.exec(rest);
        const id = bm ? bm[1] : sanitizeId(unq(rest), `Group_${gi++}`);
        const label = bm ? unq(bm[2]) : unq(rest);
        if (!gmap.has(id)) gmap.set(id, { id, label });
        gstack.push(id);
        continue;
      }
      if (/^end$/i.test(l)) { gstack.pop(); continue; }
      if (/^(?:flowchart|graph|classDef|class|style|linkStyle|click|direction)\b/i.test(l)) continue;
      let rest = l, prev = null, matched = false;
      while (true) {
        const em = arrowRe.exec(rest);
        if (!em) break;
        const leftStr = rest.slice(0, em.index).trim();
        const arr = em[1];
        let label = (em[2] || '').trim();
        let style = 'solid';
        if (arr.includes('-.')) { style = 'dashed'; if (!label) label = arr.replace(/^-\.\s*/, '').replace(/\s*-\.->$/, '').trim(); }
        else if (arr.includes('==')) { style = 'thick'; if (!label) label = arr.replace(/^==\s*/, '').replace(/\s*==>$/, '').trim(); }
        else if (arr === '---') style = 'open';
        else if (!label && arr.length > 3) label = arr.replace(/^--\s*/, '').replace(/\s*-->$/, '').trim();
        const leftList = prev || parseList(leftStr);
        rest = rest.slice(em.index + em[0].length).trim();
        const nextM = arrowRe.exec(rest);
        const rightStr = nextM ? rest.slice(0, nextM.index).trim() : rest;
        const rightList = parseList(rightStr);
        if (!leftList.length || !rightList.length) break;
        matched = true;
        leftList.forEach(upsert);
        rightList.forEach(upsert);
        for (const a of leftList) for (const b of rightList) edges.push({ id: `e${++ei}`, from: a.id, to: b.id, label: unq(label), style });
        prev = rightList;
        if (!nextM) break;
        rest = rest.slice(nextM.index);
      }
      if (matched) continue;
      const single = parseTok(l);
      if (single) upsert(single);
    }
    const groups = [...gmap.values()];
    return autoLayout({ direction, nodes: [...nmap.values()], edges, ...(groups.length ? { groups } : {}) });
  }

  const elk = typeof window.ELK === 'function' ? new window.ELK() : null;

  function resolveOverlaps(nodes, edges, direction) {
    if (nodes.length < 2) return false;
    const padX = 36, padY = 32;
    let anyMoved = false;
    for (let iter = 0; iter < 50; iter++) {
      let moved = false;
      for (let i = 0; i < nodes.length; i++) {
        for (let j = i + 1; j < nodes.length; j++) {
          const a = nodes[i], b = nodes[j];
          const wa = nodeW(a), wb = nodeW(b);
          const cax = a.x + wa / 2, cay = a.y + NODE_H / 2;
          const cbx = b.x + wb / 2, cby = b.y + NODE_H / 2;
          const ox = (wa + wb) / 2 + padX - Math.abs(cax - cbx);
          const oy = NODE_H + padY - Math.abs(cay - cby);
          if (ox > 0 && oy > 0) {
            moved = true;
            anyMoved = true;
            const preferY = direction === 'LR' ? oy <= ox * 1.4 : oy < ox * 0.7;
            if (preferY) {
              const shift = Math.ceil(oy / 2);
              if (cay <= cby) { a.y -= shift; b.y += shift; }
              else { b.y -= shift; a.y += shift; }
            } else {
              const shift = Math.ceil(ox / 2);
              if (cax <= cbx) { a.x -= shift; b.x += shift; }
              else { b.x -= shift; a.x += shift; }
            }
          }
        }
      }
      if (!moved) break;
    }
    if (anyMoved) {
      const minX = Math.min(...nodes.map((n) => n.x));
      const minY = Math.min(...nodes.map((n) => n.y));
      const dx = minX < 48 ? 48 - minX : 0;
      const dy = minY < 48 ? 48 - minY : 0;
      nodes.forEach((n) => { n.x = Math.round(n.x + dx); n.y = Math.round(n.y + dy); });
      if (edges) edges.forEach((e) => { e.points = null; });
    }
    return anyMoved;
  }

  async function elkLayout(g) {
    if (!elk || !g.nodes.length) return autoLayout(g);
    const nodes = g.nodes.map((n) => ({ ...n, w: nodeW(n) }));
    const groups = (g.groups || []).map((grp) => ({ ...grp }));
    const nodeIds = new Set(nodes.map((n) => n.id));
    const groupIds = new Set(groups.map((grp) => grp.id));
    const edges = g.edges.map((e) => ({ ...e, points: null }));
    const elkEdges = [];
    const edgeIdxByElkId = new Map();
    edges.forEach((e, idx) => {
      if (!nodeIds.has(e.from) || !nodeIds.has(e.to) || e.from === e.to) return;
      const eid = `__e_${idx}`;
      edgeIdxByElkId.set(eid, idx);
      elkEdges.push({
        id: eid,
        sources: [e.from],
        targets: [e.to],
        ...(e.label ? { labels: [{ text: e.label, width: Math.min(200, Math.max(36, e.label.length * 6.8 + 16)), height: 22 }] } : {}),
      });
    });

    const hasGroups = groups.some((grp) => nodes.some((n) => n.group === grp.id));
    const elkChildren = [];
    if (hasGroups) {
      const usedInGroup = new Set();
      for (const grp of groups) {
        const members = nodes.filter((n) => n.group === grp.id);
        if (!members.length) continue;
        members.forEach((n) => usedInGroup.add(n.id));
        elkChildren.push({
          id: `__grp_${grp.id}`,
          layoutOptions: {
            'elk.padding': '[top=38,left=20,bottom=20,right=20]',
            'elk.spacing.nodeNode': '36',
            'elk.layered.spacing.nodeNodeBetweenLayers': '64',
          },
          children: members.map((n) => ({ id: n.id, width: nodeW(n), height: NODE_H })),
        });
      }
      nodes.filter((n) => !usedInGroup.has(n.id)).forEach((n) => elkChildren.push({ id: n.id, width: nodeW(n), height: NODE_H }));
    } else {
      nodes.forEach((n) => elkChildren.push({ id: n.id, width: nodeW(n), height: NODE_H }));
    }

    try {
      const res = await elk.layout({
        id: 'root',
        layoutOptions: {
          'elk.algorithm': 'layered',
          'elk.direction': g.direction === 'TD' ? 'DOWN' : 'RIGHT',
          'elk.edgeRouting': 'ORTHOGONAL',
          'elk.hierarchyHandling': 'INCLUDE_CHILDREN',
          'elk.separateConnectedComponents': 'true',
          'elk.spacing.componentComponent': '72',
          'elk.spacing.nodeNode': '64',
          'elk.layered.spacing.nodeNodeBetweenLayers': '96',
          'elk.layered.spacing.edgeNodeBetweenLayers': '40',
          'elk.layered.spacing.edgeEdgeBetweenLayers': '26',
          'elk.layered.nodePlacement.strategy': 'NETWORK_SIMPLEX',
          'elk.layered.crossingMinimization.strategy': 'LAYER_SWEEP',
          'elk.layered.considerModelOrder.strategy': 'NODES_AND_EDGES',
          'elk.padding': '[top=48,left=48,bottom=48,right=48]',
        },
        children: elkChildren,
        edges: elkEdges,
      });
      const posMap = new Map();
      const collect = (list, ox, oy) => {
        for (const c of list || []) {
          const ax = ox + (Number.isFinite(c.x) ? c.x : 0);
          const ay = oy + (Number.isFinite(c.y) ? c.y : 0);
          posMap.set(c.id, { x: ax, y: ay, w: c.width, h: c.height });
          if (c.children?.length) collect(c.children, ax, ay);
        }
      };
      collect(res.children, 0, 0);
      nodes.forEach((n) => {
        const c = posMap.get(n.id);
        if (c) {
          n.x = Math.round(c.x);
          n.y = Math.round(c.y);
        }
      });
      const collectEdges = (container, ox, oy) => {
        for (const re of container.edges || []) {
          const idx = edgeIdxByElkId.get(re.id);
          const sec = re?.sections?.[0];
          if (idx != null && sec?.startPoint && sec?.endPoint) {
            edges[idx].points = [sec.startPoint, ...(sec.bendPoints || []), sec.endPoint].map((p) => ({
              x: Math.round(p.x + ox),
              y: Math.round(p.y + oy),
            }));
          }
        }
        for (const c of container.children || []) {
          if (c.edges?.length || c.children?.length) {
            collectEdges(c, ox + (Number.isFinite(c.x) ? c.x : 0), oy + (Number.isFinite(c.y) ? c.y : 0));
          }
        }
      };
      collectEdges(res, 0, 0);
      resolveOverlaps(nodes, edges, g.direction);
      return { direction: g.direction, nodes, edges, ...(groupIds.size ? { groups } : {}) };
    } catch {
      return autoLayout(g);
    }
  }

  function roundedOrthoPath(pts) {
    if (!pts || pts.length < 2) return '';
    if (pts.length === 2) return `M ${pts[0].x} ${pts[0].y} L ${pts[1].x} ${pts[1].y}`;
    let d = `M ${pts[0].x} ${pts[0].y}`;
    for (let i = 1; i < pts.length - 1; i++) {
      const p0 = pts[i - 1], p1 = pts[i], p2 = pts[i + 1];
      const d1 = Math.hypot(p1.x - p0.x, p1.y - p0.y);
      const d2 = Math.hypot(p2.x - p1.x, p2.y - p1.y);
      const r = Math.min(8, d1 / 2, d2 / 2);
      if (r < 1) { d += ` L ${p1.x} ${p1.y}`; continue; }
      const x1 = p1.x - ((p1.x - p0.x) / d1) * r;
      const y1 = p1.y - ((p1.y - p0.y) / d1) * r;
      const x2 = p1.x + ((p2.x - p1.x) / d2) * r;
      const y2 = p1.y + ((p2.y - p1.y) / d2) * r;
      d += ` L ${x1.toFixed(1)} ${y1.toFixed(1)} Q ${p1.x} ${p1.y} ${x2.toFixed(1)} ${y2.toFixed(1)}`;
    }
    const last = pts[pts.length - 1];
    return `${d} L ${last.x} ${last.y}`;
  }

  function edgeLabelPoint(pts, p1, p2) {
    if (pts && pts.length >= 2) {
      let bestLen = -1, mx = (pts[0].x + pts[1].x) / 2, my = (pts[0].y + pts[1].y) / 2;
      for (let i = 0; i < pts.length - 1; i++) {
        const len = Math.hypot(pts[i + 1].x - pts[i].x, pts[i + 1].y - pts[i].y);
        if (len > bestLen) {
          bestLen = len;
          mx = (pts[i].x + pts[i + 1].x) / 2;
          my = (pts[i].y + pts[i + 1].y) / 2;
        }
      }
      return { x: mx, y: my };
    }
    return { x: (p1.x + p2.x) / 2, y: (p1.y + p2.y) / 2 };
  }

  function autoLayout(g) {
    const nodes = g.nodes.map((n) => ({ ...n, w: nodeW(n) }));
    const edges = g.edges.map((e) => ({ ...e }));
    if (!nodes.length) return { direction: g.direction, nodes, edges };

    const ids = new Set(nodes.map((n) => n.id));
    const rawAdj = new Map(nodes.map((n) => [n.id, []]));
    const rawIndeg = new Map(nodes.map((n) => [n.id, 0]));
    for (const e of edges) {
      if (!ids.has(e.from) || !ids.has(e.to) || e.from === e.to) continue;
      rawAdj.get(e.from).push(e.to);
      rawIndeg.set(e.to, rawIndeg.get(e.to) + 1);
    }

    // Break cycles via DFS from sources (then lowest in-degree) so step-by-step edges flow forward.
    const visitState = new Map();
    const dagEdges = [];
    const dfs = (u) => {
      visitState.set(u, 1);
      for (const v of rawAdj.get(u)) {
        const st = visitState.get(v) ?? 0;
        if (st === 1) continue;
        dagEdges.push({ from: u, to: v });
        if (st === 0) dfs(v);
      }
      visitState.set(u, 2);
    };
    const seeds = [...nodes].sort((a, b) => rawIndeg.get(a.id) - rawIndeg.get(b.id));
    for (const n of seeds) if ((visitState.get(n.id) ?? 0) === 0) dfs(n.id);

    // Longest-path layering so every step appears strictly after all prerequisites.
    const layer = new Map(nodes.map((n) => [n.id, 0]));
    for (let i = 0; i < nodes.length; i++) {
      let changed = false;
      for (const e of dagEdges) {
        const next = layer.get(e.from) + 1;
        if (next > layer.get(e.to)) {
          layer.set(e.to, next);
          changed = true;
        }
      }
      if (!changed) break;
    }

    const groups = new Map();
    for (const n of nodes) {
      const l = layer.get(n.id) ?? 0;
      if (!groups.has(l)) groups.set(l, []);
      groups.get(l).push(n);
    }

    const preds = new Map(nodes.map((n) => [n.id, []]));
    const succs = new Map(nodes.map((n) => [n.id, []]));
    for (const e of dagEdges) {
      preds.get(e.to).push(e.from);
      succs.get(e.from).push(e.to);
    }

    // 4-pass bidirectional barycenter ordering to minimize edge crossings.
    const layers = [...groups.keys()].sort((a, b) => a - b);
    const posInLayer = new Map();
    for (const l of layers) groups.get(l).forEach((n, i) => posInLayer.set(n.id, i));
    const sortLayer = (l, neighbors) => {
      const grp = groups.get(l);
      grp.sort((a, b) => {
        const avg = (id) => {
          const ns = neighbors.get(id).filter((p) => posInLayer.has(p));
          return ns.length ? ns.reduce((s, p) => s + posInLayer.get(p), 0) / ns.length : posInLayer.get(id);
        };
        return avg(a.id) - avg(b.id);
      });
      grp.forEach((n, i) => posInLayer.set(n.id, i));
    };
    for (let pass = 0; pass < 4; pass++) {
      for (let i = 1; i < layers.length; i++) sortLayer(layers[i], preds);
      for (let i = layers.length - 2; i >= 0; i--) sortLayer(layers[i], succs);
    }

    const horiz = g.direction === 'LR';
    const spans = layers.map((l) => {
      const grp = groups.get(l);
      return horiz
        ? grp.length * NODE_H + Math.max(0, grp.length - 1) * GAP_Y
        : grp.reduce((s, n) => s + nodeW(n), 0) + Math.max(0, grp.length - 1) * GAP_X;
    });
    const maxSpan = Math.max(0, ...spans);
    const depths = layers.map((l) => (horiz ? Math.max(...groups.get(l).map(nodeW)) : NODE_H));
    let primary = 60;
    layers.forEach((l, li) => {
      let cross = 60 + Math.round((maxSpan - spans[li]) / 2);
      groups.get(l).forEach((n) => {
        const w = nodeW(n);
        if (horiz) {
          n.x = primary + Math.round((depths[li] - w) / 2);
          n.y = cross;
          cross += NODE_H + GAP_Y;
        } else {
          n.x = cross;
          n.y = primary;
          cross += w + GAP_X;
        }
      });
      primary += depths[li] + (horiz ? GAP_X : GAP_Y);
    });
    resolveOverlaps(nodes, edges, g.direction);
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
      maxX = Math.max(maxX, n.x + nodeW(n)); maxY = Math.max(maxY, n.y + NODE_H);
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

  function nodeCenter(n) { return { x: n.x + nodeW(n) / 2, y: n.y + NODE_H / 2 }; }
  function edgeAnchors(a, b) {
    const ca = nodeCenter(a), cb = nodeCenter(b);
    const wa = nodeW(a), wb = nodeW(b);
    if (graph.direction === 'LR') {
      if (cb.x >= ca.x - 12) return [{ x: a.x + wa, y: ca.y }, { x: b.x, y: cb.y }, 'h', false];
      return [{ x: ca.x, y: a.y }, { x: cb.x, y: b.y }, 'h-back', true];
    }
    if (cb.y >= ca.y - 12) return [{ x: ca.x, y: a.y + NODE_H }, { x: cb.x, y: b.y }, 'v', false];
    return [{ x: a.x + wa, y: ca.y }, { x: b.x + wb, y: cb.y }, 'v-back', true];
  }
  function curvePath(p1, p2, axis) {
    if (axis === 'h') {
      const c = Math.max(44, Math.abs(p2.x - p1.x) * 0.45);
      return `M ${p1.x} ${p1.y} C ${p1.x + c} ${p1.y}, ${p2.x - c} ${p2.y}, ${p2.x} ${p2.y}`;
    }
    if (axis === 'v') {
      const c = Math.max(38, Math.abs(p2.y - p1.y) * 0.45);
      return `M ${p1.x} ${p1.y} C ${p1.x} ${p1.y + c}, ${p2.x} ${p2.y - c}, ${p2.x} ${p2.y}`;
    }
    if (axis === 'h-back') {
      const lift = Math.max(48, Math.min(110, Math.abs(p1.x - p2.x) * 0.25));
      const top = Math.min(p1.y, p2.y) - lift;
      return `M ${p1.x} ${p1.y} C ${p1.x} ${top}, ${p2.x} ${top}, ${p2.x} ${p2.y}`;
    }
    const push = Math.max(54, Math.min(120, Math.abs(p1.y - p2.y) * 0.25));
    const right = Math.max(p1.x, p2.x) + push;
    return `M ${p1.x} ${p1.y} C ${right} ${p1.y}, ${right} ${p2.y}, ${p2.x} ${p2.y}`;
  }

  function shapeElement(n) {
    const w = nodeW(n), h = NODE_H;
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
    groupsLayer.replaceChildren();
    edgesLayer.replaceChildren();
    nodesLayer.replaceChildren();
    const byId = new Map(graph.nodes.map((n) => [n.id, n]));

    for (const grp of graph.groups || []) {
      const members = graph.nodes.filter((n) => n.group === grp.id);
      if (!members.length) continue;
      const padX = 18, padTop = 34, padBot = 18;
      const minX = Math.min(...members.map((n) => n.x)) - padX;
      const minY = Math.min(...members.map((n) => n.y)) - padTop;
      const maxX = Math.max(...members.map((n) => n.x + nodeW(n))) + padX;
      const maxY = Math.max(...members.map((n) => n.y + NODE_H)) + padBot;
      const g = svgEl('g', { class: 'flow-group', 'data-id': grp.id });
      g.append(svgEl('rect', { class: 'group-box', x: minX, y: minY, width: Math.max(120, maxX - minX), height: Math.max(80, maxY - minY) }));
      const title = svgEl('text', { class: 'group-title', x: minX + 12, y: minY + 20 });
      title.textContent = grp.label || grp.id;
      g.append(title);
      groupsLayer.append(g);
    }

    for (const e of graph.edges) {
      const a = byId.get(e.from), b = byId.get(e.to);
      if (!a || !b) continue;
      const isSel = sel?.kind === 'edge' && sel.id === e.id;
      const g = svgEl('g', { class: `flow-edge ${e.style}${isSel ? ' selected' : ''}`, 'data-id': e.id });
      const [p1, p2, axis] = edgeAnchors(a, b);
      const d = e.points?.length >= 2 ? roundedOrthoPath(e.points) : curvePath(p1, p2, axis);
      const hit = svgEl('path', { class: 'edge-hit', d });
      const path = svgEl('path', { class: 'edge-path', d });
      if (e.style !== 'open') path.setAttribute('marker-end', isSel ? 'url(#arrow-sel)' : 'url(#arrow)');
      g.append(hit, path);

      const { x: mx, y: my } = edgeLabelPoint(e.points, p1, p2);
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
      const w = nodeW(n);
      const g = svgEl('g', { class: `flow-node${isSel ? ' selected' : ''}`, transform: `translate(${n.x} ${n.y})`, 'data-id': n.id });
      g.append(shapeElement(n));
      const text = String(n.label || n.id);
      const maxChars = Math.max(16, Math.floor((w - 28) / 7.2));
      const label = svgEl('text', { class: 'node-text', x: w / 2, y: NODE_H / 2 });
      if (text.length <= maxChars) {
        label.textContent = text;
      } else {
        const cut = text.lastIndexOf(' ', maxChars);
        const split = cut > 8 ? cut : maxChars;
        const l1 = text.slice(0, split).trim();
        let l2 = text.slice(split).trim();
        if (l2.length > maxChars) l2 = `${l2.slice(0, maxChars - 1)}…`;
        const t1 = svgEl('tspan', { x: w / 2, dy: '-0.42em' });
        t1.textContent = l1;
        const t2 = svgEl('tspan', { x: w / 2, dy: '1.2em' });
        t2.textContent = l2;
        label.append(t1, t2);
      }
      g.append(label);

      const ports = [
        { cx: w, cy: NODE_H / 2 },
        { cx: 0, cy: NODE_H / 2 },
        { cx: w / 2, cy: NODE_H },
        { cx: w / 2, cy: 0 },
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
    const insp = $('inspector'), title = $('inspTitle'), empty = $('inspEmpty'), nForm = $('inspNode'), eForm = $('inspEdge');
    insp.hidden = !sel;
    empty.hidden = !!sel;
    nForm.hidden = sel?.kind !== 'node';
    eForm.hidden = sel?.kind !== 'edge';
    if (sel?.kind === 'node') {
      const n = graph.nodes.find((x) => x.id === sel.id);
      if (!n) { insp.hidden = true; return; }
      title.textContent = 'Edit Card';
      $('nodeLabel').value = n.label;
      $('nodeId').value = n.id;
      $('nodeShape').value = n.shape;
      $('nodeColors').querySelectorAll('.swatch').forEach((s) => s.classList.toggle('active', (s.dataset.color || '') === (n.color || '')));
    } else if (sel?.kind === 'edge') {
      const e = graph.edges.find((x) => x.id === sel.id);
      if (!e) { insp.hidden = true; return; }
      title.textContent = 'Edit Connection';
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
      for (const ed of graph.edges) if (ed.from === n.id || ed.to === n.id) ed.points = null;
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

  // Palette drag & drop + click + external file drop
  document.querySelectorAll('.pal-item').forEach((item) => {
    item.addEventListener('click', () => addNode(item.dataset.shape));
    item.addEventListener('dragstart', (e) => {
      e.dataTransfer.setData('application/x-taskx-shape', item.dataset.shape);
      e.dataTransfer.setData('text/plain', item.dataset.shape);
    });
  });
  function extractDroppedFiles(dt) {
    if (!dt) return [];
    const out = [];
    const seen = new Set();
    const push = (v) => {
      const s = String(v || '').trim();
      if (s && !s.startsWith('#') && !seen.has(s)) { seen.add(s); out.push(s); }
    };
    const uriList = dt.getData('text/uri-list') || dt.getData('application/vnd.code.uri-list') || '';
    for (const line of uriList.split(/\r?\n/)) push(line);
    const resUrls = dt.getData('resourceurls');
    if (resUrls) {
      try {
        const arr = JSON.parse(resUrls);
        if (Array.isArray(arr)) arr.forEach(push);
      } catch {
        // Ignore malformed resourceurls.
      }
    }
    if (!out.length && dt.files?.length) {
      for (const f of dt.files) push(f.path || f.name);
    }
    if (!out.length) {
      const text = (dt.getData('text/plain') || '').trim();
      if (text && !SHAPE_SYNTAX[text] && (/^(?:file:\/\/|\/|[a-zA-Z]:[\\/])/.test(text) || /[\\/][^\\/\n]+\.[a-zA-Z0-9]+$/.test(text))) {
        for (const line of text.split(/\r?\n/)) push(line);
      }
    }
    return out;
  }
  const canvasWrap = $('canvasWrap');
  canvasWrap.addEventListener('dragover', (e) => {
    e.preventDefault();
    canvasWrap.classList.add('drop-active');
  });
  canvasWrap.addEventListener('dragleave', (e) => {
    if (e.target === canvasWrap) canvasWrap.classList.remove('drop-active');
  });
  canvasWrap.addEventListener('drop', (e) => {
    e.preventDefault();
    canvasWrap.classList.remove('drop-active');
    const w = screenToWorld(e.clientX, e.clientY);
    const shape = e.dataTransfer.getData('application/x-taskx-shape');
    if (shape) {
      addNode(shape, w.x, w.y);
      return;
    }
    const files = extractDroppedFiles(e.dataTransfer);
    if (files.length) {
      vscode.postMessage({ type: 'dropFiles', items: files, wx: w.x, wy: w.y });
      return;
    }
    const fallbackShape = e.dataTransfer.getData('text/plain');
    if (fallbackShape && SHAPE_SYNTAX[fallbackShape]) addNode(fallbackShape, w.x, w.y);
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
  $('autoBtn').addEventListener('click', async () => {
    if (graph.nodes.length > 0) {
      pushUndo();
      graph = await elkLayout(graph);
      render();
      fitToView();
      setStatus('Refined current flow with ELK');
      return;
    }
    vscode.postMessage({ type: 'autoFlow', level: 'module', direction: graph.direction });
  });
  $('aiBtn').addEventListener('click', () => { $('aiPrompt').value = ''; $('aiDialog').showModal(); $('aiPrompt').focus(); });
  $('aiCancel').addEventListener('click', () => $('aiDialog').close());
  $('aiForm').addEventListener('submit', (e) => {
    e.preventDefault();
    const prompt = $('aiPrompt').value.trim();
    if (prompt) vscode.postMessage({ type: 'askAgent', prompt });
    $('aiDialog').close();
  });
  $('closePaletteBtn').addEventListener('click', () => {
    $('palette').hidden = true;
    $('fabAdd').hidden = false;
  });
  $('fabAdd').addEventListener('click', () => {
    $('palette').hidden = false;
    $('fabAdd').hidden = true;
  });
  $('closeInspBtn').addEventListener('click', () => { sel = null; render(); });
  $('inspDelBtn').addEventListener('click', deleteSelected);
  $('addNodeBtn').addEventListener('click', () => {
    $('palette').hidden = false;
    $('fabAdd').hidden = true;
    addNode('rect');
  });
  $('layoutBtn').addEventListener('click', async () => {
    pushUndo();
    graph = await elkLayout(graph);
    render();
    fitToView();
    setStatus('Auto-arranged with ELK');
  });
  $('dirBtn').addEventListener('click', async () => {
    pushUndo();
    graph.direction = graph.direction === 'LR' ? 'TD' : 'LR';
    graph = await elkLayout(graph);
    render();
    fitToView();
  });
  const defaultTheme = () => (document.body.classList.contains('vscode-light') || document.body.classList.contains('vscode-high-contrast-light') ? 'light' : 'dark');
  const applyTheme = (name) => {
    const t = name === 'light' ? 'light' : 'dark';
    document.body.dataset.theme = t;
    document.querySelectorAll('.theme-opt').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.theme === t)));
    vscode.setState({ ...(vscode.getState() || {}), theme: t });
  };
  document.querySelectorAll('.theme-opt').forEach((b) => {
    b.addEventListener('click', () => {
      applyTheme(b.dataset.theme);
      setStatus(`Theme: ${b.dataset.theme === 'light' ? 'Light' : 'Dark'}`);
    });
  });
  const savedTheme = vscode.getState()?.theme;
  applyTheme(savedTheme === 'light' || savedTheme === 'dark' ? savedTheme : defaultTheme());
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
  $('applyCodeBtn').addEventListener('click', async () => {
    pushUndo();
    graph = await elkLayout(parseMermaidLocal(mermaidText.value));
    sel = null;
    render();
    fitToView();
    setStatus('Synced Mermaid code to canvas');
  });
  $('copyBtn').addEventListener('click', () => vscode.postMessage({ type: 'copyText', text: toMermaid(graph) }));
  $('chatBtn').addEventListener('click', () => vscode.postMessage({ type: 'sendToChat', code: toMermaid(graph) }));
  $('openBtn').addEventListener('click', () => vscode.postMessage({ type: 'openMermaid' }));
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

  window.addEventListener('message', async (ev) => {
    const m = ev.data;
    if (m.type === 'load' && m.graph) {
      graph = await elkLayout(m.graph);
      undoStack = [];
      redoStack = [];
      sel = null;
      render();
      requestAnimationFrame(fitToView);
    } else if (m.type === 'addFileSteps' && Array.isArray(m.files) && m.files.length) {
      pushUndo();
      let lastId = null;
      m.files.forEach((f, idx) => {
        const base = sanitizeId(f.name || f.path, `File_${seq}`);
        let id = base;
        let k = 2;
        while (graph.nodes.some((n) => n.id === id)) id = `${base}_${k++}`;
        const x = snap((m.wx ?? 120) - NODE_W / 2);
        const y = snap((m.wy ?? 120) - NODE_H / 2 + idx * (NODE_H + 24));
        graph.nodes.push({ id, label: f.path, shape: 'round', x, y });
        lastId = id;
      });
      if (lastId) sel = { kind: 'node', id: lastId };
      render();
      setStatus(m.files.length === 1 ? `Added file step: ${m.files[0].path}` : `Added ${m.files.length} file steps`);
    } else if (m.type === 'status') {
      setStatus(m.text);
    }
  });

  vscode.postMessage({ type: 'ready' });
})();
