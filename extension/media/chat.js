// task-x chat webview. Text is always inserted with textContent; model output is never parsed as HTML.
(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const log = $('log'), input = $('input'), send = $('send'), progress = $('progress');
  const modes = $('modes'), modelBtn = $('modelBtn'), permBtn = $('permBtn'), autoCommitBox = $('autoCommit');

  const MODES = {
    ask: { icon: 'comment-discussion', label: 'Ask', desc: 'Answer questions about your code. Read-only.', placeholder: 'Ask about your code' },
    agent: { icon: 'robot', label: 'Agent', desc: 'Edit files and run commands to finish the task.', placeholder: 'Tell task-x what to change' },
  };
  const ROLE_ICONS = { research: 'search', implementation: 'code', testing: 'beaker', review: 'eye', documentation: 'book' };
  const APPROVALS = {
    askBeforeEdit: { icon: 'shield', label: 'Ask first', title: 'Ask before edits', desc: 'Approve every edit, delete, and command.' },
    askBeforeExecution: { icon: 'edit', label: 'Auto-edit', title: 'Edit automatically', desc: 'Apply edits directly. Ask before commands and deletes.' },
    autonomous: { icon: 'rocket', label: 'Autonomous', title: 'Run autonomously', desc: 'Apply edits and run project commands. Ask before deletes.' },
  };
  const TOOL_LABELS = {
    read_file: 'Read', read_range: 'Read', read_multiple_files: 'Read files', read_symbol: 'Read symbol', get_file_outline: 'Outlined',
    list_files: 'Listed', find_files: 'Found files', get_file_tree: 'Listed tree', get_file_metadata: 'Inspected', detect_language: 'Inspected',
    get_project_structure: 'Inspected project', search_text: 'Searched', search_regex: 'Searched', find_all: 'Searched', find_symbol: 'Found symbol',
    find_definition: 'Found definition', find_references: 'Found references', search_files_by_content: 'Searched', replace_text: 'Edited',
    replace_all: 'Edited', insert_at_line: 'Edited', delete_range: 'Edited', apply_patch: 'Patched', create_file: 'Created', rename_file: 'Renamed',
    delete_file: 'Deleted', git_status: 'Git status', git_diff: 'Git diff', git_log: 'Git log', git_show: 'Git show', get_changed_files: 'Changed files',
    get_file_history: 'File history', format_code: 'Formatted', run_linter: 'Linted', run_tests: 'Ran tests', check_build: 'Built',
    get_diagnostics: 'Checked problems', run_command: 'Ran', read_skill: 'Loaded skill',
    Read: 'Read', Write: 'Wrote', Edit: 'Edited', MultiEdit: 'Edited', NotebookEdit: 'Edited notebook', Bash: 'Ran', BashOutput: 'Read output',
    KillShell: 'Stopped shell', Grep: 'Searched', Glob: 'Found files', LS: 'Listed', TodoWrite: 'Updated todos', WebFetch: 'Fetched',
    WebSearch: 'Searched web', Task: 'Subagent', ExitPlanMode: 'Finished plan', run_subagent: 'Subagent', worktree_apply: 'Applied worktree', worktree_discard: 'Discarded worktree',
  };

  // A successful edit tool shows as its edit card, not as a row (a failed edit still shows as a row).
  const EDIT_TOOLS = new Set(['replace_text', 'replace_all', 'insert_at_line', 'delete_range', 'apply_patch', 'create_file', 'rename_file', 'delete_file', 'Write', 'Edit', 'MultiEdit', 'NotebookEdit']);
  let replaying = false; // true while the saved history is drawn: no enter animations
  const enter = (e, cls = 'enter') => { if (!replaying) e.classList.add(cls); return e; };
  let state = { busy: false, mode: 'agent', approval: 'askBeforeExecution', autoCommit: false, modelGroups: [], active: null };
  let turn = null; // { response: Element|null }
  let current = null; // streaming assistant body
  let renderQueued = false;

  // ---------- DOM helpers ----------
  const el = (tag, cls, text) => {
    const e = document.createElement(tag);
    if (cls) e.className = cls;
    if (text != null) e.textContent = text;
    return e;
  };
  const icon = (name, extra = '') => {
    const i = el('span', `codicon codicon-${name}${extra ? ' ' + extra : ''}`);
    i.setAttribute('aria-hidden', 'true');
    return i;
  };
  const button = (cls, content, onClick, title) => {
    const b = el('button', cls);
    b.type = 'button';
    for (const c of [].concat(content)) b.append(typeof c === 'string' ? document.createTextNode(c) : c);
    if (title) { b.title = title; if (!b.textContent.trim()) b.setAttribute('aria-label', title); }
    b.addEventListener('click', onClick);
    return b;
  };
  const nearBottom = () => log.scrollHeight - log.scrollTop - log.clientHeight < 80;
  // Sticks to the bottom until the user scrolls up; any later growth of the log keeps it pinned.
  let stick = true;
  const scroll = (force) => { if (force) stick = true; if (stick) log.scrollTop = log.scrollHeight; };
  log.addEventListener('scroll', () => { stick = nearBottom(); }, { passive: true });
  const pin = () => scroll();
  new MutationObserver(pin).observe(log, { childList: true, subtree: true, characterData: true });
  new ResizeObserver(pin).observe(log);
  log.addEventListener('load', pin, true);
  log.addEventListener('toggle', pin, true);

  // ---------- Menus (popover="auto": light dismiss + Escape for free) ----------
  function openMenu(anchor, build) {
    document.querySelectorAll('.menu').forEach((m) => m.remove());
    const menu = el('div', 'menu');
    menu.popover = 'auto';
    menu.setAttribute('role', 'menu');
    build(menu);
    document.body.append(menu);
    menu.addEventListener('toggle', (e) => {
      if (e.newState === 'closed') { if (menu.contains(document.activeElement) || document.activeElement === document.body) anchor.focus(); menu.remove(); anchor.setAttribute('aria-expanded', 'false'); }
    });
    menu.showPopover();
    anchor.setAttribute('aria-expanded', 'true');
    menu.place = () => place(menu, anchor);
    menu.place();
    const visible = () => [...menu.querySelectorAll('.menu-item')].filter((i) => !i.hidden);
    const search = menu.querySelector('.menu-search input');
    if (search) search.focus();
    else (visible().find((i) => i.getAttribute('aria-checked') === 'true') ?? visible()[0])?.focus();
    menu.addEventListener('keydown', (e) => {
      const items = visible();
      const i = items.indexOf(document.activeElement);
      const go = (n) => { e.preventDefault(); items[(n + items.length) % items.length]?.focus(); };
      const inSearch = e.target === search;
      if (e.key === 'ArrowDown') go(i + 1);
      else if (e.key === 'ArrowUp') i <= 0 && search ? (e.preventDefault(), search.focus()) : go(i - 1);
      else if (e.key === 'Home' && !inSearch) go(0);
      else if (e.key === 'End' && !inSearch) go(items.length - 1);
      else if (e.key === 'Tab') menu.hidePopover();
    });
    return menu;
  }
  // Opens on the side with more room; the height is capped to that room so the list scrolls inside.
  function place(menu, anchor) {
    const r = anchor.getBoundingClientRect();
    const roomAbove = r.top - 8, roomBelow = innerHeight - r.bottom - 8;
    const above = roomAbove >= roomBelow;
    menu.style.maxHeight = Math.max(120, above ? roomAbove : roomBelow) + 'px';
    const w = menu.offsetWidth, h = menu.offsetHeight;
    menu.style.left = Math.max(4, Math.min(r.left, innerWidth - w - 4)) + 'px';
    menu.style.top = Math.max(4, above ? r.top - h - 4 : Math.min(r.bottom + 4, innerHeight - h - 4)) + 'px';
  }
  function menuItem(menu, { icon: ic, title, desc, checked, onSelect }) {
    const b = el('button', 'menu-item');
    b.type = 'button';
    b.setAttribute('role', checked === undefined ? 'menuitem' : 'menuitemradio');
    if (checked !== undefined) b.setAttribute('aria-checked', String(!!checked));
    b.append(checked === undefined ? (ic ? icon(ic) : el('span')) : icon('check', 'check'));
    const t = el('span', 'title');
    if (ic && checked !== undefined) t.append(icon(ic));
    t.append(document.createTextNode(title));
    b.append(t);
    if (desc) b.append(el('span', 'desc', desc));
    b.addEventListener('click', () => { b.closest('.menu')?.hidePopover(); onSelect(); });
    menu.append(b);
  }
  const menuHeader = (menu, text) => menu.append(el('div', 'menu-header', text));
  const menuSep = (menu) => menu.append(el('div', 'menu-sep'));

  // Mode tabs: filled segmented control with roving focus (arrow keys, Home, End).
  const modeIds = Object.keys(MODES);
  const modeTabs = modeIds.map((id, i) => {
    const m = MODES[id];
    const t = button('tab', [icon(m.icon), el('span', 'text', m.label)], () => { if (state.mode !== id) vscode.postMessage({ type: 'selectMode', mode: id }); }, `${m.label}: ${m.desc}`);
    t.setAttribute('role', 'tab');
    t.setAttribute('aria-controls', 'input');
    t.dataset.mode = id;
    t.style.setProperty('--i', String(i));
    return t;
  });
  modes.style.setProperty('--n', String(modeIds.length));
  modes.append(el('span', 'tab-ind'), ...modeTabs);
  modes.addEventListener('keydown', (e) => {
    const i = modeTabs.indexOf(document.activeElement);
    if (i < 0) return;
    const n = modeTabs.length;
    const j = { ArrowRight: (i + 1) % n, ArrowLeft: (i - 1 + n) % n, Home: 0, End: n - 1 }[e.key];
    if (j == null) return;
    e.preventDefault();
    modeTabs[j].focus();
    modeTabs[j].click();
  });
  // Models picked before, listed first in the model menu.
  const MAX_RECENT = 5;
  const recentModels = () => vscode.getState()?.recent ?? [];
  function rememberModel(kind, model) {
    const recent = [{ kind, model }, ...recentModels().filter((r) => r.kind !== kind || r.model !== model)].slice(0, MAX_RECENT);
    vscode.setState({ ...(vscode.getState() || {}), recent });
  }
  const providerName = (kind) => state.modelGroups.find((g) => g.kind === kind)?.label.split(' · ')[0] ?? kind;
  // Model list with search; refilled when the model list or the query changes.
  let modelMenu = null;
  // One tab per provider (built-in or named), plus All. The tab narrows the list below the search box.
  function renderModelTabs() {
    const { tabs } = modelMenu;
    const all = [{ kind: 'all', label: 'All' }, ...state.modelGroups];
    if (!all.some((g) => g.kind === modelMenu.tab)) modelMenu.tab = 'all';
    tabs.replaceChildren(...all.map((g) => {
      const label = g.label.split(' · ')[0];
      const t = button('model-tab', label, () => { modelMenu.tab = g.kind; fillModels(); }, label);
      t.setAttribute('role', 'tab');
      t.setAttribute('aria-selected', String(g.kind === modelMenu.tab));
      return t;
    }));
  }
  function fillModels() {
    if (!modelMenu) return;
    const { list, input: q } = modelMenu;
    renderModelTabs();
    const query = q.value.trim().toLowerCase();
    const focused = document.activeElement === q;
    list.replaceChildren();
    const pick = (kind, model) => { rememberModel(kind, model); vscode.postMessage({ type: 'selectModel', kind, model }); };
    let shown = 0;
    const recent = query ? [] : recentModels().filter((r) => (modelMenu.tab === 'all' || modelMenu.tab === r.kind) && state.modelGroups.some((g) => g.kind === r.kind && g.models.includes(r.model)));
    if (recent.length) {
      menuHeader(list, 'Recent');
      for (const r of recent) menuItem(list, { title: r.model, desc: providerName(r.kind), checked: state.active?.kind === r.kind && state.active?.model === r.model, onSelect: () => pick(r.kind, r.model) });
    }
    const groups = modelMenu.tab === 'all' ? state.modelGroups : state.modelGroups.filter((g) => g.kind === modelMenu.tab);
    for (const g of groups) {
      const groupHit = query && g.label.toLowerCase().includes(query);
      const models = g.models.filter((n) => !query || groupHit || n.toLowerCase().includes(query));
      if (query && !models.length) continue;
      menuHeader(list, g.label);
      if (!g.models.length) list.append(el('div', 'menu-header muted', 'No model set'));
      for (const name of models) {
        const checked = state.active && state.active.kind === g.kind && state.active.model === name;
        menuItem(list, { title: name, checked, onSelect: () => pick(g.kind, name) });
        shown++;
      }
    }
    if (query && !shown) list.append(el('div', 'menu-empty', 'No models match.'));
    const raw = q.value.trim();
    if (raw && state.active && !state.modelGroups.some((g) => g.kind === state.active.kind && g.models.includes(raw))) {
      menuItem(list, { icon: 'add', title: `Use "${raw}"`, desc: 'Set this model id for the current provider.', onSelect: () => pick(state.active.kind, raw) });
    }
    if (focused) q.focus();
    modelMenu.menu.place?.();
  }
  modelBtn.addEventListener('click', () => {
    vscode.postMessage({ type: 'loadModels' });
    openMenu(modelBtn, (menu) => {
      const box = el('div', 'menu-search');
      const q = el('input');
      q.type = 'search';
      q.placeholder = 'Search models';
      q.setAttribute('aria-label', 'Search models');
      q.maxLength = 256;
      q.addEventListener('input', fillModels);
      q.addEventListener('keydown', (e) => {
        if (e.key !== 'Enter') return;
        e.preventDefault();
        [...list.querySelectorAll('.menu-item')][0]?.click();
      });
      box.append(icon('search'), q);
      const list = el('div', 'menu-list');
      const tabs = el('div', 'model-tabs');
      tabs.setAttribute('role', 'tablist');
      tabs.setAttribute('aria-label', 'Providers');
      menu.classList.add('searchable');
      menu.addEventListener('toggle', (e) => { if (e.newState === 'closed' && modelMenu?.menu === menu) modelMenu = null; });
      menu.append(box, tabs, list);
      modelMenu = { menu, list, input: q, tabs, tab: state.active?.kind ?? 'all' };
      fillModels();
      menuSep(menu);
      menuItem(menu, { icon: 'add', title: 'Add provider…', onSelect: () => vscode.postMessage({ type: 'addProvider' }) });
      if (state.auth?.loggedIn) menuItem(menu, { icon: 'sign-out', title: 'Sign out of Anthropic', desc: state.auth.email, onSelect: () => vscode.postMessage({ type: 'signOut' }) });
      else menuItem(menu, { icon: 'account', title: 'Sign in with Anthropic…', desc: 'Use your Claude account through Claude Code.', onSelect: () => vscode.postMessage({ type: 'signIn' }) });
    });
  });
  permBtn.addEventListener('click', () => openMenu(permBtn, (menu) => {
    menuHeader(menu, 'Edits and commands');
    for (const [id, a] of Object.entries(APPROVALS)) menuItem(menu, { icon: a.icon, title: a.title, desc: a.desc, checked: state.approval === id, onSelect: () => vscode.postMessage({ type: 'selectApproval', mode: id }) });
    menuSep(menu);
    menuItem(menu, { icon: 'settings-gear', title: 'Configure allowed tools…', onSelect: () => vscode.postMessage({ type: 'openSettings', query: 'task-x.agent' }) });
  }));

  autoCommitBox.addEventListener('change', () => vscode.postMessage({ type: 'selectAutoCommit', enabled: autoCommitBox.checked }));

  // ---------- Markdown (fenced code, inline code, bold, headings, lists) ----------
  function renderInline(parent, text) {
    let last = 0;
    for (const m of text.matchAll(/(`[^`\n]+`|\*\*[^*\n]+\*\*)/g)) {
      if (m.index > last) parent.append(text.slice(last, m.index));
      const t = m[0];
      parent.append(t.startsWith('`') ? el('code', null, t.slice(1, -1)) : el('strong', null, t.slice(2, -2)));
      last = m.index + t.length;
    }
    if (last < text.length) parent.append(text.slice(last));
  }
  function renderMarkdown(container, text) {
    container.replaceChildren();
    const lines = text.split('\n');
    let list = null, para = [];
    const flush = () => { if (para.length) { const p = el('p'); renderInline(p, para.join('\n')); container.append(p); para = []; } };
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      const fence = line.match(/^\s*(`{3,}|~{3,})(.*)$/);
      if (fence) {
        flush(); list = null;
        const body = [];
        for (i++; i < lines.length && !lines[i].trim().startsWith(fence[1]); i++) body.push(lines[i]);
        const code = body.join('\n');
        if (/^(?:task-x|anvil)-findings$/.test(fence[2].trim())) { container.append(findingsChip(code)); continue; }
        const wrap = el('div', 'code'), head = el('div', 'code-head');
        head.append(el('span', null, fence[2].trim() || 'code'));
        const copy = button('action', icon('copy'), () => navigator.clipboard.writeText(code).then(() => {
          copy.replaceChildren(icon('check')); setTimeout(() => copy.replaceChildren(icon('copy')), 1200);
        }), 'Copy');
        head.append(copy);
        const pre = el('pre');
        pre.append(highlight(code, fence[2].trim().split(/\s/)[0]));
        wrap.append(head, pre);
        container.append(wrap);
        continue;
      }
      const h = line.match(/^(#{1,4})\s+(.*)$/), li = line.match(/^\s*(?:[-*]|\d+\.)\s+(.*)$/);
      if (h) { flush(); list = null; const e = el('h' + Math.min(6, h[1].length + 2)); renderInline(e, h[2]); container.append(e); }
      else if (li) {
        flush();
        if (!list) { list = el(/^\s*\d+\./.test(line) ? 'ol' : 'ul'); container.append(list); }
        const e = el('li'); renderInline(e, li[1]); list.append(e);
      } else if (!line.trim()) { flush(); list = null; }
      else { list = null; para.push(line); }
    }
    flush();
  }
  // Syntax highlighting with highlight.js (common languages). Its output escapes all source text,
  // so the HTML it returns holds only its own <span class="hljs-…"> markup around escaped text.
  const MAX_HIGHLIGHT = 20_000;
  const LANG_ALIAS = { sh: 'bash', zsh: 'bash', shell: 'bash', console: 'bash', ts: 'typescript', js: 'javascript', py: 'python', rs: 'rust', yml: 'yaml', md: 'markdown', jsonc: 'json', tsx: 'typescript', jsx: 'javascript' };
  function highlight(code, lang) {
    const c = el('code', null, code);
    const hl = window.hljs;
    if (!hl || code.length > MAX_HIGHLIGHT) return c;
    const name = LANG_ALIAS[lang?.toLowerCase()] || lang?.toLowerCase();
    try {
      const res = name && hl.getLanguage(name) ? hl.highlight(code, { language: name, ignoreIllegals: true }) : code.length < 4_000 && code.includes('\n') ? hl.highlightAuto(code) : null;
      if (res && res.relevance !== 0) {
        c.innerHTML = res.value;
        c.classList.add('hljs');
      }
    } catch { /* plain text */ }
    return c;
  }

  // Review findings go to the Problems panel; the chat shows a count instead of the JSON.
  function findingsChip(code) {
    let n = null;
    try { const d = JSON.parse(code); if (Array.isArray(d)) n = d.length; } catch { /* still streaming */ }
    const chip = el('div', 'findings');
    chip.append(icon(n ? 'warning' : 'pass'), el('span', null, n == null ? 'Collecting findings…' : n ? `${n} finding${n > 1 ? 's' : ''} in the Problems panel` : 'No findings'));
    if (n) chip.append(button('link', 'Show', () => vscode.postMessage({ type: 'showProblems' })));
    return chip;
  }
  function scheduleRender() {
    if (renderQueued) return;
    renderQueued = true;
    requestAnimationFrame(() => { renderQueued = false; if (current) { renderMarkdown(current.body, current.text); scroll(); } });
  }

  // ---------- Transcript ----------
  // Empty chat: only the animated logo (orbiting rings), no text or buttons.
  function welcome() {
    if (log.children.length) return;
    const w = el('div', 'welcome');
    w.setAttribute('role', 'img');
    w.setAttribute('aria-label', 'New chat');
    const orb = el('div', 'orb');
    orb.append(el('span', 'glow'), el('span', 'ring r1'), el('span', 'ring r2'), el('span', 'ring r3'), el('span', 'logo'));
    w.append(orb);
    log.append(w);
  }
  function response() {
    if (!turn) { turn = { el: el('div', 'turn'), response: null }; log.append(turn.el); }
    if (!turn.response) {
      turn.response = el('div', 'response');
      turn.el.append(turn.response);
    }
    return turn.response;
  }
  // Consecutive tool, file, and note rows share one timeline.
  function step(row) {
    const res = response();
    let tl = res.lastElementChild;
    if (!tl?.classList.contains('timeline')) { tl = el('div', 'timeline'); res.append(tl); }
    row.prepend(el('span', 'dot'));
    tl.append(enter(row));
  }
  // One edit as it happened: its own counts and diff, with Keep and Undo (Undo reverts only this edit).
  function editCard(it) {
    const card = el('div', 'edit' + (it.undone ? ' undone' : ''));
    const slash = it.path.lastIndexOf('/');
    const head = el('div', 'edit-head');
    const name = el('span', 'edit-name', it.path.slice(slash + 1));
    name.title = it.path;
    const stat = el('span', 'stat');
    stat.append(el('span', 'add', `+${it.added ?? 0}`), el('span', 'del', `-${it.removed ?? 0}`));
    const acts = el('span', 'edit-acts');
    const keep = button('action', icon('check'), () => vscode.postMessage({ type: 'keep', paths: [it.path] }), `Keep changes to ${it.path}`);
    const undo = button('action', icon('discard'), () => vscode.postMessage({ type: 'undo', operation: it.operation }), `Undo this edit to ${it.path}`);
    keep.disabled = undo.disabled = Boolean(it.undone) || it.operation == null;
    if (!it.kept) acts.append(keep, undo);
    head.append(icon(it.added && !it.removed ? 'diff-added' : it.removed && !it.added ? 'diff-removed' : 'diff-modified'), name);
    if (slash >= 0) head.append(el('span', 'edit-dir', it.path.slice(0, slash)));
    head.append(stat, el('span', 'spacer'), acts);
    card.append(head);
    const text = (it.diff || '').replace(/\n$/, '');
    if (!text) return card;
    const lines = text.split('\n');
    const body = el('details', 'edit-diff');
    body.open = lines.length <= 12;
    body.append(el('summary', null, it.undone ? 'Change (undone)' : 'Change'));
    const pre = el('pre', 'diff');
    for (const l of lines) {
      const kind = l.startsWith('+++ ') || l.startsWith('--- ') ? 'meta' : l.startsWith('@@') ? 'hunk' : l[0] === '+' ? 'add' : l[0] === '-' ? 'del' : 'ctx';
      pre.append(el('span', `dl ${kind}`, l));
    }
    body.append(pre);
    if (it.diffCut) body.append(el('div', 'edit-note', 'Change shortened. Open the file to see all of it.'));
    card.append(body);
    return card;
  }
  function renderItem(it) {
    log.querySelector('.welcome')?.remove();
    switch (it.kind) {
      case 'user': {
        turn = { el: el('div', 'turn'), response: null };
        const req = enter(el('div', 'request'));
        req.append(el('div', 'request-text', it.text), button('action copy-msg', icon('copy'), () => copyText(it.text), 'Copy message'));
        turn.el.append(req);
        log.append(turn.el);
        current = null;
        break;
      }
      case 'assistant': {
        const body = el('div', 'md');
        const msg = enter(el('div', 'message'));
        msg.append(body, button('action copy-msg', icon('copy'), () => copyText(current?.body === body ? current.text : body.innerText), 'Copy response'));
        response().append(msg);
        renderMarkdown(body, it.text);
        current = { body, text: it.text };
        break;
      }
      case 'tool': {
        const row = el('div', 'row tool');
        row.dataset.id = it.id;
        const st = it.ok === undefined ? 'run' : it.ok ? 'ok' : 'fail';
        row.dataset.state = st;
        const [owner, tool] = it.tool.includes(' › ') ? it.tool.split(' › ') : [null, it.tool];
        const plain = TOOL_LABELS[tool] || tool.replace(/^mcp__([^_]+)__/, '$1: ');
        const label = (owner ? owner + ' › ' : '') + (EDIT_TOOLS.has(tool) ? { run: 'Editing', fail: 'Edit failed' }[st] || plain : plain);
        // A running or failed edit shows as a row; once it succeeds, its edit card replaces the row.
        if (EDIT_TOOLS.has(tool)) { row.dataset.edit = '1'; if (it.ok === true) row.classList.add('quiet'); }
        const lab = el('span', 'label', label);
        lab.dataset.tool = it.tool;
        row.append(lab, el('span', 'detail', it.detail || ''));
        step(row);
        attachOutput(row, it.id, it.tool, it.ok !== false, it.output, it.outputCut);
        current = null;
        break;
      }
      case 'file':
        step(editCard(it));
        current = null;
        break;
      case 'compressed': {
        // How much context compression saved, and how.
        const row = el('div', 'task-info');
        const chip = el('span', 'tchip squeeze');
        const how = [it.pruned ? `${it.pruned} old tool result${it.pruned > 1 ? 's' : ''} shortened` : '', it.folded ? `${it.folded} earlier messages summarized` : '', it.dropped ? 'repository excerpts removed' : ''].filter(Boolean).join(', ');
        chip.title = `${how}. The window is ${fmtCount(it.window)} tokens.`;
        chip.append(icon('fold'), el('span', null, it.before ? `Context compressed: ${fmtCount(it.before)} → ${fmtCount(it.after)} tokens` : 'Context compressed'));
        row.append(chip);
        if (how) row.append(el('span', 'tnote', how));
        step(row);
        current = null;
        break;
      }
      case 'taskInfo': {
        response().append(enter(taskInfo(it)));
        current = null;
        break;
      }
      case 'steer': {
        // A message the agent read during the run, shown where it joined.
        const row = enter(el('div', 'steer'));
        row.append(icon('arrow-right'), el('span', 'steer-text', it.text));
        response().append(row);
        current = null;
        break;
      }
      case 'context': {
        const row = el('div', 'row note');
        row.append(el('span', null, it.text));
        step(row);
        break;
      }
      case 'summary': {
        const s = enter(el('div', 'summary ' + (it.ok ? 'ok' : it.signIn ? 'auth' : 'warn')));
        const head = el('div', 'summary-head');
        head.append(icon(it.ok ? 'pass-filled' : it.signIn ? 'account' : 'warning'), el('span', 'summary-title', it.title ?? it.text));
        s.append(head);
        if (it.title === undefined) { response().append(s); current = null; break; }
        if (it.error) s.append(el('div', 'summary-error', it.error));
        if (it.stats?.length) {
          const chips = el('div', 'chips');
          for (const t of it.stats) chips.append(el('span', 'chip', t));
          s.append(chips);
        }
        const acts = el('div', 'summary-actions');
        if (it.signIn) acts.append(button('btn small', [icon('sign-in'), 'Sign in with Anthropic'], () => vscode.postMessage({ type: 'signIn' })));
        if (it.raiseLimit) acts.append(button('btn small secondary', [icon('settings-gear'), 'Raise limit'], () => vscode.postMessage({ type: 'openSettings', query: 'task-x.agent.maxIterations' }), 'Change the turn limit'));
        if (it.canContinue) {
          acts.append(button('btn small secondary', [icon('debug-continue'), 'Continue'], () => {
            vscode.postMessage({ type: 'send', text: 'Continue the task from where you stopped. Check what is already done, finish the remaining work, then run the build and tests.' });
          }, 'Continue this task'));
        }
        if (acts.childElementCount) s.append(acts);
        response().append(s);
        current = null;
        break;
      }
      case 'lane': {
        response().append(enter(laneCard(it)));
        current = null;
        break;
      }
      case 'error': {
        const e = enter(el('div', 'error'));
        e.setAttribute('role', 'alert');
        e.append(icon('error'), el('span', null, it.text));
        if (it.retryText) e.append(button('link', 'Retry', () => { e.remove(); vscode.postMessage({ type: 'send', text: it.retryText }); }));
        response().append(e);
        current = null;
        break;
      }
    }
  }
  // What the agent works with: the task kind, the context files it was given, instructions, skills.
  const KIND_ICONS = { question: 'question', bugInvestigation: 'bug', codeModification: 'code', newFeature: 'sparkle', refactoring: 'wand', testing: 'beaker', documentation: 'book', repositoryWide: 'repo', architecture: 'type-hierarchy' };
  const MAX_INFO_FILES = 4;
  function taskInfo(it) {
    const row = el('div', 'task-info');
    row.setAttribute('aria-label', 'Task context');
    const kind = el('span', 'tchip kind');
    kind.append(icon(KIND_ICONS[it.taskKind] || 'circle-filled'), el('span', null, it.label));
    row.append(kind);
    const files = it.files || [];
    for (const f of files.slice(0, MAX_INFO_FILES)) {
      const b = button('tchip file', [icon('file'), el('span', 'tname', f.split('/').pop())], () => vscode.postMessage({ type: 'openFile', path: f }), `Context: ${f}`);
      row.append(b);
    }
    if (files.length > MAX_INFO_FILES) {
      const more = el('span', 'tchip more', `+${files.length - MAX_INFO_FILES}`);
      more.title = files.slice(MAX_INFO_FILES).join('\n');
      row.append(more);
    }
    for (const ins of it.instructions || []) {
      const c = el('span', 'tchip instr');
      c.title = `Instructions: ${ins.path}`;
      c.append(icon('book'), el('span', 'tname', ins.label));
      row.append(c);
    }
    for (const sk of it.skills || []) {
      const c = el('span', 'tchip skill');
      c.title = `Skill: ${sk}`;
      c.append(icon('zap'), el('span', 'tname', sk));
      row.append(c);
    }
    return row;
  }

  // A subagent lane: role, model, live tool steps, result, and Stop / Apply / Discard / Open.
  const LANE_STATUS = { running: 'Running', completed: 'Done', cancelled: 'Stopped', failed: 'Failed', maxIterations: 'Turn limit', stuck: 'Stuck', providerError: 'Model error', deadlineExceeded: 'Time limit', budgetExceeded: 'Budget' };
  // `seen` is the number of steps already on screen; later steps animate in.
  function laneCard(it, seen) {
    const card = el('section', 'lane');
    card.dataset.taskId = it.taskId;
    card.dataset.hidden = String(it.hidden || 0);
    card.dataset.role = it.role;
    card.dataset.state = it.status === 'running' ? 'run' : it.status === 'completed' ? 'ok' : 'fail';
    card.setAttribute('aria-label', `Subagent ${it.name}`);
    const head = el('div', 'lane-head');
    const role = el('span', 'lane-role');
    role.append(icon(ROLE_ICONS[it.role] || 'person'), el('span', null, it.step || it.role));
    head.append(role, el('span', 'lane-model', it.model), el('span', 'spacer'), el('span', 'lane-status', LANE_STATUS[it.status] || it.status));
    if (it.status === 'running') head.append(button('action', icon('debug-stop'), () => vscode.postMessage({ type: 'stopLane', taskId: it.taskId }), 'Stop this subagent'));
    card.append(head, el('div', 'lane-goal', it.objective));
    const body = el('details', 'lane-body');
    body.open = it.status === 'running';
    const steps = el('summary', null, `${(it.hidden || 0) + it.steps.length} step${(it.hidden || 0) + it.steps.length === 1 ? '' : 's'}`);
    body.append(steps);
    if (it.hidden) body.append(el('div', 'lane-step muted', `${it.hidden} earlier steps`));
    for (const st of it.steps) {
      const row = el('div', 'lane-step');
      if (seen !== undefined && body.querySelectorAll('.lane-step:not(.muted)').length >= seen) row.classList.add('enter');
      row.dataset.state = st.ok === undefined ? 'run' : st.ok ? 'ok' : 'fail';
      row.append(el('span', 'dot'), el('span', 'label', TOOL_LABELS[st.tool] || st.tool), el('span', 'detail', st.detail || ''));
      body.append(row);
    }
    if (it.steps.length || it.hidden) card.append(body);
    if (it.error) card.append(el('div', 'lane-error', it.error));
    const foot = el('div', 'lane-foot');
    const chips = el('div', 'chips');
    if (it.files) { const c = el('span', 'chip stat'); c.append(`${it.files} file${it.files > 1 ? 's' : ''} `, el('span', 'add', `+${it.added || 0}`), ' ', el('span', 'del', `-${it.removed || 0}`)); chips.append(c); }
    if (it.cost) chips.append(el('span', 'chip', it.cost));
    else if (it.tokens) chips.append(el('span', 'chip', `${it.tokens} tokens`));
    if (it.merge) chips.append(el('span', 'chip merge-' + it.merge, { merged: 'Merged', conflict: 'Merge conflict', discarded: 'Discarded' }[it.merge]));
    foot.append(chips, el('span', 'spacer'));
    const pending = it.worktree && it.files && it.status !== 'running' && (!it.merge || it.merge === 'conflict');
    if (pending) {
      foot.append(
        button('btn small', [icon('check'), 'Apply'], () => vscode.postMessage({ type: 'applyWorktree', name: it.worktree }), 'Apply this worktree to the working tree'),
        button('btn small secondary', 'Discard', () => vscode.postMessage({ type: 'discardWorktree', name: it.worktree }), 'Delete this worktree'),
      );
    }
    if (it.dir && it.status !== 'running' && it.merge !== 'merged' && it.merge !== 'discarded') foot.append(button('action', icon('folder-opened'), () => vscode.postMessage({ type: 'openWorktree', name: it.name }), 'Open the worktree in a new window'));
    if (chips.childElementCount || foot.childElementCount > 2) card.append(foot);
    if (it.mergeError) card.append(el('div', 'lane-error', it.mergeError));
    return card;
  }

  // Terminal-style output: scrollable (and resizable), with Copy and Open in Editor.
  const EXPANDED = new Set(['run_command', 'Bash']);
  function outputBlock(id, output, cut, expanded) {
    const box = el('div', 'term');
    box.dataset.id = id;
    box.hidden = !expanded;
    const head = el('div', 'term-head');
    head.append(icon('terminal'), el('span', null, 'Output'), el('span', 'spacer'));
    head.append(
      button('action', icon('copy'), () => vscode.postMessage({ type: 'copyOutput', id }), 'Copy output'),
      button('action', icon('go-to-file'), () => vscode.postMessage({ type: 'openOutput', id }), 'Open in editor'),
    );
    const body = el('pre', 'term-body', output);
    body.tabIndex = 0;
    body.setAttribute('aria-label', 'Command output');
    box.append(head, body);
    if (cut) box.append(el('div', 'term-note', 'Showing the start of the output. Open in editor to see all of it.'));
    return box;
  }
  function attachOutput(row, id, tool, ok, output, cut) {
    if (!output || row.nextElementSibling?.classList.contains('term')) return;
    const base = tool.split(' › ').pop();
    const box = outputBlock(id, output, cut, EXPANDED.has(base) || !ok);
    const toggle = button('action', icon(box.hidden ? 'chevron-right' : 'chevron-down'), () => {
      box.hidden = !box.hidden;
      toggle.replaceChildren(icon(box.hidden ? 'chevron-right' : 'chevron-down'));
      toggle.setAttribute('aria-expanded', String(!box.hidden));
    }, 'Show output');
    toggle.setAttribute('aria-expanded', String(!box.hidden));
    row.append(el('span', 'spacer'), toggle);
    row.after(box);
  }

  function renderApproval(m) {
    log.querySelector('.welcome')?.remove();
    current = null;
    const verb = { write: 'Allow edit', delete: 'Allow delete', execute: 'Allow command', network: 'Allow external tool' }[m.permission] || 'Allow tool';
    const card = enter(el('div', 'approval'));
    card.dataset.callId = m.callId;
    const head = el('div', 'approval-head');
    head.append(icon('shield'), el('span', null, `${verb}?`));
    card.append(head);
    card.append(el('div', 'detail', m.detail ? `${m.tool} · ${m.detail}` : m.tool));
    if (m.preview) {
      const d = el('details');
      if (m.preview.length < 600) d.open = true;
      d.append(el('summary', null, 'Details'), el('pre', null, m.preview));
      card.append(d);
    }
    const decide = (decision) => vscode.postMessage({ type: 'approve', callId: m.callId, decision });
    const actions = el('div', 'actions');
    const split = el('div', 'split');
    const more = button('btn', icon('chevron-down'), () => openMenu(more, (menu) => {
      menuItem(menu, { icon: 'check', title: 'Allow once', onSelect: () => decide('allow') });
      menuItem(menu, { icon: 'clock', title: 'Allow for this session', desc: `Don't ask again for ${m.tool} until VS Code restarts.`, onSelect: () => decide('allowSession') });
      menuItem(menu, { icon: 'check-all', title: `Always allow ${m.tool}`, desc: 'Adds it to task-x.agent.allowTools.', onSelect: () => decide('always') });
    }), 'More allow options');
    more.setAttribute('aria-haspopup', 'menu');
    split.append(button('btn', 'Allow', () => decide('allow')), more);
    actions.append(split, button('btn secondary', 'Deny', () => decide('deny')));
    card.append(actions);
    response().append(card);
    scroll(true);
    split.firstChild.focus();
  }

  // ---------- Changed files ----------
  const changes = $('changes'), changesList = $('changesList'), toggle = $('changesToggle');
  let collapsed = (vscode.getState() || {}).changesCollapsed === true;
  const applyCollapsed = () => { changes.classList.toggle('collapsed', collapsed); toggle.setAttribute('aria-expanded', String(!collapsed)); };
  toggle.addEventListener('click', () => { collapsed = !collapsed; vscode.setState({ ...(vscode.getState() || {}), changesCollapsed: collapsed }); applyCollapsed(); });
  $('keepAll').addEventListener('click', () => vscode.postMessage({ type: 'keep' }));
  $('revertAll').addEventListener('click', () => vscode.postMessage({ type: 'revert' }));
  function renderChanges(files) {
    changes.hidden = !files.length;
    if (!files.length) return void changesList.replaceChildren();
    const added = files.reduce((n, f) => n + f.added, 0), removed = files.reduce((n, f) => n + f.removed, 0);
    $('changesTitle').textContent = `${files.length} file${files.length > 1 ? 's' : ''} changed`;
    $('changesStat').replaceChildren(el('span', 'add', `+${added}`), el('span', 'del', `-${removed}`));
    changesList.replaceChildren(...files.map((f) => {
      const li = el('li', 'change ' + f.status);
      const slash = f.path.lastIndexOf('/');
      const open = button('file-btn', [], () => vscode.postMessage({ type: 'diffFile', path: f.path }), `Show changes in ${f.path}`);
      open.append(icon('file'), el('span', 'fname', slash >= 0 ? f.path.slice(slash + 1) : f.path));
      if (slash >= 0) open.append(el('span', 'dir', f.path.slice(0, slash)));
      const stat = el('span', 'stat');
      stat.append(el('span', 'add', `+${f.added}`), el('span', 'del', `-${f.removed}`));
      const acts = el('span', 'acts');
      acts.append(
        button('action', icon('check'), () => vscode.postMessage({ type: 'keep', paths: [f.path] }), `Keep changes to ${f.path}`),
        button('action', icon('discard'), () => vscode.postMessage({ type: 'revert', paths: [f.path] }), `Undo changes to ${f.path}`),
      );
      li.append(open, stat, acts, el('span', 'letter', { added: 'A', deleted: 'D', modified: 'M' }[f.status]));
      return li;
    }));
    applyCollapsed();
  }

  // ---------- Toolbar state ----------
  function renderToolbar() {
    const m = MODES[state.mode] || MODES.agent;
    const mi = Math.max(0, modeIds.indexOf(state.mode));
    document.body.dataset.mode = modeIds[mi];
    modes.style.setProperty('--sel', String(mi));
    modeTabs.forEach((t, i) => { t.setAttribute('aria-selected', String(i === mi)); t.tabIndex = i === mi ? 0 : -1; t.disabled = state.busy; });
    const model = state.active?.model || 'Pick a model';
    modelBtn.replaceChildren(el('span', 'text', model), icon('chevron-down'));
    modelBtn.title = `Model: ${model}`;
    const readOnly = state.mode !== 'agent';
    const a = APPROVALS[state.approval] || APPROVALS.askBeforeExecution;
    permBtn.replaceChildren(icon(readOnly ? 'lock' : a.icon), el('span', 'text', readOnly ? 'Read-only' : a.label), ...(readOnly ? [] : [icon('chevron-down')]));
    permBtn.disabled = readOnly || state.busy;
    permBtn.title = readOnly ? `${m.label} mode is read-only` : `Approvals: ${a.title}. ${a.desc}`;
    autoCommitBox.checked = state.autoCommit;
    modelBtn.disabled = state.busy;
    if (send.dataset.busy !== String(state.busy)) {
      send.dataset.busy = String(state.busy);
      send.classList.remove('morph');
      void send.offsetWidth; // restart the icon transition
      send.classList.add('morph');
    }
    send.replaceChildren(icon(state.busy ? 'debug-stop' : 'send'));
    send.title = state.busy ? 'Stop' : 'Send (Enter)';
    send.setAttribute('aria-label', send.title);
    send.classList.toggle('primary', !state.busy);
    progress.classList.toggle('busy', state.busy);
    // While working, the input box gets the animated border (the top bar is no longer drawn).
    const form = $('form');
    form.classList.toggle('busy', state.busy);
    form.setAttribute('aria-busy', String(state.busy));
    input.placeholder = state.busy ? 'Enter steers the running task. Ctrl+Enter queues the next message. Stop ends the task.' : `${m.placeholder} (Enter to send, Shift+Enter for a new line)`;
  }
  for (const b of [modelBtn, permBtn]) { b.setAttribute('aria-haspopup', 'menu'); b.setAttribute('aria-expanded', 'false'); }

  // ---------- Context chips: the active file and selection that go with the message ----------
  const ctxRow = $('ctx'), indexBtn = $('indexBtn');
  // Messages waiting for the running task; each one is sent when the one before it finishes.
  const queueBox = $('queue');
  // Steers the agent has not read yet come first (arrow), then queued messages (clock).
  function renderQueue(items, steering = []) {
    const rows = [...steering.map((t) => ['arrow-right', t]), ...items.map((t) => ['clock', t])];
    queueBox.replaceChildren(...rows.map(([name, t]) => {
      const row = el('div', 'queued');
      row.append(icon(name), el('span', null, t));
      return row;
    }));
    queueBox.hidden = !rows.length;
  }
  function renderContext(m) {
    ctxRow.replaceChildren();
    ctxRow.hidden = !m.file;
    if (!m.file) return;
    const chip = el('span', 'ctx-chip' + (m.include ? '' : ' off'));
    const name = m.file.split('/').pop();
    chip.title = m.include ? `${m.file}${m.selection ? `, lines ${m.selection.start}-${m.selection.end}` : ''} is sent with your message` : `${m.file} is not sent`;
    chip.append(icon(m.selection ? 'selection' : 'file'), el('span', 'ctx-name', name));
    if (m.selection) chip.append(el('span', 'ctx-lines', m.selection.start === m.selection.end ? `L${m.selection.start}` : `L${m.selection.start}-${m.selection.end}`));
    const toggle = button('ctx-toggle', icon(m.include ? 'eye' : 'eye-closed'), () => vscode.postMessage({ type: 'toggleEditor', include: !m.include }), m.include ? 'Do not send the current file' : 'Send the current file');
    toggle.setAttribute('aria-pressed', String(m.include));
    chip.append(toggle);
    ctxRow.append(chip, el('span', 'ctx-hint', 'Type @ to add files'));
  }
  const fmtCount = (n) => (n >= 10_000 ? `${Math.round(n / 1000)}k` : n >= 1000 ? `${(n / 1000).toFixed(1)}k` : String(n));
  function renderIndex() {
    const ix = state.index || { state: 'none' };
    indexBtn.hidden = ix.state === 'none';
    indexBtn.classList.toggle('busy', ix.state === 'indexing');
    indexBtn.classList.toggle('bad', ix.state === 'failed');
    if (ix.state === 'indexing') {
      indexBtn.replaceChildren(icon('loading', 'codicon-modifier-spin'), el('span', 'text', ix.phase === 'graph' ? 'Graph…' : `${fmtCount(ix.files)} files`));
      indexBtn.title = ix.phase === 'graph' ? 'Building the project graph' : `Indexing: ${ix.files} files seen, ${ix.parsed} parsed`;
    } else if (ix.state === 'ready') {
      indexBtn.replaceChildren(icon('type-hierarchy'), el('span', 'text', `${fmtCount(ix.files)}`));
      const g = ix.graph;
      indexBtn.title = `Indexed ${ix.files} files and ${ix.symbols} symbols${g ? `. Graph: ${g.modules} modules, ${g.importEdges} imports, ${g.referenceEdges} symbol links, ${g.cycles} import cycles` : ''}. Click to open the project graph.`;
    } else if (ix.state === 'failed') {
      indexBtn.replaceChildren(icon('warning'), el('span', 'text', 'Index'));
      indexBtn.title = `Indexing failed: ${ix.error}`;
    }
    indexBtn.setAttribute('aria-label', indexBtn.title);
  }
  indexBtn.addEventListener('click', () => vscode.postMessage({ type: 'showGraph' }));

  // Context meter: how full the model's context window was on the last request.
  const meter = $('ctxMeter');
  function renderMeter(used, window) {
    if (!window) { meter.hidden = true; return; }
    const pct = Math.min(100, Math.round((used / window) * 100));
    meter.hidden = false;
    meter.style.setProperty('--pct', String(pct));
    meter.dataset.level = pct >= 85 ? 'high' : pct >= 60 ? 'mid' : 'low';
    meter.replaceChildren(el('span', 'ring'), el('span', 'meter-text', `${pct}%`));
    meter.title = `Context: ${fmtCount(used)} of ${fmtCount(window)} tokens. Older parts are compressed automatically when it fills up.`;
    meter.setAttribute('aria-valuenow', String(pct));
    meter.setAttribute('aria-label', meter.title);
  }

  // ---------- Views: Chat and History tabs ----------
  const viewTabs = { chat: $('tabChat'), history: $('tabHistory') };
  const historyBox = $('history');
  function showView(v) {
    document.body.dataset.view = v;
    for (const [k, b] of Object.entries(viewTabs)) { b.setAttribute('aria-selected', String(k === v)); b.tabIndex = k === v ? 0 : -1; }
    historyBox.hidden = v !== 'history';
    if (v === 'history') {
      if (!historyBox.childElementCount) historyBox.append(el('div', 'history-empty', 'Loading…'));
      vscode.postMessage({ type: 'listTasks' });
    } else input.focus();
  }
  viewTabs.chat.addEventListener('click', () => showView('chat'));
  viewTabs.history.addEventListener('click', () => showView('history'));
  $('views').addEventListener('keydown', (e) => {
    if (e.key !== 'ArrowLeft' && e.key !== 'ArrowRight') return;
    const next = document.body.dataset.view === 'chat' ? 'history' : 'chat';
    e.preventDefault();
    showView(next);
    viewTabs[next].focus();
  });
  $('newChat').addEventListener('click', () => { vscode.postMessage({ type: 'clear' }); showView('chat'); });

  // Saved tasks, newest first, grouped by day.
  const STATUS_ICONS = { completed: 'pass-filled', running: 'loading codicon-modifier-spin', cancelled: 'circle-slash', failed: 'error' };
  function dayGroup(ms) {
    const d = new Date(ms), now = new Date();
    const start = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime();
    if (ms >= start) return 'Today';
    if (ms >= start - 86_400_000) return 'Yesterday';
    if (ms >= start - 6 * 86_400_000) return 'Last 7 days';
    if (d.getFullYear() === now.getFullYear()) return d.toLocaleString(undefined, { month: 'long' });
    return String(d.getFullYear());
  }
  function renderTasks(m) {
    historyBox.replaceChildren();
    if (m.error) return void historyBox.append(el('div', 'history-empty', `Could not load history: ${m.error}`));
    if (!m.tasks.length) return void historyBox.append(el('div', 'history-empty', 'No saved tasks yet.'));
    let group = null, list = null;
    m.tasks.forEach((t, i) => {
      const g = dayGroup(t.updatedMs);
      if (g !== group) {
        group = g;
        historyBox.append(el('div', 'history-group', g));
        list = el('div', 'history-list');
        historyBox.append(list);
      }
      const [name, ...mods] = (STATUS_ICONS[t.status] || 'warning').split(' ');
      const row = button('history-row' + (t.id === m.current ? ' current' : ''), [], () => vscode.postMessage({ type: 'openTask', id: t.id }), t.title);
      row.style.setProperty('--i', String(Math.min(i, 20)));
      const time = new Date(t.updatedMs);
      const when = g === 'Today' || g === 'Yesterday' ? time.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' }) : time.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
      const meta = [t.mode === 'ask' ? 'Ask' : 'Agent', t.files ? `${t.files} file${t.files > 1 ? 's' : ''}` : '', when].filter(Boolean).join(' · ');
      const text = el('span', 'history-text');
      text.append(el('span', 'history-title', t.title), el('span', 'history-meta', meta));
      row.append(icon(name, ['status', t.status, ...mods].join(' ')), text);
      list.append(row);
    });
  }

  // ---------- Copy ----------
  const copyText = (text) => navigator.clipboard.writeText(text).catch(() => vscode.postMessage({ type: 'copyText', text }));
  // Cmd/Ctrl+C on selected transcript text; the input keeps its native copy.
  document.addEventListener('keydown', (e) => {
    if (!(e.metaKey || e.ctrlKey) || e.key.toLowerCase() !== 'c' || e.target === input) return;
    const text = getSelection()?.toString();
    if (text) { e.preventDefault(); copyText(text); }
  });

  // ---------- Input ----------
  const autosize = () => { input.style.height = 'auto'; input.style.height = Math.min(input.scrollHeight, innerHeight * 0.4) + 'px'; };
  // Enter steers a running task (or sends when idle); Ctrl/Cmd+Enter queues the message for after it.
  // The button stops a running task.
  let queueNext = false;
  $('form').addEventListener('submit', (e) => {
    e.preventDefault();
    const queue = queueNext;
    queueNext = false;
    const text = input.value.trim();
    if (!text) return;
    input.value = '';
    autosize();
    rememberPrompt(text);
    vscode.postMessage({ type: 'send', text, queue });
    scroll(true);
  });
  send.addEventListener('click', () => (state.busy ? vscode.postMessage({ type: 'cancel' }) : $('form').requestSubmit()));

  // ---------- Prompt history: Up and Down recall sent messages, like a shell ----------
  const MAX_PROMPTS = 100;
  let promptHistory = vscode.getState()?.prompts ?? [];
  let histIndex = -1, histDraft = '';
  function rememberPrompt(text) {
    if (promptHistory.at(-1) !== text) promptHistory = [...promptHistory, text].slice(-MAX_PROMPTS);
    vscode.setState({ ...(vscode.getState() || {}), prompts: promptHistory });
    histIndex = -1;
  }
  function recallPrompt(i) {
    histIndex = i;
    input.value = i < 0 ? histDraft : promptHistory[i];
    input.setSelectionRange(input.value.length, input.value.length);
    autosize();
  }
  input.addEventListener('keydown', (e) => {
    if (e.isComposing || e.altKey || e.ctrlKey || e.metaKey || e.shiftKey || slashOpen) return;
    const beforeCaret = input.value.slice(0, input.selectionStart);
    const afterCaret = input.value.slice(input.selectionEnd);
    if (e.key === 'ArrowUp' && !beforeCaret.includes('\n') && histIndex < promptHistory.length - 1) {
      e.preventDefault();
      if (histIndex < 0) histDraft = input.value;
      recallPrompt(histIndex + 1);
    } else if (e.key === 'ArrowDown' && histIndex >= 0 && !afterCaret.includes('\n')) {
      e.preventDefault();
      recallPrompt(histIndex - 1);
    }
  });
  // Typing after a recall starts a new draft, so Down no longer walks the history.
  input.addEventListener('input', () => { histIndex = -1; });
  // ---------- Slash menu: "/" at the start of the input lists skills ----------
  const slash = $('slash');
  const MAX_SLASH = 50;
  let skills = null, slashItems = [], slashActive = 0, slashOpen = false;
  input.setAttribute('aria-autocomplete', 'list');
  input.setAttribute('aria-controls', 'slash');
  const slashQuery = () => /^\/([\w.-]*)$/.exec(input.value.slice(0, input.selectionStart))?.[1];
  // "@" at a word start opens the file menu; the query is what follows it, up to the caret.
  const atMatch = () => /(^|\s)@([\w./-]*)$/.exec(input.value.slice(0, input.selectionStart));
  let menuKind = 'skill', fileItems = null, fileQuery = null;
  function closeSlash() {
    slashOpen = false;
    fileQuery = null;
    slash.hidden = true;
    input.removeAttribute('aria-activedescendant');
    input.setAttribute('aria-expanded', 'false');
  }
  function acceptSlash(i = slashActive) {
    const s = slashItems[i];
    if (!s) return;
    if (menuKind === 'file') {
      const m = atMatch();
      const at = m ? input.selectionStart - m[2].length : input.selectionStart;
      const after = input.value.slice(input.selectionStart);
      const text = s.path + (s.path.endsWith('/') ? '' : ' ');
      input.value = input.value.slice(0, at) + text + after;
      const end = at + text.length;
      input.setSelectionRange(end, end);
      // A folder keeps the menu open to pick a file inside it.
      if (s.path.endsWith('/')) { renderSlash(); } else closeSlash();
      autosize();
      input.focus();
      return;
    }
    const rest = input.value.slice(input.selectionStart).replace(/^\S*/, '').trimStart();
    input.value = `/${s.name} ${rest}`;
    const end = s.name.length + 2;
    input.setSelectionRange(end, end);
    closeSlash();
    autosize();
    input.focus();
  }
  function markActive() {
    [...slash.children].forEach((c, i) => c.setAttribute('aria-selected', String(i === slashActive)));
    const a = slash.children[slashActive];
    if (a?.id) { input.setAttribute('aria-activedescendant', a.id); a.scrollIntoView({ block: 'nearest' }); }
  }
  function renderFiles(q) {
    menuKind = 'file';
    slash.setAttribute('aria-label', 'Files');
    if (fileQuery !== q) { fileQuery = q; vscode.postMessage({ type: 'listFiles', query: q }); }
    slashItems = (fileItems || []).map((p) => ({ path: p }));
    slashActive = Math.min(slashActive, Math.max(0, slashItems.length - 1));
    slash.replaceChildren();
    if (!fileItems) slash.append(el('div', 'slash-empty', 'Loading files…'));
    else if (!slashItems.length) slash.append(el('div', 'slash-empty', `No file matches "@${q}".`));
    slashItems.forEach((s, i) => {
      const o = el('div', 'slash-item file-item');
      o.id = `slash-${i}`;
      o.setAttribute('role', 'option');
      const dir = s.path.endsWith('/');
      const name = dir ? s.path.slice(0, -1).split('/').pop() + '/' : s.path.split('/').pop();
      const parent = s.path.slice(0, s.path.length - name.length).replace(/\/$/, '');
      const head = el('div', 'slash-head');
      head.append(icon(dir ? 'folder' : 'file'), el('span', 'slash-name', name));
      if (parent) head.append(el('span', 'slash-params', parent));
      o.append(head);
      o.addEventListener('mousedown', (e) => { e.preventDefault(); acceptSlash(i); });
      o.addEventListener('mousemove', () => { if (slashActive !== i) { slashActive = i; markActive(); } });
      slash.append(o);
    });
    slashOpen = true;
    slash.hidden = false;
    input.setAttribute('aria-expanded', 'true');
    markActive();
  }
  function renderSlash() {
    const at = atMatch();
    if (at) return renderFiles(at[2]);
    menuKind = 'skill';
    slash.setAttribute('aria-label', 'Skills');
    const q = slashQuery();
    if (q === undefined) return closeSlash();
    const ql = q.toLowerCase();
    const all = skills || [];
    const starts = all.filter((s) => s.name.toLowerCase().startsWith(ql));
    const rest = ql ? all.filter((s) => !starts.includes(s) && (s.name.toLowerCase().includes(ql) || s.description.toLowerCase().includes(ql))) : [];
    slashItems = [...starts, ...rest].slice(0, MAX_SLASH);
    slashActive = Math.min(slashActive, Math.max(0, slashItems.length - 1));
    slash.replaceChildren();
    if (!skills) slash.append(el('div', 'slash-empty', 'Loading skills…'));
    else if (!slashItems.length) slash.append(el('div', 'slash-empty', all.length ? `No skill matches "/${q}".` : 'No skills found. Add them under .github/skills, .claude/skills, or ~/.claude/skills.'));
    slashItems.forEach((s, i) => {
      const o = el('div', 'slash-item');
      o.id = `slash-${i}`;
      o.setAttribute('role', 'option');
      const head = el('div', 'slash-head');
      head.append(el('span', 'slash-name', `/${s.name}`));
      if (s.params?.length) head.append(el('span', 'slash-params', s.params.map((p) => `${p}=`).join(' ')));
      if (s.source) head.append(el('span', 'slash-src', s.source));
      o.append(head);
      if (s.description) o.append(el('div', 'slash-desc', s.description));
      o.addEventListener('mousedown', (e) => { e.preventDefault(); acceptSlash(i); });
      o.addEventListener('mousemove', () => { if (slashActive !== i) { slashActive = i; markActive(); } });
      slash.append(o);
    });
    if (!slashOpen || !skills) vscode.postMessage({ type: 'listSkills' });
    slashOpen = true;
    slash.hidden = false;
    input.setAttribute('aria-expanded', 'true');
    markActive();
  }
  input.addEventListener('input', () => { slashActive = 0; renderSlash(); });
  input.addEventListener('click', renderSlash);
  input.addEventListener('blur', () => setTimeout(closeSlash, 100));
  input.addEventListener('keydown', (e) => {
    if (!slashOpen || e.isComposing) return;
    const n = slashItems.length;
    if (e.key === 'ArrowDown' && n) { slashActive = (slashActive + 1) % n; markActive(); }
    else if (e.key === 'ArrowUp' && n) { slashActive = (slashActive - 1 + n) % n; markActive(); }
    else if ((e.key === 'Enter' && !e.shiftKey || e.key === 'Tab') && n) acceptSlash();
    else if (e.key === 'Escape') closeSlash();
    else return;
    e.preventDefault();
    e.stopImmediatePropagation();
  });

  input.addEventListener('keydown', (e) => { if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) { e.preventDefault(); queueNext = e.ctrlKey || e.metaKey; $('form').requestSubmit(); } });
  input.addEventListener('input', autosize);

  // ---------- Messages from the extension ----------
  window.addEventListener('message', ({ data: m }) => {
    switch (m.type) {
      case 'history':
        log.replaceChildren();
        turn = null; current = null;
        replaying = true;
        m.items.forEach(renderItem);
        replaying = false;
        current = null;
        welcome();
        scroll(true);
        break;
      case 'status':
        state = { ...state, busy: m.busy, mode: m.mode || state.mode, approval: m.approval || state.approval, autoCommit: m.autoCommit ?? state.autoCommit };
        renderToolbar();
        break;
      case 'models':
        state.modelGroups = m.groups;
        state.active = m.active;
        state.auth = m.auth;
        renderToolbar();
        fillModels();
        break;
      case 'changes': renderChanges(m.files); break;
      case 'item': renderItem(m.item); scroll(); break;
      case 'delta': if (current) { current.text += m.text; scheduleRender(); } break;
      case 'toolDone': {
        const row = log.querySelector(`.row.tool[data-id="${CSS.escape(m.id)}"]`);
        if (row) {
          row.dataset.state = m.ok ? 'ok' : 'fail';
          row.classList.toggle('quiet', Boolean(row.dataset.edit) && m.ok);
          if (row.dataset.edit && !m.ok) row.querySelector('.label').textContent = 'Edit failed';
          attachOutput(row, m.id, row.querySelector('.label')?.dataset.tool || '', m.ok, m.output, m.outputCut);
          scroll();
        }
        break;
      }
      case 'approval': renderApproval(m); break;
      case 'laneUpdate': {
        const old = log.querySelector(`.lane[data-task-id="${CSS.escape(m.item.taskId)}"]`);
        if (old) {
          const open = old.querySelector('details')?.open;
          const next = laneCard(m.item, old.querySelectorAll('.lane-step:not(.muted)').length - ((m.item.hidden || 0) - (Number(old.dataset.hidden) || 0)));
          if (open !== undefined && next.querySelector('details') && m.item.status === 'running') next.querySelector('details').open = open;
          next.classList.add('settled');
          old.replaceWith(next);
          scroll();
        }
        break;
      }
      case 'skills':
        skills = m.skills;
        if (slashOpen) renderSlash();
        break;
      case 'files':
        if (m.query === fileQuery) { fileItems = m.items; if (slashOpen && menuKind === 'file') renderFiles(m.query); }
        break;
      case 'editor': renderContext(m); break;
      case 'index': state.index = m.index; renderIndex(); break;
      case 'contextUsage': renderMeter(m.used, m.window); break;
      case 'queue': renderQueue(m.items, m.steering); break;
      case 'tasks': renderTasks(m); break;
      case 'view': showView(m.view); break;
      case 'copySelection': {
        const text = getSelection()?.toString();
        if (text) copyText(text);
        break;
      }
      case 'approvalDone': {
        const card = log.querySelector(`.approval[data-call-id="${CSS.escape(m.callId)}"]`);
        if (card) {
          const label = { allow: 'Allowed', allowSession: 'Allowed for this session', always: 'Always allowed', deny: 'Denied' }[m.decision] || m.decision;
          card.querySelector('.actions').replaceChildren(icon(m.decision === 'deny' ? 'circle-slash' : 'check'), el('span', null, label));
          card.classList.add('resolved');
        }
        break;
      }
    }
  });

  renderToolbar();
  showView('chat');
  vscode.postMessage({ type: 'ready' });
  input.focus();
})();
