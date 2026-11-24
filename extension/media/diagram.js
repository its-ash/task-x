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
  let pan = { x: 40, y: 40, scale: 1 };
  let panDrag = null;
  let undoStack = [];
  let redoStack = [];
  let renderSeq = 0;
  let nodeSeq = 1;
  let useElk = false;

  const setStatus = (text) => { statusBar.textContent = text || ''; };
  const sanitizeId = (raw, fallback) => String(raw || '').replace(/[^a-zA-Z0-9_]/g, '_').replace(/^(\d)/, '_$1') || fallback;
  const escapeLabel = (text) => String(text || '').replace(/"/g, '#quot;').replace(/\r?\n/g, ' ').trim();

  function sanitizeMermaid(src) {
    return src.replace(/(\b[\w-]+)\[(?![(["/\\])([^\]\n"]*[()][^\]\n"]*)\]/g, (_, id, label) => `${id}["${label.replace(/"/g, '#quot;')}"]`);
  }

  function pushUndo() {
    undoStack.push(code);
    if (undoStack.length > 60) undoStack.shift();
    redoStack = [];
    syncButtons();
  }

  function syncButtons() {
    $('undoBtn').disabled = undoStack.length === 0;
    $('redoBtn').disabled = redoStack.length === 0;
    $('delBtn').disabled = !selNodeId;
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
      flowchart: { useMaxWidth: false, htmlLabels: true, curve: 'basis', nodeSpacing: 48, rankSpacing: 64 },
    });
    return true;
  }

  function getDirection(src) {
    const m = /^\s*(?:flowchart|graph)\s+(TD|TB|BT|LR|RL)\b/im.exec(src);
    if (!m) return 'TD';
    const d = m[1].toUpperCase();
    return d === 'LR' || d === 'RL' ? 'LR' : 'TD';
  }

  function parseNodesFromCode(src) {
    const map = new Map();
    const unq = (t) => {
      const s = t.trim();
      return (s.startsWith('"') && s.endsWith('"')) || (s.startsWith("'") && s.endsWith("'"))
        ? s.slice(1, -1).replace(/#quot;|&quot;/g, '"')
        : s.replace(/#quot;|&quot;/g, '"');
    };
    const pats = [
      [/\b([a-zA-Z_][\w-]*)\s*\(\[([^\]]+)\]\)/g, 'stadium'],
      [/\b([a-zA-Z_][\w-]*)\s*\[\(([^)]+)\)\]/g, 'cylinder'],
      [/\b([a-zA-Z_][\w-]*)\s*\[\[([^\]]+)\]\]/g, 'subroutine'],
      [/\b([a-zA-Z_][\w-]*)\s*\[\/([^/\]]+)\/\]/g, 'trapezoid'],
      [/\b([a-zA-Z_][\w-]*)\s*\{([^}]+)\}/g, 'diamond'],
      [/\b([a-zA-Z_][\w-]*)\s*\(([^()]+)\)/g, 'round'],
      [/\b([a-zA-Z_][\w-]*)\s*\[([^\]]+)\]/g, 'rect'],
    ];
    for (const raw of src.replace(/\r\n?/g, '\n').split('\n')) {
      const line = raw.trim();
      if (!line || line.startsWith('%%') || /^(?:flowchart|graph|subgraph|end|style|classDef|class|linkStyle|click|direction)\b/i.test(line)) continue;
      for (const [re, shape] of pats) {
        re.lastIndex = 0;
        let m;
        while ((m = re.exec(line))) {
          const id = m[1];
          if (!map.has(id)) map.set(id, { id, label: unq(m[2]), shape, color: '' });
        }
      }
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
    const raw = el?.getAttribute('data-id') || el?.id || '';
    const m = /^flowchart-([^-]+(?:-[^-]+)*)-\d+$/.exec(raw);
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

  function highlightSelected() {
    host.querySelectorAll('svg .node.mmd-selected').forEach((n) => n.classList.remove('mmd-selected'));
    if (!selNodeId) return;
    host.querySelectorAll('svg .node').forEach((n) => {
      if (extractNodeIdFromDom(n) === selNodeId) n.classList.add('mmd-selected');
    });
  }

  function syncInspector() {
    const insp = $('inspector');
    const nForm = $('inspNode');
    const nodes = parseNodesFromCode(code);
    const node = selNodeId ? nodes.get(selNodeId) : null;
    if (!node) {
      insp.hidden = true;
      nForm.hidden = true;
      syncButtons();
      return;
    }
    insp.hidden = false;
    nForm.hidden = false;
    $('inspTitle').textContent = 'Edit Card';
    $('nodeLabel').value = node.label;
    $('nodeId').value = node.id;
    $('nodeShape').value = node.shape;
    $('nodeColors').querySelectorAll('.swatch').forEach((s) => s.classList.toggle('active', (s.dataset.color || '') === (node.color || '')));
    const targetSel = $('connectTarget');
    targetSel.replaceChildren();
    for (const [id, info] of nodes) {
      if (id === node.id) continue;
      const opt = document.createElement('option');
      opt.value = id;
      opt.textContent = info.label && info.label !== id ? `${info.label} (${id})` : id;
      targetSel.append(opt);
    }
    $('connectBtn').disabled = targetSel.options.length === 0;
    syncButtons();
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
        host.querySelectorAll('svg .node').forEach((nodeEl) => {
          nodeEl.addEventListener('click', (ev) => {
            ev.stopPropagation();
            const nid = extractNodeIdFromDom(nodeEl);
            if (!nid) return;
            selNodeId = nid;
            highlightSelected();
            syncInspector();
          });
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
    const defRe = new RegExp(`\\b${esc}\\s*(?:\\(\\[[^\\]]+\\]\\)|\\[\\([^)]+\\)\\]|\\[\\[[^\\]]+\\]\\]|\\[\\/[^/\\]]+\\/\\]|\\{[^}]+\\}|\\([^()]+\\)|\\[[^\\]]+\\])`);
    let updated = code;
    if (defRe.test(updated)) {
      updated = updated.replace(defRe, replacement);
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

  function addCard(shape = 'rect', customLabel, customId) {
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
    selNodeId = id;
    void renderDiagram(true);
    setStatus(`Added card: ${label}`);
  }

  function deleteSelected() {
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
  }

  // Pan & zoom on stage
  stage.addEventListener('pointerdown', (e) => {
    if (e.target.closest('.node')) return;
    if (selNodeId) {
      selNodeId = null;
      highlightSelected();
      syncInspector();
    }
    stage.classList.add('panning');
    panDrag = { sx: e.clientX, sy: e.clientY, px: pan.x, py: pan.y };
  });
  window.addEventListener('pointermove', (e) => {
    if (!panDrag) return;
    pan.x = panDrag.px + (e.clientX - panDrag.sx);
    pan.y = panDrag.py + (e.clientY - panDrag.sy);
    applyTransform();
  });
  window.addEventListener('pointerup', () => {
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
  $('nodeLabel').addEventListener('change', (e) => {
    if (!selNodeId) return;
    pushUndo();
    updateNodeInCode(selNodeId, e.target.value, $('nodeShape').value);
  });
  $('nodeId').addEventListener('change', (e) => {
    if (!selNodeId) return;
    const next = sanitizeId(e.target.value, selNodeId);
    if (next === selNodeId) return;
    pushUndo();
    updateNodeInCode(selNodeId, $('nodeLabel').value, $('nodeShape').value, next);
  });
  $('nodeShape').addEventListener('change', (e) => {
    if (!selNodeId) return;
    pushUndo();
    updateNodeInCode(selNodeId, $('nodeLabel').value, e.target.value);
  });
  $('nodeColors').addEventListener('click', (e) => {
    const b = e.target.closest('.swatch');
    if (!b || !selNodeId) return;
    pushUndo();
    updateNodeInCode(selNodeId, $('nodeLabel').value, $('nodeShape').value, selNodeId, b.dataset.color || '');
  });
  $('connectBtn').addEventListener('click', () => {
    const target = $('connectTarget').value;
    if (!selNodeId || !target) return;
    pushUndo();
    const lbl = escapeLabel($('connectLabel').value);
    code = `${code.trimEnd()}\n    ${selNodeId} ${lbl ? `-->|"${lbl}"|` : '-->'} ${target}`;
    $('connectLabel').value = '';
    void renderDiagram(false);
    setStatus(`Connected ${selNodeId} -> ${target}`);
  });

  // Floating panel toggles & toolbar actions
  $('closePaletteBtn').addEventListener('click', () => { $('palette').hidden = true; $('fabAdd').hidden = false; });
  $('fabAdd').addEventListener('click', () => { $('palette').hidden = false; $('fabAdd').hidden = true; });
  $('closeInspBtn').addEventListener('click', () => { selNodeId = null; highlightSelected(); syncInspector(); });
  $('inspDelBtn').addEventListener('click', deleteSelected);
  $('addNodeBtn').addEventListener('click', () => { $('palette').hidden = false; $('fabAdd').hidden = true; addCard('rect'); });

  $('autoBtn').addEventListener('click', () => {
    if (!code.trim()) {
      vscode.postMessage({ type: 'autoFlow', level: 'module', direction: 'LR' });
      return;
    }
    useElk = !useElk;
    void renderDiagram(true);
    setStatus(`Mermaid layout: ${useElk ? 'ELK' : 'Dagre'}`);
  });
  $('layoutBtn').addEventListener('click', () => {
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
  $('saveBtn').addEventListener('click', () => vscode.postMessage({ type: 'saveMermaid', code }));
  $('svgBtn').addEventListener('click', () => {
    if (renderedSvg) vscode.postMessage({ type: 'exportSvg', svg: renderedSvg });
  });

  window.addEventListener('keydown', (e) => {
    if (e.target.matches('input, textarea, select')) return;
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'z') {
      e.preventDefault();
      if (e.shiftKey) $('redoBtn').click();
      else $('undoBtn').click();
    } else if (e.key === 'Delete' || e.key === 'Backspace') {
      if (selNodeId) { e.preventDefault(); deleteSelected(); }
    }
  });

  window.addEventListener('message', (ev) => {
    const m = ev.data;
    if (m.type === 'load') {
      code = typeof m.code === 'string' ? m.code : '';
      undoStack = [];
      redoStack = [];
      selNodeId = null;
      void renderDiagram(true);
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
