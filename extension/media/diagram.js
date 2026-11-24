(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);

  const stage = $('stage');
  const host = $('mermaidHost');
  const statusBar = $('statusBar');
  const mermaidText = $('mermaidText');
  const canvasWrap = $('canvasWrap');

  const SHAPE_SYNTAX = {
    rect: ['[', ']'],
    round: ['(', ')'],
    stadium: ['([', '])'],
    diamond: ['{', '}'],
    cylinder: ['[(', ')]'],
    subroutine: ['[[', ']]'],
    trapezoid: ['[/', '/]'],
  };

  let code = '';
  let renderedSvg = '';
  let selNodeId = null;
  let selEdge = null;
  let dirty = false;
  let pan = { x: 40, y: 40, scale: 1 };
  let panDrag = null;
  let nodeDrag = null;
  let suppressNextClick = false;
  let nodeOffsets = new Map();
  let labelEditTimer = null;
  let edgeEditTimer = null;
  let undoStack = [];
  let redoStack = [];
  let renderSeq = 0;
  let nodeSeq = 1;
  let useElk = false;

  const setStatus = (text) => { statusBar.textContent = text || ''; };
  const sanitizeId = (raw, fallback) => String(raw || '').replace(/[^a-zA-Z0-9_]/g, '_').replace(/^(\d)/, '_$1') || fallback;
  const escapeLabel = (text) => String(text || '').replace(/"/g, '#quot;').replace(/\r?\n/g, '<br/>').trim();
  const decodeLabel = (text) => String(text || '').replace(/<br\s*\/?>/gi, '\n').replace(/#quot;|&quot;/g, '"');
  const markDirty = (val = true) => { dirty = val; $('saveBtn')?.classList.toggle('dirty', dirty); };

  function sanitizeMermaid(src) {
    return src.replace(/(\b[\w-]+)\[(?![(["/\\])([^\]\n"]*[()][^\]\n"]*)\]/g, (_, id, label) => `${id}["${label.replace(/"/g, '#quot;')}"]`);
  }

  function pushUndo() {
    undoStack.push(code);
    if (undoStack.length > 60) undoStack.shift();
    redoStack = [];
    markDirty(true);
    syncButtons();
  }

  function syncButtons() {
    $('undoBtn').disabled = undoStack.length === 0;
    $('redoBtn').disabled = redoStack.length === 0;
    $('delBtn').disabled = !selNodeId && !selEdge;
  }

  function currentTheme() {
    return document.body.dataset.theme === 'light' ? 'light' : 'dark';
  }

  function initMermaid() {
    const m = window.mermaid;
    if (!m) return false;
    const dark = currentTheme() === 'dark';
    const cs = getComputedStyle(document.body);
    const canvas = document.createElement('canvas');
    canvas.width = canvas.height = 1;
    const ctx2d = canvas.getContext('2d', { willReadFrequently: true });
    const color = (v, fallback) => {
      const raw = cs.getPropertyValue(v).trim();
      const val = raw && typeof CSS !== 'undefined' && CSS.supports('color', raw) ? raw : fallback;
      if (!ctx2d) return fallback;
      ctx2d.clearRect(0, 0, 1, 1);
      ctx2d.fillStyle = dark ? '#1e1e1e' : '#f8f8f8';
      ctx2d.fillRect(0, 0, 1, 1);
      ctx2d.fillStyle = val;
      ctx2d.fillRect(0, 0, 1, 1);
      const [r, g, b] = ctx2d.getImageData(0, 0, 1, 1).data;
      return `#${((1 << 24) | (r << 16) | (g << 8) | b).toString(16).slice(1)}`;
    };
    const bg = color('--bg', dark ? '#18191d' : '#f7f7f9');
    const surface = color('--surface', dark ? '#22242a' : '#ffffff');
    const surfaceHigh = color('--surface-high', dark ? '#2c2f37' : '#f0f1f4');
    const fg = color('--on-surface', dark ? '#eceef2' : '#1f2229');
    const muted = color('--on-surface-variant', dark ? '#aeb4c2' : '#5e6472');
    const border = color('--border-strong', dark ? '#4a4f5e' : '#c4c8d2');
    const accent = color('--accent', dark ? '#7ab2ff' : '#2563eb');
    const font = cs.getPropertyValue('--vscode-font-family').trim() || 'system-ui, -apple-system, sans-serif';
    m.initialize({
      startOnLoad: false,
      securityLevel: 'strict',
      suppressErrorRendering: true,
      theme: 'base',
      layout: useElk ? 'elk' : 'dagre',
      fontFamily: font,
      themeVariables: {
        darkMode: dark,
        background: bg,
        primaryColor: surface,
        primaryTextColor: fg,
        primaryBorderColor: border,
        secondaryColor: surfaceHigh,
        secondaryTextColor: fg,
        secondaryBorderColor: border,
        tertiaryColor: surfaceHigh,
        tertiaryTextColor: fg,
        tertiaryBorderColor: border,
        lineColor: muted,
        textColor: fg,
        mainBkg: surface,
        nodeBorder: border,
        clusterBkg: surfaceHigh,
        clusterBorder: border,
        titleColor: fg,
        edgeLabelBackground: bg,
        actorBkg: surface,
        actorBorder: accent,
        actorTextColor: fg,
        actorLineColor: muted,
        signalColor: fg,
        signalTextColor: fg,
        labelBoxBkgColor: surface,
        labelBoxBorderColor: border,
        labelTextColor: fg,
        loopTextColor: fg,
        noteBkgColor: surfaceHigh,
        noteTextColor: fg,
        noteBorderColor: accent,
        activationBkgColor: surfaceHigh,
        activationBorderColor: accent,
        sequenceNumberColor: bg,
      },
      sequence: { useMaxWidth: false, showSequenceNumbers: false, wrap: true },
      flowchart: { useMaxWidth: false, htmlLabels: true, wrappingWidth: 220, padding: 18, curve: 'basis', nodeSpacing: 52, rankSpacing: 68 },
    });
    return true;
  }

  function getDirection(src) {
    const m = /^\s*(?:flowchart|graph)\s+(TD|TB|BT|LR|RL)\b/im.exec(src);
    if (!m) return 'TD';
    const d = m[1].toUpperCase();
    return d === 'LR' || d === 'RL' ? 'LR' : 'TD';
  }

  const EDGE_RE = /^(\s*)([a-zA-Z_][\w-]*)([^\n]*?)\s+(-->|-\.->|==>|--o|--x|<-->|---)\s*(?:\|([^|]*)\|\s*)?([a-zA-Z_][\w-]*)(.*)$/;

  function unquote(t) {
    const s = String(t || '').trim();
    const inner = (s.startsWith('"') && s.endsWith('"')) || (s.startsWith("'") && s.endsWith("'")) ? s.slice(1, -1) : s;
    return decodeLabel(inner);
  }

  function parseEdgesFromCode(src) {
    const edges = [];
    const lines = src.replace(/\r\n?/g, '\n').split('\n');
    for (let i = 0; i < lines.length; i++) {
      const trimmed = lines[i].trim();
      if (!trimmed || trimmed.startsWith('%%') || /^(?:flowchart|graph|subgraph|end|style|classDef|class|linkStyle|click|direction)\b/i.test(trimmed)) continue;
      const m = EDGE_RE.exec(lines[i]);
      if (!m) continue;
      edges.push({
        index: edges.length,
        lineIndex: i,
        indent: m[1],
        from: m[2],
        fromSuffix: m[3],
        arrow: m[4],
        label: m[5] ? unquote(m[5]) : '',
        to: m[6],
        toSuffix: m[7],
      });
    }
    return edges;
  }

  function parseNodesFromCode(src) {
    const map = new Map();
    const pats = [
      [/\b([a-zA-Z_][\w-]*)\s*\(\[((?:"[^"]*"|[^\]"])+)\]\)/g, 'stadium'],
      [/\b([a-zA-Z_][\w-]*)\s*\[\(((?:"[^"]*"|[^)"])+)\)\]/g, 'cylinder'],
      [/\b([a-zA-Z_][\w-]*)\s*\[\[((?:"[^"]*"|[^\]"])+)\]\]/g, 'subroutine'],
      [/\b([a-zA-Z_][\w-]*)\s*\[\/((?:"[^"]*"|[^/\]"])+)\/\]/g, 'trapezoid'],
      [/\b([a-zA-Z_][\w-]*)\s*\{((?:"[^"]*"|[^}"])+)\}/g, 'diamond'],
      [/\b([a-zA-Z_][\w-]*)\s*\(((?:"[^"]*"|[^()"])+)\)/g, 'round'],
      [/\b([a-zA-Z_][\w-]*)\s*\[((?:"[^"]*"|[^\]"])+)\]/g, 'rect'],
    ];
    for (const raw of src.replace(/\r\n?/g, '\n').split('\n')) {
      const line = raw.trim();
      if (!line || line.startsWith('%%') || /^(?:flowchart|graph|subgraph|end|style|classDef|class|linkStyle|click|direction)\b/i.test(line)) continue;
      for (const [re, shape] of pats) {
        re.lastIndex = 0;
        let m;
        while ((m = re.exec(line))) {
          const id = m[1];
          if (!map.has(id)) map.set(id, { id, label: unquote(m[2]), shape, color: '' });
        }
      }
    }
    for (const e of parseEdgesFromCode(src)) {
      if (!map.has(e.from)) map.set(e.from, { id: e.from, label: e.from, shape: 'rect', color: '' });
      if (!map.has(e.to)) map.set(e.to, { id: e.to, label: e.to, shape: 'rect', color: '' });
    }
    const styleRe = /^\s*style\s+([a-zA-Z_][\w-]*)\s+.*stroke:\s*(#[0-9a-fA-F]{3,8})/gm;
    let sm;
    while ((sm = styleRe.exec(src))) {
      const n = map.get(sm[1]);
      if (n) n.color = sm[2];
    }
    return map;
  }

  function extractNodeIdFromDom(el) {
    const dataId = el?.getAttribute('data-id');
    if (dataId && !/^flowchart-/.test(dataId)) return dataId;
    const raw = dataId || el?.id || '';
    const m = /^flowchart-(.+)-\d+$/.exec(raw);
    if (m) return m[1];
    return raw.replace(/^flowchart-/, '').replace(/-\d+$/, '');
  }

  function parseSafeSvg(svgText) {
    const doc = new DOMParser().parseFromString(svgText, 'text/html');
    const svg = doc.body.querySelector('svg');
    if (!svg) return null;
    svg.querySelectorAll('script, iframe, object, embed').forEach((n) => n.remove());
    for (const n of svg.querySelectorAll('*')) {
      for (const a of [...n.attributes]) {
        if (/^on/i.test(a.name) || (/^(?:href|xlink:href)$/i.test(a.name) && /^\s*javascript:/i.test(a.value))) n.removeAttribute(a.name);
      }
    }
    return document.importNode(svg, true);
  }

  function getSvgNaturalSize(svgEl) {
    const vb = (svgEl.getAttribute('viewBox') || '').trim().split(/[\s,]+/).map(Number);
    if (vb.length === 4 && vb[2] > 0 && vb[3] > 0) return { w: vb[2], h: vb[3] };
    const w = parseFloat(svgEl.getAttribute('width') || '');
    const h = parseFloat(svgEl.getAttribute('height') || '');
    if (w > 0 && h > 0) return { w, h };
    const bbox = svgEl.getBoundingClientRect();
    return { w: bbox.width / pan.scale || 400, h: bbox.height / pan.scale || 300 };
  }

  function applyTransform() {
    host.style.transform = `translate(${pan.x}px, ${pan.y}px) scale(${pan.scale})`;
    $('zoomVal').textContent = `${Math.round(pan.scale * 100)}%`;
  }

  function fitToView() {
    const svgEl = host.querySelector('svg');
    if (!svgEl) { pan = { x: 40, y: 40, scale: 1 }; applyTransform(); return; }
    const stageRect = stage.getBoundingClientRect();
    const { w: rawW, h: rawH } = getSvgNaturalSize(svgEl);
    const pad = 96;
    const scale = Math.max(0.08, Math.min(1.35, Math.min((stageRect.width - pad) / Math.max(1, rawW), (stageRect.height - pad) / Math.max(1, rawH))));
    pan = {
      scale,
      x: Math.round((stageRect.width - rawW * scale) / 2 - 48 * scale),
      y: Math.round((stageRect.height - rawH * scale) / 2 - 48 * scale),
    };
    applyTransform();
  }

  function parseTranslate(el) {
    const tr = el?.getAttribute('transform') || '';
    const m = /translate\(\s*([-\d.]+)(?:[\s,]+([-\d.]+))?\s*\)/.exec(tr);
    return m ? { x: parseFloat(m[1]) || 0, y: parseFloat(m[2]) || 0 } : { x: 0, y: 0 };
  }

  function fixCroppedLabels(svgEl) {
    if (!svgEl) return;
    svgEl.querySelectorAll('.node').forEach((nodeEl) => {
      const fo = nodeEl.querySelector('foreignObject');
      if (!fo) return;
      const content = fo.querySelector('.nodeLabel, span, div, p') || fo.firstElementChild;
      if (!content) return;
      fo.querySelectorAll('div, span, p').forEach((el) => {
        el.style.whiteSpace = 'normal';
        el.style.overflowWrap = 'break-word';
        el.style.wordBreak = 'break-word';
        el.style.lineHeight = '1.35';
      });
      const foW = parseFloat(fo.getAttribute('width') || '0');
      const foH = parseFloat(fo.getAttribute('height') || '0');
      const needW = Math.ceil(Math.max(foW, content.scrollWidth + 8));
      const needH = Math.ceil(Math.max(foH, content.scrollHeight + 6));
      if (needW <= foW + 1 && needH <= foH + 1) return;
      const foX = parseFloat(fo.getAttribute('x') || String(-foW / 2));
      const foY = parseFloat(fo.getAttribute('y') || String(-foH / 2));
      const cx = foX + foW / 2;
      const cy = foY + foH / 2;
      fo.setAttribute('width', String(needW));
      fo.setAttribute('height', String(needH));
      fo.setAttribute('x', String(Math.round(cx - needW / 2)));
      fo.setAttribute('y', String(Math.round(cy - needH / 2)));
      const labelG = fo.closest('g.label');
      if (labelG && labelG.getAttribute('transform')) {
        labelG.setAttribute('transform', `translate(${Math.round(-needW / 2)}, ${Math.round(-needH / 2)})`);
        fo.setAttribute('x', '0');
        fo.setAttribute('y', '0');
      }
      const shape = nodeEl.querySelector('rect.basic, rect.label-container, polygon.label-container, path.label-container');
      if (!shape) return;
      const padX = 28;
      const padY = 18;
      const tag = shape.tagName.toLowerCase();
      if (tag === 'rect') {
        const curW = parseFloat(shape.getAttribute('width') || '0');
        const curH = parseFloat(shape.getAttribute('height') || '0');
        const targetW = Math.max(curW, needW + padX);
        const targetH = Math.max(curH, needH + padY);
        shape.setAttribute('width', String(Math.ceil(targetW)));
        shape.setAttribute('height', String(Math.ceil(targetH)));
        shape.setAttribute('x', String(Math.round(-targetW / 2)));
        shape.setAttribute('y', String(Math.round(-targetH / 2)));
      } else if (tag === 'polygon' || tag === 'path') {
        try {
          const bb = shape.getBBox();
          if (bb.width > 0 && bb.height > 0) {
            const sx = Math.max(1, (needW + padX + (tag === 'polygon' ? 24 : 0)) / bb.width);
            const sy = Math.max(1, (needH + padY + (tag === 'polygon' ? 24 : 0)) / bb.height);
            if (sx > 1.02 || sy > 1.02) {
              const bx = bb.x + bb.width / 2;
              const by = bb.y + bb.height / 2;
              shape.setAttribute('transform', `translate(${bx},${by}) scale(${sx.toFixed(3)},${sy.toFixed(3)}) translate(${-bx},${-by})`);
            }
          }
        } catch {}
      }
    });
    svgEl.querySelectorAll('.edgeLabels .edgeLabel foreignObject').forEach((fo) => {
      const content = fo.querySelector('.edgeLabel, span, div, p') || fo.firstElementChild;
      if (!content) return;
      const foW = parseFloat(fo.getAttribute('width') || '0');
      const foH = parseFloat(fo.getAttribute('height') || '0');
      const needW = Math.ceil(Math.max(foW, content.scrollWidth + 8));
      const needH = Math.ceil(Math.max(foH, content.scrollHeight + 4));
      if (needW > foW + 1) fo.setAttribute('width', String(needW));
      if (needH > foH + 1) fo.setAttribute('height', String(needH));
    });
  }

  function anchorOnBox(box, target) {
    const dx = target.x - box.x;
    const dy = target.y - box.y;
    const hw = Math.max(10, box.w / 2);
    const hh = Math.max(10, box.h / 2);
    if (Math.abs(dx) < 0.01 && Math.abs(dy) < 0.01) return { x: box.x, y: box.y };
    const t = Math.min(hw / Math.max(0.01, Math.abs(dx)), hh / Math.max(0.01, Math.abs(dy)));
    return { x: box.x + dx * t, y: box.y + dy * t };
  }

  function applyNodeOffsets(svgEl, edgesList) {
    if (!svgEl) return;
    const boxes = new Map();
    svgEl.querySelectorAll('.node').forEach((nodeEl) => {
      const nid = extractNodeIdFromDom(nodeEl);
      if (!nid) return;
      if (nodeEl.dataset.baseX === undefined) {
        const base = parseTranslate(nodeEl);
        nodeEl.dataset.baseX = String(base.x);
        nodeEl.dataset.baseY = String(base.y);
      }
      const bx = parseFloat(nodeEl.dataset.baseX);
      const by = parseFloat(nodeEl.dataset.baseY);
      const off = nodeOffsets.get(nid) || { dx: 0, dy: 0 };
      const nx = bx + off.dx;
      const ny = by + off.dy;
      if (off.dx !== 0 || off.dy !== 0) nodeEl.setAttribute('transform', `translate(${nx}, ${ny})`);
      let w = 120, h = 44;
      try {
        const bb = nodeEl.getBBox();
        if (bb.width > 0) w = bb.width;
        if (bb.height > 0) h = bb.height;
      } catch {}
      boxes.set(nid, { x: nx, y: ny, w, h });
    });
    if (!nodeOffsets.size) return;
    const edges = edgesList || parseEdgesFromCode(code);
    const paths = [...svgEl.querySelectorAll('.edgePaths path.flowchart-link')];
    const labels = [...svgEl.querySelectorAll('.edgeLabels .edgeLabel')];
    const isLR = getDirection(code) === 'LR';
    edges.forEach((edge, idx) => {
      const fromOff = nodeOffsets.get(edge.from);
      const toOff = nodeOffsets.get(edge.to);
      if ((!fromOff || (fromOff.dx === 0 && fromOff.dy === 0)) && (!toOff || (toOff.dx === 0 && toOff.dy === 0))) return;
      const fromBox = boxes.get(edge.from);
      const toBox = boxes.get(edge.to);
      const path = paths[idx];
      if (!fromBox || !toBox || !path) return;
      const p1 = anchorOnBox(fromBox, toBox);
      const p2 = anchorOnBox(toBox, fromBox);
      const dx = p2.x - p1.x;
      const dy = p2.y - p1.y;
      const c1x = isLR ? p1.x + dx * 0.45 : p1.x;
      const c1y = isLR ? p1.y : p1.y + dy * 0.45;
      const c2x = isLR ? p2.x - dx * 0.45 : p2.x;
      const c2y = isLR ? p2.y : p2.y - dy * 0.45;
      path.setAttribute('d', `M${p1.x.toFixed(1)},${p1.y.toFixed(1)}C${c1x.toFixed(1)},${c1y.toFixed(1)},${c2x.toFixed(1)},${c2y.toFixed(1)},${p2.x.toFixed(1)},${p2.y.toFixed(1)}`);
      const lbl = labels[idx];
      if (lbl) {
        const mx = (p1.x + p2.x) / 2;
        const my = (p1.y + p2.y) / 2;
        lbl.setAttribute('transform', `translate(${mx.toFixed(1)}, ${my.toFixed(1)})`);
      }
    });
  }

  function highlightSelected() {
    host.querySelectorAll('svg .node.mmd-selected').forEach((n) => n.classList.remove('mmd-selected'));
    host.querySelectorAll('svg .edge-selected').forEach((n) => n.classList.remove('edge-selected'));
    if (selNodeId) {
      host.querySelectorAll('svg .node').forEach((n) => {
        if (extractNodeIdFromDom(n) === selNodeId) n.classList.add('mmd-selected');
      });
    }
    if (selEdge) {
      const paths = [...host.querySelectorAll('svg .edgePaths path.flowchart-link')];
      const labels = [...host.querySelectorAll('svg .edgeLabels .edgeLabel')];
      if (paths[selEdge.index]) paths[selEdge.index].classList.add('edge-selected');
      if (labels[selEdge.index]) labels[selEdge.index].classList.add('edge-selected');
    }
  }

  function syncInspector() {
    const insp = $('inspector');
    const nForm = $('inspNode');
    const eForm = $('inspEdge');
    const nodes = parseNodesFromCode(code);
    const edges = parseEdgesFromCode(code);
    let node = selNodeId ? nodes.get(selNodeId) : null;
    if (selNodeId && !node) {
      const domEl = [...host.querySelectorAll('svg .node, svg g[data-id]')].find((n) => extractNodeIdFromDom(n) === selNodeId);
      node = { id: selNodeId, label: (domEl?.textContent || selNodeId).trim(), shape: 'rect', color: '' };
    }
    if (selEdge) {
      const liveEdge = edges.find((e) => e.lineIndex === selEdge.lineIndex) || edges[selEdge.index] || null;
      selEdge = liveEdge;
    }
    if (!node && !selEdge) {
      insp.hidden = true;
      nForm.hidden = true;
      eForm.hidden = true;
      syncButtons();
      return;
    }
    insp.hidden = false;
    if (selEdge) {
      nForm.hidden = true;
      eForm.hidden = false;
      $('inspTitle').textContent = 'Edit Connection';
      $('edgeRoute').textContent = `${selEdge.from} ${selEdge.arrow} ${selEdge.to}`;
      $('edgeLabel').dataset.edgeLine = String(selEdge.lineIndex);
      if (document.activeElement !== $('edgeLabel')) $('edgeLabel').value = selEdge.label;
      $('edgeStyle').value = ['-->', '-.->', '==>'].includes(selEdge.arrow) ? selEdge.arrow : '-->';
      syncButtons();
      return;
    }
    eForm.hidden = true;
    nForm.hidden = false;
    $('inspTitle').textContent = 'Edit Card';
    $('nodeLabel').dataset.editingId = node.id;
    $('nodeId').dataset.editingId = node.id;
    if (document.activeElement !== $('nodeLabel')) $('nodeLabel').value = node.label;
    if (document.activeElement !== $('nodeId')) $('nodeId').value = node.id;
    $('nodeShape').value = node.shape;
    $('nodeColors').querySelectorAll('.swatch').forEach((s) => s.classList.toggle('active', (s.dataset.color || '') === (node.color || '')));
    const targetSel = $('connectTarget');
    targetSel.replaceChildren();
    for (const [id, info] of nodes) {
      if (id === node.id) continue;
      const opt = document.createElement('option');
      opt.value = id;
      const single = info.label.replace(/\n/g, ' ');
      opt.textContent = single && single !== id ? `${single} (${id})` : id;
      targetSel.append(opt);
    }
    $('connectBtn').disabled = targetSel.options.length === 0;
    const related = edges.filter((e) => e.from === node.id || e.to === node.id);
    const edgesWrap = $('nodeEdgesWrap');
    const edgesBox = $('nodeEdges');
    edgesBox.replaceChildren();
    edgesWrap.hidden = related.length === 0;
    for (const edge of related) {
      const row = document.createElement('div');
      row.className = 'edge-item';
      const btn = document.createElement('button');
      btn.type = 'button';
      btn.className = 'edge-pick';
      const dir = edge.from === node.id ? `→ ${edge.to}` : `← ${edge.from}`;
      btn.textContent = edge.label ? `${dir} (${edge.label})` : dir;
      btn.title = 'Edit connection';
      btn.addEventListener('click', () => {
        selNodeId = null;
        selEdge = edge;
        highlightSelected();
        syncInspector();
      });
      const del = document.createElement('button');
      del.type = 'button';
      del.className = 'btn icon small-icon danger';
      del.title = 'Remove connection';
      del.innerHTML = '<span class="codicon codicon-close" aria-hidden="true"></span>';
      del.addEventListener('click', () => deleteEdgeInCode(edge));
      row.append(btn, del);
      edgesBox.append(row);
    }
    syncButtons();
  }

  function openInlineEdit(nodeEl, nid) {
    canvasWrap.querySelector('.inline-edit')?.remove();
    const nodes = parseNodesFromCode(code);
    const node = nodes.get(nid) || { id: nid, label: (nodeEl?.textContent || nid).trim(), shape: 'rect', color: '' };
    const rect = nodeEl.getBoundingClientRect();
    const wrapRect = canvasWrap.getBoundingClientRect();
    const input = document.createElement('textarea');
    input.className = 'inline-edit';
    input.rows = Math.max(1, Math.min(4, node.label.split('\n').length));
    input.value = node.label;
    const w = Math.max(200, Math.min(340, rect.width + 32));
    input.style.left = `${Math.max(12, Math.min(wrapRect.width - w - 12, rect.left - wrapRect.left + rect.width / 2 - w / 2))}px`;
    input.style.top = `${Math.max(12, rect.top - wrapRect.top + rect.height / 2 - 22)}px`;
    input.style.width = `${w}px`;
    let committed = false;
    const commit = (save) => {
      if (committed) return;
      committed = true;
      const val = input.value.trim();
      input.remove();
      if (save && val && val !== node.label) {
        pushUndo();
        updateNodeInCode(nid, val, node.shape);
        setStatus(`Updated ${nid}`);
      }
    };
    input.addEventListener('pointerdown', (e) => e.stopPropagation());
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); commit(true); }
      else if (e.key === 'Escape') { e.preventDefault(); commit(false); }
    });
    input.addEventListener('blur', () => commit(true));
    canvasWrap.append(input);
    input.focus();
    input.select();
  }

  async function renderDiagram(shouldFit = false) {
    $('dirBtn').textContent = getDirection(code);
    if (document.activeElement !== mermaidText) mermaidText.value = code;
    const trimmed = code.trim();
    const emptyState = $('emptyState');
    if (emptyState) emptyState.hidden = !!trimmed;
    if (!trimmed) {
      host.replaceChildren();
      renderedSvg = '';
      syncInspector();
      return;
    }
    if (!initMermaid()) return;
    const id = `flow-mmd-${++renderSeq}`;
    try {
      let res;
      try {
        res = await window.mermaid.render(id, trimmed);
      } catch (err) {
        document.getElementById(`d${id}`)?.remove();
        const fixed = sanitizeMermaid(trimmed);
        if (fixed === trimmed) throw err;
        res = await window.mermaid.render(`${id}-s`, fixed);
      }
      renderedSvg = res.svg;
      const svgEl = parseSafeSvg(res.svg);
      if (svgEl) {
        svgEl.removeAttribute('style');
        const vb = (svgEl.getAttribute('viewBox') || '').trim().split(/[\s,]+/).map(Number);
        if (vb.length === 4 && vb[2] > 0 && vb[3] > 0) {
          svgEl.setAttribute('width', String(Math.ceil(vb[2])));
          svgEl.setAttribute('height', String(Math.ceil(vb[3])));
        }
        host.replaceChildren(svgEl);
        try { fixCroppedLabels(svgEl); } catch {}
        const edges = parseEdgesFromCode(code);
        try { applyNodeOffsets(svgEl, edges); } catch {}
        const selectNode = (nid, shiftKey) => {
          if (!nid) return;
          if (shiftKey && selNodeId && selNodeId !== nid) {
            pushUndo();
            code = `${code.trimEnd()}\n    ${selNodeId} --> ${nid}`;
            setStatus(`Connected ${selNodeId} -> ${nid}`);
            selNodeId = nid;
            selEdge = null;
            void renderDiagram(false);
            return;
          }
          selNodeId = nid;
          selEdge = null;
          highlightSelected();
          syncInspector();
        };
        host.querySelectorAll('svg .node, svg g[data-id][id^="flowchart-"]').forEach((nodeEl) => {
          nodeEl.addEventListener('pointerdown', (ev) => {
            if (ev.button !== 0 || ev.shiftKey) return;
            ev.stopPropagation();
            const nid = extractNodeIdFromDom(nodeEl);
            if (!nid) return;
            const orig = nodeOffsets.get(nid) || { dx: 0, dy: 0 };
            nodeDrag = { nid, nodeEl, svgEl, edges, startX: ev.clientX, startY: ev.clientY, origDx: orig.dx, origDy: orig.dy, moved: false, selectNode };
          });
          nodeEl.addEventListener('click', (ev) => {
            ev.stopPropagation();
            if (suppressNextClick) { suppressNextClick = false; return; }
            selectNode(extractNodeIdFromDom(nodeEl), ev.shiftKey);
          });
          nodeEl.addEventListener('dblclick', (ev) => {
            ev.stopPropagation();
            const nid = extractNodeIdFromDom(nodeEl);
            if (!nid) return;
            selectNode(nid, false);
            openInlineEdit(nodeEl, nid);
          });
        });
        const selectEdgeByIdx = (idx, ev) => {
          ev.stopPropagation();
          const edge = edges[idx];
          if (!edge) return;
          selNodeId = null;
          selEdge = edge;
          highlightSelected();
          syncInspector();
        };
        host.querySelectorAll('svg .edgePaths path.flowchart-link').forEach((p, idx) => {
          p.addEventListener('click', (ev) => selectEdgeByIdx(idx, ev));
        });
        host.querySelectorAll('svg .edgeLabels .edgeLabel').forEach((lbl, idx) => {
          lbl.addEventListener('click', (ev) => selectEdgeByIdx(idx, ev));
        });
        highlightSelected();
        if (shouldFit) requestAnimationFrame(fitToView);
      }
    } catch (e) {
      document.getElementById(`d${id}`)?.remove();
      document.getElementById(`d${id}-s`)?.remove();
      const box = document.createElement('div');
      box.className = 'mermaid-err';
      box.textContent = e instanceof Error ? e.message : String(e);
      host.replaceChildren(box);
    }
    syncInspector();
  }

  function updateNodeInCode(targetId, nextLabel, nextShape, nextId = targetId, nextColor = undefined) {
    const [o, c] = SHAPE_SYNTAX[nextShape] || SHAPE_SYNTAX.rect;
    const replacement = `${nextId}${o}"${escapeLabel(nextLabel || nextId)}"${c}`;
    const esc = targetId.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const defRe = new RegExp(`\\b${esc}\\s*(?:\\(\\[(?:"[^"]*"|[^\\]"])+\\]\\)|\\[\\((?:"[^"]*"|[^)"])+\\)\\]|\\[\\[(?:"[^"]*"|[^\\]"])+\\]\\]|\\[\\/(?:"[^"]*"|[^/\\]"])+\\/\\]|\\{(?:"[^"]*"|[^}"])+\\}|\\((?:"[^"]*"|[^()"])+\\)|\\[(?:"[^"]*"|[^\\]"])+\\])`);
    let updated = code;
    if (defRe.test(updated)) {
      updated = updated.replace(defRe, replacement);
    } else {
      const lines = updated.replace(/\r\n?/g, '\n').split('\n');
      const bareRe = new RegExp(`\\b${esc}\\b`);
      for (let i = 0; i < lines.length; i++) {
        const t = lines[i].trim();
        if (!t || t.startsWith('%%') || /^(?:flowchart|graph|subgraph|end|style|classDef|class|linkStyle|click|direction)\b/i.test(t)) continue;
        if (bareRe.test(lines[i])) {
          lines[i] = lines[i].replace(bareRe, replacement);
          break;
        }
      }
      updated = lines.join('\n');
    }
    if (nextId !== targetId) {
      updated = updated.replace(new RegExp(`\\b${esc}\\b`, 'g'), nextId);
      selNodeId = nextId;
    }
    if (nextColor !== undefined) {
      const styleRe = new RegExp(`^\\s*style\\s+${nextId.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')}\\b[^\\n]*\\n?`, 'gm');
      updated = updated.replace(styleRe, '').trimEnd();
      if (nextColor) updated += `\n    style ${nextId} stroke:${nextColor},stroke-width:2px`;
    }
    code = updated;
    void renderDiagram(false);
  }

  function updateEdgeInCode(edge, nextLabel, nextArrow, reverse = false) {
    if (!edge) return;
    const lines = code.replace(/\r\n?/g, '\n').split('\n');
    const line = lines[edge.lineIndex];
    if (line === undefined) return;
    const m = EDGE_RE.exec(line);
    if (!m) return;
    pushUndo();
    const arrow = nextArrow || m[4];
    const lbl = escapeLabel(nextLabel);
    const labelPart = lbl ? `|"${lbl}"|` : '';
    const left = reverse ? `${m[6]}${m[7]}` : `${m[2]}${m[3]}`;
    const right = reverse ? `${m[2]}${m[3]}` : `${m[6]}${m[7]}`;
    lines[edge.lineIndex] = `${m[1]}${left} ${arrow}${labelPart} ${right}`;
    code = lines.join('\n');
    void renderDiagram(false);
    setStatus(reverse ? 'Reversed connection' : 'Updated connection');
  }

  function deleteEdgeInCode(edge) {
    if (!edge) return;
    const lines = code.replace(/\r\n?/g, '\n').split('\n');
    if (edge.lineIndex < 0 || edge.lineIndex >= lines.length) return;
    pushUndo();
    lines.splice(edge.lineIndex, 1);
    code = lines.join('\n').replace(/\n{3,}/g, '\n\n');
    if (selEdge && selEdge.lineIndex === edge.lineIndex) selEdge = null;
    void renderDiagram(false);
    setStatus('Removed connection');
  }

  function addCard(shape = 'rect', customLabel, customId, color = '') {
    pushUndo();
    const existing = parseNodesFromCode(code);
    const base = sanitizeId(customId || `Step_${nodeSeq++}`, 'Step');
    let id = base;
    let k = 2;
    while (existing.has(id)) id = `${base}_${k++}`;
    const label = customLabel || `Step ${existing.size + 1}`;
    const [o, c] = SHAPE_SYNTAX[shape] || SHAPE_SYNTAX.rect;
    if (!code.trim()) code = 'flowchart TD';
    const def = `${id}${o}"${escapeLabel(label)}"${c}`;
    code = `${code.trimEnd()}\n    ${selNodeId ? `${selNodeId} --> ${def}` : def}`;
    if (color) code += `\n    style ${id} stroke:${color},stroke-width:2px`;
    selNodeId = id;
    selEdge = null;
    void renderDiagram(true);
    setStatus(`Added card: ${label}`);
  }

  function deleteSelected() {
    if (selEdge) {
      deleteEdgeInCode(selEdge);
      return;
    }
    if (!selNodeId) return;
    pushUndo();
    const esc = selNodeId.replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
    const wordRe = new RegExp(`\\b${esc}\\b`);
    code = code
      .split('\n')
      .filter((line) => !wordRe.test(line))
      .join('\n')
      .replace(/\n{3,}/g, '\n\n');
    selNodeId = null;
    void renderDiagram(false);
    setStatus('Deleted card');
  }

  // Pan & zoom on stage
  stage.addEventListener('pointerdown', (e) => {
    if (e.target.closest('.node, .edgeLabel, path.flowchart-link')) return;
    if (selNodeId || selEdge) {
      selNodeId = null;
      selEdge = null;
      highlightSelected();
      syncInspector();
    }
    stage.classList.add('panning');
    panDrag = { sx: e.clientX, sy: e.clientY, px: pan.x, py: pan.y };
  });
  stage.addEventListener('dblclick', (e) => {
    if (e.target.closest('.node, .edgeLabel, path.flowchart-link')) return;
    addCard('rect');
  });
  window.addEventListener('pointermove', (e) => {
    if (nodeDrag) {
      const dist = Math.hypot(e.clientX - nodeDrag.startX, e.clientY - nodeDrag.startY);
      if (!nodeDrag.moved && dist > 6) {
        nodeDrag.moved = true;
        nodeDrag.nodeEl.classList.add('dragging');
      }
      if (nodeDrag.moved) {
        const dx = nodeDrag.origDx + (e.clientX - nodeDrag.startX) / pan.scale;
        const dy = nodeDrag.origDy + (e.clientY - nodeDrag.startY) / pan.scale;
        nodeOffsets.set(nodeDrag.nid, { dx, dy });
        applyNodeOffsets(nodeDrag.svgEl, nodeDrag.edges);
      }
      return;
    }
    if (!panDrag) return;
    pan.x = panDrag.px + (e.clientX - panDrag.sx);
    pan.y = panDrag.py + (e.clientY - panDrag.sy);
    applyTransform();
  });
  window.addEventListener('pointerup', () => {
    if (nodeDrag) {
      const d = nodeDrag;
      nodeDrag = null;
      d.nodeEl.classList.remove('dragging');
      if (d.moved) {
        suppressNextClick = true;
        setTimeout(() => { suppressNextClick = false; }, 80);
        setStatus(`Moved ${d.nid}`);
      } else {
        d.selectNode?.(d.nid, false);
      }
      return;
    }
    if (!panDrag) return;
    panDrag = null;
    stage.classList.remove('panning');
  });
  stage.addEventListener('wheel', (e) => {
    e.preventDefault();
    const r = stage.getBoundingClientRect();
    const mx = e.clientX - r.left, my = e.clientY - r.top;
    const factor = e.deltaY < 0 ? 1.12 : 0.9;
    const next = Math.max(0.08, Math.min(3, pan.scale * factor));
    pan.x = mx - ((mx - pan.x) * next) / pan.scale;
    pan.y = my - ((my - pan.y) * next) / pan.scale;
    pan.scale = next;
    applyTransform();
  }, { passive: false });

  // Palette drag & drop + click + external file drop
  document.querySelectorAll('.pal-item').forEach((item) => {
    item.addEventListener('click', () => addCard(item.dataset.shape));
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
      try { const arr = JSON.parse(resUrls); if (Array.isArray(arr)) arr.forEach(push); } catch {}
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

  canvasWrap.addEventListener('dragover', (e) => { e.preventDefault(); canvasWrap.classList.add('drop-active'); });
  canvasWrap.addEventListener('dragleave', (e) => { if (e.target === canvasWrap) canvasWrap.classList.remove('drop-active'); });
  canvasWrap.addEventListener('drop', (e) => {
    e.preventDefault();
    canvasWrap.classList.remove('drop-active');
    const shape = e.dataTransfer.getData('application/x-taskx-shape');
    if (shape) { addCard(shape); return; }
    const files = extractDroppedFiles(e.dataTransfer);
    if (files.length) {
      vscode.postMessage({ type: 'dropFiles', items: files, wx: 0, wy: 0 });
      return;
    }
    const fallbackShape = e.dataTransfer.getData('text/plain');
    if (fallbackShape && SHAPE_SYNTAX[fallbackShape]) addCard(fallbackShape);
  });

  // Floating Edit Card bindings
  const commitNodeLabel = (el, pushHistory) => {
    const tid = el.dataset.editingId || selNodeId;
    if (!tid) return;
    const node = parseNodesFromCode(code).get(tid);
    if (!node || el.value === node.label) return;
    if (pushHistory) pushUndo();
    updateNodeInCode(tid, el.value, $('nodeShape').value);
  };
  $('nodeLabel').addEventListener('input', (e) => {
    clearTimeout(labelEditTimer);
    labelEditTimer = setTimeout(() => commitNodeLabel(e.target, true), 320);
  });
  $('nodeLabel').addEventListener('change', (e) => {
    clearTimeout(labelEditTimer);
    commitNodeLabel(e.target, true);
  });
  $('nodeId').addEventListener('change', (e) => {
    const tid = e.target.dataset.editingId || selNodeId;
    if (!tid) return;
    const next = sanitizeId(e.target.value, tid);
    if (next === tid) return;
    pushUndo();
    if (nodeOffsets.has(tid)) {
      nodeOffsets.set(next, nodeOffsets.get(tid));
      nodeOffsets.delete(tid);
    }
    updateNodeInCode(tid, $('nodeLabel').value, $('nodeShape').value, next);
  });
  $('nodeShape').addEventListener('change', (e) => {
    const tid = $('nodeId').dataset.editingId || selNodeId;
    if (!tid) return;
    pushUndo();
    updateNodeInCode(tid, $('nodeLabel').value, e.target.value);
  });
  $('nodeColors').addEventListener('click', (e) => {
    const b = e.target.closest('.swatch');
    const tid = $('nodeId').dataset.editingId || selNodeId;
    if (!b || !tid) return;
    pushUndo();
    updateNodeInCode(tid, $('nodeLabel').value, $('nodeShape').value, tid, b.dataset.color || '');
  });
  $('connectBtn').addEventListener('click', () => {
    const target = $('connectTarget').value;
    if (!selNodeId || !target) return;
    pushUndo();
    const arrow = $('connectArrow')?.value || '-->';
    const lbl = escapeLabel($('connectLabel').value);
    code = `${code.trimEnd()}\n    ${selNodeId} ${lbl ? `${arrow}|"${lbl}"|` : arrow} ${target}`;
    $('connectLabel').value = '';
    void renderDiagram(false);
    setStatus(`Connected ${selNodeId} -> ${target}`);
  });
  $('addNextBtn').addEventListener('click', () => {
    if (!selNodeId) return;
    addCard('rect');
  });
  $('dupNodeBtn').addEventListener('click', () => {
    if (!selNodeId) return;
    const node = parseNodesFromCode(code).get(selNodeId);
    if (!node) return;
    const prev = selNodeId;
    selNodeId = null;
    addCard(node.shape, node.label, `${prev}_copy`, node.color);
  });
  const commitEdgeLabel = (el) => {
    const lineIdx = Number(el.dataset.edgeLine);
    const target = selEdge || parseEdgesFromCode(code).find((ed) => ed.lineIndex === lineIdx);
    if (!target || el.value === target.label) return;
    updateEdgeInCode(target, el.value, $('edgeStyle').value);
  };
  $('edgeLabel').addEventListener('input', (e) => {
    clearTimeout(edgeEditTimer);
    edgeEditTimer = setTimeout(() => commitEdgeLabel(e.target), 320);
  });
  $('edgeLabel').addEventListener('change', (e) => {
    clearTimeout(edgeEditTimer);
    commitEdgeLabel(e.target);
  });
  $('edgeStyle').addEventListener('change', (e) => {
    if (!selEdge) return;
    updateEdgeInCode(selEdge, $('edgeLabel').value, e.target.value);
  });
  $('edgeReverseBtn').addEventListener('click', () => {
    if (!selEdge) return;
    updateEdgeInCode(selEdge, $('edgeLabel').value, $('edgeStyle').value, true);
  });

  // Floating panel toggles & toolbar actions
  $('closePaletteBtn').addEventListener('click', () => { $('palette').hidden = true; $('fabAdd').hidden = false; });
  $('fabAdd').addEventListener('click', () => { $('palette').hidden = false; $('fabAdd').hidden = true; });
  $('closeInspBtn').addEventListener('click', () => { selNodeId = null; selEdge = null; highlightSelected(); syncInspector(); });
  $('inspDelBtn').addEventListener('click', deleteSelected);
  $('addNodeBtn').addEventListener('click', () => { $('palette').hidden = false; $('fabAdd').hidden = true; addCard('rect'); });

  $('autoBtn').addEventListener('click', () => {
    if (!code.trim()) {
      vscode.postMessage({ type: 'autoFlow', level: 'module', direction: 'LR' });
      return;
    }
    nodeOffsets.clear();
    useElk = !useElk;
    void renderDiagram(true);
    setStatus(`Mermaid layout: ${useElk ? 'ELK' : 'Dagre'}`);
  });
  $('layoutBtn').addEventListener('click', () => {
    nodeOffsets.clear();
    useElk = !useElk;
    void renderDiagram(true);
    setStatus(`Mermaid layout: ${useElk ? 'ELK' : 'Dagre'}`);
  });
  $('dirBtn').addEventListener('click', () => {
    if (!code.trim()) return;
    pushUndo();
    const nextDir = getDirection(code) === 'LR' ? 'TD' : 'LR';
    if (/^\s*(?:flowchart|graph)\s+(?:TD|TB|BT|LR|RL)\b/im.test(code)) {
      code = code.replace(/^(\s*(?:flowchart|graph)\s+)(?:TD|TB|BT|LR|RL)\b/im, `$1${nextDir}`);
    }
    void renderDiagram(true);
  });

  const defaultTheme = () => (document.body.classList.contains('vscode-light') || document.body.classList.contains('vscode-high-contrast-light') ? 'light' : 'dark');
  const applyTheme = (name) => {
    const t = name === 'light' ? 'light' : 'dark';
    document.body.dataset.theme = t;
    document.querySelectorAll('.theme-opt').forEach((b) => b.setAttribute('aria-pressed', String(b.dataset.theme === t)));
    vscode.setState({ ...(vscode.getState() || {}), theme: t });
    if (code.trim()) void renderDiagram(false);
  };
  document.querySelectorAll('.theme-opt').forEach((b) => {
    b.addEventListener('click', () => {
      applyTheme(b.dataset.theme);
      setStatus(`Theme: ${b.dataset.theme === 'light' ? 'Light' : 'Dark'}`);
    });
  });
  const savedTheme = vscode.getState()?.theme;
  applyTheme(savedTheme === 'light' || savedTheme === 'dark' ? savedTheme : defaultTheme());

  $('aiBtn').addEventListener('click', () => { $('aiPrompt').value = ''; $('aiDialog').showModal(); $('aiPrompt').focus(); });
  $('aiCancel').addEventListener('click', () => $('aiDialog').close());
  $('aiForm').addEventListener('submit', (e) => {
    e.preventDefault();
    const prompt = $('aiPrompt').value.trim();
    if (prompt) vscode.postMessage({ type: 'askAgent', prompt });
    $('aiDialog').close();
  });
  $('undoBtn').addEventListener('click', () => {
    if (!undoStack.length) return;
    redoStack.push(code);
    code = undoStack.pop();
    selNodeId = null;
    void renderDiagram(false);
  });
  $('redoBtn').addEventListener('click', () => {
    if (!redoStack.length) return;
    undoStack.push(code);
    code = redoStack.pop();
    selNodeId = null;
    void renderDiagram(false);
  });
  $('delBtn').addEventListener('click', deleteSelected);
  $('zoomOutBtn').addEventListener('click', () => { pan.scale = Math.max(0.08, pan.scale - 0.15); applyTransform(); });
  $('zoomInBtn').addEventListener('click', () => { pan.scale = Math.min(3, pan.scale + 0.15); applyTransform(); });
  $('fitBtn').addEventListener('click', fitToView);
  $('codeToggleBtn').addEventListener('click', () => {
    const d = $('codeDrawer');
    d.hidden = !d.hidden;
    if (!d.hidden) mermaidText.value = code;
  });
  $('closeCodeBtn').addEventListener('click', () => { $('codeDrawer').hidden = true; });
  $('applyCodeBtn').addEventListener('click', () => {
    pushUndo();
    code = mermaidText.value;
    selNodeId = null;
    void renderDiagram(true);
    setStatus('Applied Mermaid code');
  });
  $('copyBtn').addEventListener('click', () => vscode.postMessage({ type: 'copyText', text: code }));
  $('chatBtn').addEventListener('click', () => vscode.postMessage({ type: 'sendToChat', code }));
  $('openBtn').addEventListener('click', () => vscode.postMessage({ type: 'openMermaid' }));
  $('emptyOpenBtn')?.addEventListener('click', () => vscode.postMessage({ type: 'openMermaid' }));
  $('refreshBtn').addEventListener('click', () => vscode.postMessage({ type: 'refreshFile' }));
  $('saveBtn').addEventListener('click', (e) => vscode.postMessage({ type: 'saveMermaid', code, saveAs: e.shiftKey }));
  $('svgBtn').addEventListener('click', () => {
    if (renderedSvg) vscode.postMessage({ type: 'exportSvg', svg: renderedSvg });
  });

  window.addEventListener('keydown', (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 's') {
      e.preventDefault();
      vscode.postMessage({ type: 'saveMermaid', code, saveAs: e.shiftKey });
      return;
    }
    if (e.target.matches('input, textarea, select')) return;
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'z') {
      e.preventDefault();
      if (e.shiftKey) $('redoBtn').click();
      else $('undoBtn').click();
    } else if (e.key === 'F2' || (e.key === 'Enter' && selNodeId)) {
      if (selNodeId) {
        const el = [...host.querySelectorAll('svg .node')].find((n) => extractNodeIdFromDom(n) === selNodeId);
        if (el) { e.preventDefault(); openInlineEdit(el, selNodeId); }
      }
    } else if (e.key === 'Delete' || e.key === 'Backspace') {
      if (selNodeId || selEdge) { e.preventDefault(); deleteSelected(); }
    } else if (e.key === 'Escape') {
      if (selNodeId || selEdge) { selNodeId = null; selEdge = null; highlightSelected(); syncInspector(); }
    }
  });

  window.addEventListener('message', (ev) => {
    const m = ev.data;
    if (m.type === 'load') {
      code = typeof m.code === 'string' ? m.code : '';
      undoStack = [];
      redoStack = [];
      selNodeId = null;
      selEdge = null;
      nodeOffsets.clear();
      markDirty(false);
      void renderDiagram(true);
    } else if (m.type === 'saved') {
      markDirty(false);
    } else if (m.type === 'addFileSteps' && Array.isArray(m.files) && m.files.length) {
      pushUndo();
      if (!code.trim()) code = 'flowchart TD';
      const existing = parseNodesFromCode(code);
      let prevId = selNodeId;
      for (const f of m.files) {
        const base = sanitizeId(f.name || f.path, `File_${nodeSeq++}`);
        let id = base;
        let k = 2;
        while (existing.has(id)) id = `${base}_${k++}`;
        existing.set(id, { id, label: f.path, shape: 'round' });
        const def = `${id}("${escapeLabel(f.path)}")`;
        code = `${code.trimEnd()}\n    ${prevId ? `${prevId} --> ${def}` : def}`;
        prevId = id;
      }
      selNodeId = prevId;
      void renderDiagram(true);
      setStatus(m.files.length === 1 ? `Added file step: ${m.files[0].path}` : `Added ${m.files.length} file steps`);
    } else if (m.type === 'rerender') {
      if (code.trim()) {
        void renderDiagram(true);
        setStatus('Refreshed diagram');
      } else {
        setStatus('Open a .mmd file to refresh');
      }
    } else if (m.type === 'status') {
      setStatus(m.text);
    }
  });

  vscode.postMessage({ type: 'ready' });
})();
