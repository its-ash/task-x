// task-x chat webview. Text is always inserted with textContent; model output is never parsed as HTML.
(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const log = $('log'), input = $('input'), send = $('send'), progress = $('progress');
  const modelBtn = $('modelBtn'), settingsBtn = $('settingsBtn');
  const nightPlanBtn = $('nightPlanBtn'), nightLoopBtn = $('nightLoopBtn');
  const refineBtn = $('refineBtn');

  const PLACEHOLDER = 'Tell task-x what to change';
  const ROLE_ICONS = { research: 'search', implementation: 'code', testing: 'beaker', review: 'eye', documentation: 'book' };
  const TOOL_LABELS = {
    read_file: 'Read', read_range: 'Read', read_multiple_files: 'Read files', read_symbol: 'Read symbol', get_file_outline: 'Outlined',
    list_files: 'Listed', find_files: 'Found files', get_file_tree: 'Listed tree', get_file_metadata: 'Inspected', detect_language: 'Inspected',
    get_project_structure: 'Inspected project', search_text: 'Searched', search_regex: 'Searched', find_all: 'Searched', find_symbol: 'Found symbol',
    find_definition: 'Found definition', find_references: 'Found references', search_files_by_content: 'Searched', replace_text: 'Edited',
    replace_all: 'Edited', insert_at_line: 'Edited', delete_range: 'Edited', apply_patch: 'Patched', create_file: 'Created', rename_file: 'Renamed',
    delete_file: 'Deleted', git_status: 'Git status', git_diff: 'Git diff', git_log: 'Git log', git_show: 'Git show', get_changed_files: 'Changed files',
    get_file_history: 'File history', format_code: 'Formatted', run_linter: 'Linted', run_tests: 'Ran tests', check_build: 'Built',
    get_diagnostics: 'Checked problems', run_command: 'Ran', read_skill: 'Loaded skill', web_fetch: 'Fetched',
    multi_search: 'Searched', read_log: 'Read log', remember: 'Remembered', find_tool: 'Loaded tools', summarize_file: 'Summarized',
    Read: 'Read', Write: 'Wrote', Edit: 'Edited', MultiEdit: 'Edited', NotebookEdit: 'Edited notebook', Bash: 'Ran', BashOutput: 'Read output',
    KillShell: 'Stopped shell', Grep: 'Searched', Glob: 'Found files', LS: 'Listed', TodoWrite: 'Updated todos', WebFetch: 'Fetched',
    WebSearch: 'Searched web', Task: 'Subagent', ExitPlanMode: 'Finished plan', run_subagent: 'Subagent', worktree_apply: 'Applied worktree', worktree_discard: 'Discarded worktree',
  };

  // A successful edit tool shows as its edit card, not as a row (a failed edit still shows as a row).
  const EDIT_TOOLS = new Set(['replace_text', 'replace_all', 'insert_at_line', 'delete_range', 'apply_patch', 'create_file', 'rename_file', 'delete_file', 'Write', 'Edit', 'MultiEdit', 'NotebookEdit']);
  let replaying = false; // true while the saved history is drawn: no enter animations
  const enter = (e, cls = 'enter') => { if (!replaying) e.classList.add(cls); return e; };
  let state = { busy: false, autoCommit: false, modelGroups: [], active: null };
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
      // The model picker has more than one area (search, providers, models), so Tab moves between them.
      else if (e.key === 'Tab' && !menu.classList.contains('model-picker')) menu.hidePopover();
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

  // ---------- Model picker: providers on the left, their models on the right ----------
  // Models picked before, listed first on the "All models" page.
  const MAX_RECENT = 5;
  const CAPS = { tools: ['tools', 'Tools'], vision: ['eye', 'Vision'], thinking: ['lightbulb', 'Thinking'], embedding: ['symbol-array', 'Embedding'] };
  const recentModels = () => vscode.getState()?.recent ?? [];
  function rememberModel(kind, model) {
    const recent = [{ kind, model }, ...recentModels().filter((r) => r.kind !== kind || r.model !== model)].slice(0, MAX_RECENT);
    vscode.setState({ ...(vscode.getState() || {}), recent });
  }
  // Every word of the query must appear in the model id, its provider, its size, or its capabilities.
  const hits = (query, m, g) => {
    const hay = `${m.id} ${g.label} ${m.details} ${m.caps.join(' ')}`.toLowerCase();
    return query.split(/\s+/).filter(Boolean).every((w) => hay.includes(w));
  };
  // The picker is refilled when the model lists, the query, or the provider page change.
  let modelMenu = null;
  // The model menu sets the chat model, a Loop Mode model ('planner' Think, 'worker' Loop), or a tool
  // model from the settings dialog ('refine', 'helper'). Loop, Refine, and Helper take provider models only.
  const menuTarget = () => modelMenu?.target ?? 'main';
  const TARGET_MODEL = { planner: 'nightPlanner', worker: 'nightWorker', refine: 'refineModel', helper: 'helperModel' };
  const targetModel = (t) => (TARGET_MODEL[t] ? state[TARGET_MODEL[t]] : state.active);
  const providerOnly = (t) => t === 'worker' || t === 'refine' || t === 'helper';
  const menuGroups = () => (providerOnly(menuTarget()) ? state.modelGroups.filter((g) => g.kind !== 'claudeCode') : state.modelGroups);
  // The same model: an Ollama name without a tag means `:latest`.
  const sameModel = (a, b) => !!a && !!b && (a === b || a === `${b}:latest` || b === `${a}:latest`);
  const isCurrent = (current, g, m) => current?.kind === g.kind && sameModel(current?.model, m.id);
  const groupIcon = (g) => (g.kind === 'claudeCode' ? 'sparkle' : g.local ? 'vm' : 'cloud');
  // One row per model: id, context size, capability icons, and size or provider below.
  function modelRow(list, g, m, { showProvider, checked, onSelect }) {
    const b = el('button', 'menu-item model-item');
    b.type = 'button';
    b.setAttribute('role', 'menuitemradio');
    b.setAttribute('aria-checked', String(!!checked));
    const meta = el('span', 'model-meta');
    if (m.ctx) meta.append(el('span', 'model-ctx', m.ctx));
    for (const c of m.caps) {
      const [ic, label] = CAPS[c] ?? ['circle-small', c];
      const i = icon(ic, 'model-cap');
      i.title = label;
      meta.append(i);
    }
    // The name gets the whole first line; size, context, and capabilities go on the line below it.
    const sub = [showProvider ? g.label : '', m.details].filter(Boolean).join(' · ');
    if (sub) meta.append(el('span', 'model-sub', sub));
    b.append(icon('check', 'check'), el('span', 'title', m.id));
    if (meta.childNodes.length) b.append(meta);
    b.title = [m.id, m.ctx && `${m.ctx} context`, m.caps.map((c) => CAPS[c]?.[1] ?? c).join(', '), m.details].filter(Boolean).join(' · ');
    b.addEventListener('click', () => { b.closest('.menu')?.hidePopover(); onSelect(); });
    list.append(b);
  }
  // Left side: All models, then one page per provider with its model count and status.
  function renderModelSide() {
    const { side, menu } = modelMenu;
    const groups = menuGroups();
    if (modelMenu.tab !== 'all' && !groups.some((g) => g.kind === modelMenu.tab)) modelMenu.tab = 'all';
    const page = (kind, label, ic, count, status) => {
      const t = button('mp-prov', [icon(ic), el('span', 'mp-name', label)], () => { modelMenu.tab = kind; fillModels(); modelMenu.input.focus(); }, label);
      t.setAttribute('role', 'tab');
      t.setAttribute('aria-selected', String(kind === modelMenu.tab));
      t.append(status === 'loading' ? icon('loading', 'codicon-modifier-spin mp-status') : status === 'error' ? icon('warning', 'mp-status warn') : el('span', 'mp-count', count));
      return t;
    };
    const total = groups.reduce((n, g) => n + g.models.length, 0);
    const add = button('mp-prov mp-add', [icon('add'), el('span', 'mp-name', 'Add provider…')], () => { menu.hidePopover(); vscode.postMessage({ type: 'addProvider' }); }, 'Add a provider');
    side.replaceChildren(
      page('all', 'All models', 'list-flat', String(total)),
      ...groups.map((g) => page(g.kind, g.label, groupIcon(g), String(g.models.length), g.loading ? 'loading' : g.error ? 'error' : '')),
      add,
    );
  }
  function fillModels() {
    if (!modelMenu) return;
    const { list, input: q } = modelMenu;
    renderModelSide();
    const query = q.value.trim().toLowerCase();
    const focused = document.activeElement === q;
    list.replaceChildren();
    const target = menuTarget();
    const current = targetModel(target);
    const pick = (kind, model) => {
      rememberModel(kind, model);
      vscode.postMessage(target === 'main' ? { type: 'selectModel', kind, model } : { type: 'selectNightModel', role: target, kind, model });
    };
    const all = modelMenu.tab === 'all';
    const groups = all ? menuGroups() : menuGroups().filter((g) => g.kind === modelMenu.tab);
    let shown = 0;
    // Refine and Helper can go back to their default.
    if ((target === 'refine' || target === 'helper') && all && !query) {
      menuItem(list, {
        icon: 'discard', title: target === 'refine' ? 'Routed or chat model (default)' : 'Chat model (default)',
        desc: target === 'refine' ? 'Refine with the task-x.routing model for the prompt, else the chat model.' : 'Summaries and research subagents use the chat model.',
        checked: !current, onSelect: () => vscode.postMessage({ type: 'selectNightModel', role: target, kind: '', model: '' }),
      });
    }
    if (all && !query) {
      const recent = recentModels()
        .map((r) => { const g = groups.find((x) => x.kind === r.kind); const m = g?.models.find((x) => x.id === r.model); return g && m ? { g, m } : null; })
        .filter(Boolean);
      if (recent.length) menuHeader(list, 'Recent');
      for (const { g, m } of recent) modelRow(list, g, m, { showProvider: true, checked: isCurrent(current, g, m), onSelect: () => pick(g.kind, m.id) });
    }
    for (const g of groups) {
      const models = g.models.filter((m) => !query || hits(query, m, g));
      if (query && !models.length) continue;
      const head = el('div', 'menu-header mp-head');
      head.append(icon(groupIcon(g)), el('span', null, g.label));
      if (g.loading) head.append(icon('loading', 'codicon-modifier-spin'));
      list.append(head);
      if (g.error) list.append(el('div', 'menu-note error', g.error));
      if (g.note) list.append(el('div', 'menu-note', g.note));
      if (!g.models.length) list.append(el('div', 'menu-empty', g.loading ? 'Loading models…' : 'No models found.'));
      for (const m of models) {
        modelRow(list, g, m, { showProvider: false, checked: isCurrent(current, g, m), onSelect: () => pick(g.kind, m.id) });
        shown++;
      }
    }
    if (query && !shown) list.append(el('div', 'menu-empty', 'No models match.'));
    // A model id the list does not have can still be set, for the open provider page or the current provider.
    const raw = q.value.trim();
    const base = all ? (current ?? state.active) : { kind: modelMenu.tab };
    if (raw && base && !(providerOnly(target) && base.kind === 'claudeCode') && !menuGroups().some((g) => g.kind === base.kind && g.models.some((m) => sameModel(m.id, raw)))) {
      menuItem(list, { icon: 'add', title: `Use "${raw}"`, desc: `Set this model id for ${menuGroups().find((g) => g.kind === base.kind)?.label ?? 'the current provider'}.`, onSelect: () => pick(base.kind, raw) });
    }
    if (focused) q.focus();
    modelMenu.menu.place?.();
  }
  function openModelMenu(anchor, target) {
    vscode.postMessage({ type: 'loadModels' });
    openMenu(anchor, (menu) => {
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
        list.querySelector('.menu-item')?.click();
      });
      const refresh = button('action', icon('refresh'), () => vscode.postMessage({ type: 'loadModels', force: true }), 'Load the model lists again');
      box.append(icon('search'), q, refresh);
      const side = el('div', 'mp-side');
      side.setAttribute('role', 'tablist');
      side.setAttribute('aria-label', 'Providers');
      side.setAttribute('aria-orientation', 'vertical');
      const list = el('div', 'menu-list mp-list');
      const body = el('div', 'mp-body');
      body.append(side, list);
      menu.classList.add('searchable', 'model-picker');
      menu.addEventListener('toggle', (e) => { if (e.newState === 'closed' && modelMenu?.menu === menu) modelMenu = null; });
      menu.append(box, body);
      modelMenu = { menu, list, side, input: q, target, tab: targetModel(target)?.kind ?? 'all' };
      fillModels();
    });
  }
  modelBtn.addEventListener('click', () => openModelMenu(modelBtn, 'main'));
  nightPlanBtn.addEventListener('click', () => openModelMenu(nightPlanBtn, 'planner'));
  nightLoopBtn.addEventListener('click', () => openModelMenu(nightLoopBtn, 'worker'));
  // ---------- Settings dialog: edit mode, Loop Mode, and auto-commit ----------
  const EDIT_MODES = {
    auto: ['rocket', 'Auto', 'Edits, deletes, commands, and tools run without asking. Commands stay in the sandbox.'],
    safe: ['shield', 'Safe', 'Free inside the project. Asks before anything that can write or delete beyond it: installs, git writes, network, outside files, MCP tools, and commands without the sandbox.'],
    ask: ['comment-discussion', 'Ask', 'Asks before every edit, delete, and command.'],
  };
  let settingsMenu = null;
  function toggleRow(menu, { key, ic, title, desc, on, disabled, onToggle }) {
    const b = el('button', 'menu-item setting-row');
    b.type = 'button';
    b.dataset.key = key;
    b.setAttribute('role', 'menuitemcheckbox');
    b.setAttribute('aria-checked', String(!!on));
    b.disabled = !!disabled;
    const text = el('span', 'setting-text');
    text.append(el('span', 'title', title), el('span', 'desc', desc));
    const sw = el('span', 'setting-switch');
    sw.setAttribute('aria-hidden', 'true');
    b.append(icon(ic, 'setting-icon'), text, sw);
    b.addEventListener('click', () => onToggle(!on));
    menu.append(b);
  }
  // A setting that is a model: opens the model picker for that role.
  function modelRowSetting(menu, target, ic, title, desc, fallback) {
    const picked = targetModel(target);
    const b = el('button', 'menu-item setting-row model-setting');
    b.type = 'button';
    b.dataset.key = `model-${target}`;
    b.setAttribute('aria-haspopup', 'menu');
    const text = el('span', 'setting-text');
    text.append(el('span', 'title', title), el('span', 'desc', desc));
    const value = el('span', `setting-value${picked ? '' : ' unset'}`, picked?.model || fallback);
    value.title = picked ? `${picked.model} (${picked.kind})` : `Not set: ${fallback}`;
    b.append(icon(ic, 'setting-icon'), text, value);
    b.addEventListener('click', () => openModelMenu(settingsBtn, target));
    menu.append(b);
  }
  function fillSettings() {
    if (!settingsMenu) return;
    const menu = settingsMenu;
    const focus = document.activeElement?.dataset.key;
    menu.replaceChildren();
    const head = el('div', 'settings-head');
    head.append(icon('settings-gear'), el('span', null, 'Chat settings'));
    menu.append(head);
    menuHeader(menu, 'Edit mode');
    const seg = el('div', 'seg');
    seg.setAttribute('role', 'radiogroup');
    seg.setAttribute('aria-label', 'Edit mode');
    const cur = EDIT_MODES[state.editMode] ? state.editMode : 'auto';
    for (const [key, [ic, label]] of Object.entries(EDIT_MODES)) {
      const b = el('button', `menu-item seg-btn ${key}`);
      b.type = 'button';
      b.dataset.key = `mode-${key}`;
      b.setAttribute('role', 'menuitemradio');
      b.setAttribute('aria-checked', String(key === cur));
      b.append(icon(ic), el('span', null, label));
      b.addEventListener('click', () => {
        state.editMode = key;
        vscode.postMessage({ type: 'selectEditMode', mode: key });
        renderToolbar();
        fillSettings();
      });
      seg.append(b);
    }
    menu.append(seg, el('div', `settings-desc ${cur}`, EDIT_MODES[cur][2]));
    menuHeader(menu, 'Models');
    modelRowSetting(menu, 'refine', 'sparkle', 'Refine model', 'Rewrites your prompt with the sparkle button.', 'routed or chat model');
    modelRowSetting(menu, 'helper', 'zap', 'Loop model', 'A small, cheap model for file summaries and research subagents.', 'chat model');
    menuHeader(menu, 'Options');
    toggleRow(menu, {
      key: 'loop', ic: 'sync', title: 'Loop Mode', on: state.night,
      desc: 'A request that fails, stalls, or stops runs again until it finishes. The Think model splits it into todo.md; the Loop model works one item at a time.',
      onToggle: (v) => { state.night = v; vscode.postMessage({ type: 'selectNight', enabled: v }); renderToolbar(); fillSettings(); },
    });
    toggleRow(menu, {
      key: 'commit', ic: 'git-commit', title: 'Auto-commit', on: state.autoCommit || state.night, disabled: state.night,
      desc: state.night ? 'Always on in Loop Mode: each todo item is committed.' : 'Commit the files each task changes when the task ends.',
      onToggle: (v) => { state.autoCommit = v; vscode.postMessage({ type: 'selectAutoCommit', enabled: v }); fillSettings(); },
    });
    // Redrawing keeps focus on the control the user was on.
    if (focus) menu.querySelector(`[data-key="${CSS.escape(focus)}"]`)?.focus();
    menu.place?.();
  }
  settingsBtn.setAttribute('aria-haspopup', 'dialog');
  settingsBtn.addEventListener('click', () => {
    openMenu(settingsBtn, (menu) => {
      menu.classList.add('settings-menu');
      menu.setAttribute('role', 'dialog');
      menu.setAttribute('aria-label', 'Chat settings');
      menu.addEventListener('toggle', (e) => { if (e.newState === 'closed' && settingsMenu === menu) settingsMenu = null; });
      settingsMenu = menu;
      fillSettings();
    });
  });

  // ---------- Markdown (fenced code, inline code, bold, italic, highlights, headings, lists, callouts) ----------
  // Inline: `code`, **bold**, *italic*, ==highlight==, and (recommended) as a badge.
  const INLINE = /(`[^`\n]+`|\*\*[^*\n]+\*\*|==[^=\n]+==|\*\((?:recommended|suggested)\)\*|\((?:recommended|suggested)\)|(?<![\w*])\*[^*\s][^*\n]*\*(?![\w*])|(?<![\w_])_[^_\s][^_\n]*_(?![\w_]))/gi;
  function renderInline(parent, text, opts = {}) {
    // In a question card, the opening question of a paragraph or item stands out.
    const q = opts.ask ? text.match(/^[^.!?\n]{3,240}\?(?=\s|$)/) : null;
    if (q) {
      const s = el('span', 'md-ask');
      renderInline(s, q[0]);
      parent.append(s);
      text = text.slice(q[0].length);
    }
    let last = 0;
    for (const m of text.matchAll(INLINE)) {
      if (m.index > last) parent.append(text.slice(last, m.index));
      const t = m[0];
      if (t.startsWith('`')) parent.append(el('code', null, t.slice(1, -1)));
      else if (/^\*?\((?:recommended|suggested)\)\*?$/i.test(t)) parent.append(el('span', 'md-badge', t.replace(/[*()]/g, '').replace(/^./, (c) => c.toUpperCase())));
      else if (t.startsWith('**')) parent.append(el('strong', null, t.slice(2, -2)));
      else if (t.startsWith('==')) parent.append(el('mark', 'md-mark', t.slice(2, -2)));
      else parent.append(el('em', null, t.slice(1, -1)));
      last = m.index + t.length;
    }
    if (last < text.length) parent.append(text.slice(last));
  }
  // Callout cards: GitHub alerts (`> [!WARNING]`), and sections whose heading asks the user for
  // something or flags a risk. Each kind has its own colour and icon.
  const CALLOUTS = {
    note: ['info', 'Note'], tip: ['lightbulb', 'Tip'], important: ['star-full', 'Important'],
    warning: ['warning', 'Warning'], caution: ['error', 'Caution'], question: ['question', 'Your input needed'],
  };
  const CALLOUT_ALIAS = { decision: 'question', ask: 'question', confirm: 'question', danger: 'caution', risk: 'warning', info: 'note' };
  const ASK_HEADING = /\b(?:need(?:s|ed)? from you|to confirm|please (?:confirm|choose|decide)|your (?:call|decision|input)|decisions? (?:needed|for you)|questions? for you|open questions|action (?:required|needed)|next steps? for you)\b/i;
  const WARN_HEADING = /\b(?:constraint|caveat|warning|risks?|heads[- ]up|security|breaking|gotcha|watch out|flag(?:ging)?)\b/i;
  function calloutKind(raw) {
    const k = raw.toLowerCase();
    return CALLOUTS[k] ? k : CALLOUT_ALIAS[k] || 'note';
  }
  function callout(kind, title) {
    const card = el('div', `md-callout ${kind}`);
    const head = el('div', 'md-callout-head');
    head.append(icon(CALLOUTS[kind][0]));
    const t = el('span', 'md-callout-title');
    renderInline(t, title || CALLOUTS[kind][1]);
    head.append(t);
    const body = el('div', 'md-callout-body');
    card.append(head, body);
    return { card, body };
  }
  function renderMarkdown(container, text) {
    container.replaceChildren();
    const lines = text.split('\n');
    let list = null, para = [];
    // Where blocks go: the message, or the card of the section being read.
    let out = container, ask = false;
    const flush = () => { if (para.length) { const p = el('p'); renderInline(p, para.join('\n'), { ask }); out.append(p); para = []; } };
    for (let i = 0; i < lines.length; i++) {
      const line = lines[i];
      const quote = line.match(/^\s*>\s?(.*)$/);
      if (quote) {
        flush(); list = null;
        const body = [quote[1]];
        for (; i + 1 < lines.length && /^\s*>/.test(lines[i + 1]); i++) body.push(lines[i + 1].replace(/^\s*>\s?/, ''));
        const alert = body[0].match(/^\[!(\w+)\][ \t]*(.*)$/);
        if (alert) {
          const kind = calloutKind(alert[1]);
          const { card, body: inner } = callout(kind, alert[2].trim());
          renderMarkdown(inner, body.slice(1).join('\n'));
          if (kind === 'question') inner.querySelectorAll(':scope > p, li').forEach((n) => markAsk(n));
          out.append(card);
        } else {
          const bq = el('blockquote', 'md-quote');
          renderMarkdown(bq, body.join('\n'));
          out.append(bq);
        }
        continue;
      }
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
        out.append(wrap);
        continue;
      }
      // A table: a row of cells, then a line of dashes (`|---|:--:|`) that also sets each column's alignment.
      if (line.includes('|') && i + 1 < lines.length && isTableRule(lines[i + 1])) {
        flush(); list = null;
        const align = cells(lines[i + 1]).map((c) => (/^:-+:$/.test(c) ? 'center' : /-:$/.test(c) ? 'right' : /^:/.test(c) ? 'left' : ''));
        const wrap = el('div', 'md-table-wrap');
        const table = el('table', 'md-table');
        const row = (text, tag) => {
          const tr = el('tr');
          cells(text).forEach((c, k) => {
            const td = el(tag);
            if (align[k]) td.style.textAlign = align[k];
            renderInline(td, c);
            tr.append(td);
          });
          return tr;
        };
        const thead = el('thead');
        thead.append(row(line, 'th'));
        const tbody = el('tbody');
        for (i += 2; i < lines.length && lines[i].includes('|') && lines[i].trim(); i++) tbody.append(row(lines[i], 'td'));
        i--;
        table.append(thead, tbody);
        wrap.append(table);
        out.append(wrap);
        continue;
      }
      // A heading, or a line that is only bold text, starts a section; a section that asks the
      // user something or flags a risk becomes a card until the next heading.
      const h = line.match(/^(#{1,4})\s+(.*)$/) || line.match(/^\s*()\*\*([^*\n]{2,80})\*\*:?\s*$/);
      const li = line.match(/^\s*(?:[-*]|\d+\.)\s+(.*)$/);
      if (h) {
        flush(); list = null; out = container; ask = false;
        const title = h[2].trim().replace(/:$/, '');
        const kind = ASK_HEADING.test(title) ? 'question' : WARN_HEADING.test(title) ? 'warning' : null;
        if (kind) {
          const { card, body } = callout(kind, title);
          container.append(card);
          out = body;
          ask = kind === 'question';
        } else {
          const e = el(h[1] ? 'h' + Math.min(6, h[1].length + 2) : 'h4');
          renderInline(e, title);
          container.append(e);
        }
      } else if (li) {
        flush();
        if (!list) { list = el(/^\s*\d+\./.test(line) ? 'ol' : 'ul'); out.append(list); }
        const e = el('li');
        renderInline(e, li[1], { ask });
        if (/\((?:recommended|suggested)\)/i.test(li[1])) e.classList.add('md-recommended');
        list.append(e);
      } else if (!line.trim()) { flush(); list = null; }
      else { list = null; para.push(line); }
    }
    flush();
  }
  // The cells of a table row: split at | outside `code`, without the outer pipes. \| is a literal pipe.
  function cells(line) {
    const out = [];
    let cur = '', code = false;
    const s = line.trim().replace(/^\|/, '').replace(/(?<!\\)\|$/, '');
    for (let k = 0; k < s.length; k++) {
      const c = s[k];
      if (c === '\\' && s[k + 1] === '|') { cur += '|'; k++; continue; }
      if (c === '`') code = !code;
      if (c === '|' && !code) { out.push(cur.trim()); cur = ''; continue; }
      cur += c;
    }
    out.push(cur.trim());
    return out;
  }
  const isTableRule = (line) => line.includes('|') && /^\s*\|?\s*:?-+:?\s*(\|\s*:?-+:?\s*)*\|?\s*$/.test(line);
  // Marks the opening question of a rendered paragraph or item (alert cards render their body first).
  function markAsk(node) {
    const first = node.firstChild;
    if (first?.nodeType !== Node.TEXT_NODE) return;
    const q = first.textContent.match(/^[^.!?\n]{3,240}\?(?=\s|$)/);
    if (!q) return;
    const s = el('span', 'md-ask', q[0]);
    first.textContent = first.textContent.slice(q[0].length);
    node.insertBefore(s, first);
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
  // Empty chat: one short line, picked at random each time an empty chat is drawn.
  const WELCOME = [
    'Tokens warm. Context loaded. What are we building?',
    'I read the repo so you do not have to. Ask away.',
    'Weights loaded. Coffee optional. Bugs beware.',
    'Give me a goal; I will bring the diffs.',
    'Every refactor starts with one prompt.',
    'Neurons idle. Let us make something ship.',
    'Small steps, verified. Where do we start?',
    'Undo is one click away. Be bold.',
    'I do not sleep. Loop Mode says retry.',
    'Describe the change. I will find the files.',
    'Your code, your model, my tools. Ready.',
    'Hello, human. The tests are waiting.',
  ];
  let welcomeLine = '';
  function welcome() {
    if (log.children.length) return;
    const others = WELCOME.filter((l) => l !== welcomeLine);
    welcomeLine = others[Math.floor(Math.random() * others.length)];
    const w = el('div', 'welcome');
    w.append(icon('sparkle', 'welcome-icon'), el('p', 'welcome-verse', welcomeLine));
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
  // The dot of a running call is its Stop control: a stop button sits inside the dot, so the state
  // colour and the rail stay as they are. `toolDone` removes the button and leaves the dot behind.
  function stopDot(row) {
    const dot = row.querySelector(':scope > .dot');
    if (!dot) return;
    const id = row.dataset.id;
    const btn = button('tool-stop dot-stop', icon('debug-stop'), () => vscode.postMessage({ type: 'stopTool', id }), 'Stop this command. The agent goes on.');
    dot.append(btn);
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
  // Auto-commit off: one card per turn lists the files written, with line counts that update on each edit.
  let turnFiles = null; // { body, files: Map<path, { added, removed }> }
  function filesCard(it) {
    if (!turnFiles) {
      const card = el('div', 'files-card');
      const body = el('div', 'files-body');
      card.append(body);
      step(card);
      turnFiles = { body, files: new Map() };
    }
    const { body, files } = turnFiles;
    const f = files.get(it.path) ?? { added: 0, removed: 0, edits: 0, undone: 0, kept: 0 };
    f.added += it.added ?? 0;
    f.removed += it.removed ?? 0;
    f.edits++;
    if (it.undone) f.undone++;
    if (it.kept) f.kept++;
    files.set(it.path, f);
    const total = [...files.values()].reduce((s, v) => ({ added: s.added + v.added, removed: s.removed + v.removed }), { added: 0, removed: 0 });
    const head = el('div', 'files-head');
    const totalStat = el('span', 'stat');
    totalStat.append(el('span', 'add', `+${total.added}`), el('span', 'del', `-${total.removed}`));
    head.append(icon('files'), el('span', 'files-title', `${files.size} file${files.size === 1 ? '' : 's'} written`), el('span', 'spacer'), totalStat);
    const list = el('ul', 'files-list');
    for (const [path, v] of files) {
      const slash = path.lastIndexOf('/');
      const name = [el('span', 'edit-name', path.slice(slash + 1))];
      if (slash >= 0) name.push(el('span', 'edit-dir', path.slice(0, slash)));
      const open = button('file-btn', name, () => vscode.postMessage({ type: 'diffFile', path }), `Show changes in ${path}`);
      const stat = el('span', 'stat');
      stat.append(el('span', 'add', `+${v.added}`), el('span', 'del', `-${v.removed}`));
      // Each file has its own Undo: it reverts the agent's changes to this file only.
      const undone = v.undone === v.edits, kept = v.kept + v.undone === v.edits && v.kept > 0;
      const row = el('li', undone ? 'undone' : kept ? 'kept' : '');
      row.append(open, stat);
      if (undone || kept) row.append(el('span', 'file-state', undone ? 'Undone' : 'Kept'));
      else row.append(button('action file-undo', icon('discard'), (e) => { e.currentTarget.disabled = true; vscode.postMessage({ type: 'revert', paths: [path] }); }, `Undo the changes to ${path} only`));
      list.append(row);
    }
    body.replaceChildren(head, list);
    scroll();
  }
  function renderItem(it) {
    log.querySelector('.welcome')?.remove();
    switch (it.kind) {
      case 'user': {
        turn = { el: el('div', 'turn'), response: null };
        turnFiles = null;
        const req = enter(el('div', 'request'));
        // Your message keeps its line breaks and shows `code`, ``` blocks, and tables like a reply.
        const body = el('div', 'request-text md');
        renderMarkdown(body, it.text);
        req.append(body, button('action copy-msg', icon('copy'), () => copyText(it.text), 'Copy message'));
        if (it.images) {
          const n = el('div', 'request-images');
          n.append(icon('file-media'), document.createTextNode(`${it.images} image${it.images > 1 ? 's' : ''} attached`));
          req.append(n);
        }
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
        // The dot itself is the stop control while the call runs; `toolDone` clears it.
        if (it.stop && it.ok === undefined && state.busy) stopDot(row);
        attachOutput(row, it.id, it.tool, it.ok !== false, it.output, it.outputCut, it.ok === undefined && state.busy);
        tokenChip(row, it.tokens);
        current = null;
        break;
      }
      case 'file':
        if (state.autoCommit || state.night) step(editCard(it));
        else filesCard(it);
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
      case 'questions': {
        response().append(questionCard(it.questions, it.answers, null, it.other));
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
    if (it.status === 'running') head.append(button('action stop-btn', icon('debug-stop'), () => vscode.postMessage({ type: 'stopLane', taskId: it.taskId }), 'Stop this subagent'));
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
  // About how many tokens a tool result adds to the conversation (it is sent again on every later turn).
  function tokenChip(row, tokens) {
    row.querySelector(':scope > .tok')?.remove();
    if (!tokens) return;
    const chip = el('span', `tok${tokens >= 4000 ? ' big' : ''}`, `${fmtCount(tokens)} tok`);
    chip.title = `This result adds about ${tokens.toLocaleString()} tokens to the conversation. Each later turn sends it again until the history is compressed.`;
    const after = row.querySelector(':scope > .detail');
    after ? after.after(chip) : row.append(chip);
  }
  // Shell commands always get a block: "Running…" until the result comes, then the output (or a note that it printed nothing).
  function attachOutput(row, id, tool, ok, output, cut, running = false) {
    const shell = EXPANDED.has(tool.split(' › ').pop());
    const text = output || (shell ? (running ? 'Running…' : '(no output)') : '');
    if (!text) return;
    const prev = row.nextElementSibling?.classList.contains('term') ? row.nextElementSibling : null;
    if (prev) {
      prev.querySelector('.term-body').textContent = text;
      prev.classList.toggle('running', running);
      if (cut && !prev.querySelector('.term-note')) prev.append(el('div', 'term-note', 'Showing the start of the output. Open in editor to see all of it.'));
      if (!ok) prev.hidden = false;
      return;
    }
    const box = outputBlock(id, text, cut, shell || !ok);
    box.classList.toggle('running', running);
    const toggle = button('action', icon(box.hidden ? 'chevron-right' : 'chevron-down'), () => {
      box.hidden = !box.hidden;
      toggle.replaceChildren(icon(box.hidden ? 'chevron-right' : 'chevron-down'));
      toggle.setAttribute('aria-expanded', String(!box.hidden));
    }, 'Show output');
    toggle.setAttribute('aria-expanded', String(!box.hidden));
    row.append(el('span', 'spacer'), toggle);
    row.after(box);
  }

  // Questions from the agent. A live card (callId set) sends the answers; a saved one only shows them.
  function questionCard(questions, answers, callId, other) {
    return callId ? liveQuestionCard(questions, callId) : savedQuestionCard(questions, answers, other);
  }
  function questionHead(text, extra) {
    const head = el('div', 'q-head');
    head.append(icon('comment-discussion'), el('span', 'q-title', text));
    if (extra) head.append(el('span', 'spacer'), extra);
    return head;
  }
  function savedQuestionCard(questions, answers, other) {
    const card = el('div', 'questions resolved');
    card.append(questionHead(questions.length > 1 ? 'Questions' : 'Question', answers ? null : el('span', 'q-badge', 'Skipped')));
    const list = el('ol', 'q-summary');
    for (const q of questions) {
      const li = el('li');
      li.append(el('div', 'q-text', q.text));
      const vals = [...(answers?.[q.id] ?? [])];
      const typed = other?.[q.id];
      const row = el('div', 'q-picked');
      for (const v of vals) row.append(el('span', 'q-chip', v));
      if (typed) row.append(el('span', 'q-chip typed', typed));
      if (!vals.length && !typed) row.append(el('span', 'q-none', answers ? 'No answer' : '—'));
      li.append(row);
      list.append(li);
    }
    card.append(list);
    return card;
  }
  function liveQuestionCard(questions, callId) {
    const card = el('div', 'questions');
    card.dataset.callId = callId;
    const multi = questions.length > 1;
    const picked = questions.map(() => new Set());
    const typed = questions.map(() => '');
    const answered = (qi) => picked[qi].size > 0 || typed[qi].trim() !== '';
    const valid = (qi) => questions[qi].kind !== 'radio' || answered(qi);
    let at = 0;
    const step = el('span', 'q-step');
    card.append(questionHead(multi ? 'Questions' : 'Question', multi ? step : null));

    const tabs = el('div', 'q-tabs');
    tabs.setAttribute('role', 'tablist');
    const panels = el('div', 'q-panels');
    const tabEls = [];
    const panelEls = [];
    questions.forEach((q, qi) => {
      const id = `${callId}-${qi}`;
      const tab = button('q-tab', [el('span', 'q-num', String(qi + 1)), el('span', 'q-label', q.text)], () => go(qi));
      tab.id = `qt-${id}`;
      tab.title = q.text;
      tab.setAttribute('role', 'tab');
      tab.setAttribute('aria-controls', `qp-${id}`);
      tabEls.push(tab);
      tabs.append(tab);

      const panel = el('div', 'q-panel');
      panel.id = `qp-${id}`;
      panel.setAttribute('role', 'tabpanel');
      panel.setAttribute('aria-labelledby', `qt-${id}`);
      const fs = el('fieldset', 'question');
      const legend = el('legend', null, q.text);
      fs.append(legend, el('div', 'q-hint', q.kind === 'radio' ? 'Pick one, or type your own answer.' : 'Pick any that apply, and add your own if needed.'));
      const opts = el('div', 'q-options');
      q.options.forEach((opt, oi) => {
        const label = el('label', 'option');
        const input = el('input');
        input.type = q.kind === 'radio' ? 'radio' : 'checkbox';
        input.name = id;
        input.value = opt;
        input.addEventListener('change', () => {
          if (q.kind === 'radio') picked[qi].clear();
          input.checked ? picked[qi].add(opt) : picked[qi].delete(opt);
          sync();
          if (q.kind === 'radio' && input.checked && multi && qi < questions.length - 1 && !typed[qi].trim()) setTimeout(() => go(qi + 1), 180);
        });
        label.append(input, el('span', 'opt-text', opt), el('kbd', 'opt-key', String(oi + 1)));
        opts.append(label);
      });
      const area = el('textarea', 'q-other');
      area.rows = 1;
      area.maxLength = 1000;
      area.placeholder = q.kind === 'radio' ? 'Other answer…' : 'Add details or another answer…';
      area.setAttribute('aria-label', `Your own answer to: ${q.text}`);
      area.addEventListener('input', () => {
        typed[qi] = area.value;
        area.style.height = 'auto';
        area.style.height = `${Math.min(area.scrollHeight, 160)}px`;
        if (q.kind === 'radio' && area.value.trim() && picked[qi].size) {
          picked[qi].clear();
          for (const i of opts.querySelectorAll('input')) i.checked = false;
        }
        sync();
      });
      fs.append(opts, area);
      panel.append(fs);
      panel.addEventListener('keydown', (e) => {
        if (e.target === area || e.altKey || e.ctrlKey || e.metaKey) return;
        const n = Number(e.key);
        const box = opts.querySelectorAll('input')[n - 1];
        if (!box) return;
        e.preventDefault();
        box.checked = q.kind === 'radio' ? true : !box.checked;
        box.dispatchEvent(new Event('change'));
        box.focus();
      });
      panelEls.push(panel);
      panels.append(panel);
    });
    tabs.addEventListener('keydown', (e) => {
      const d = { ArrowRight: 1, ArrowLeft: -1 }[e.key];
      if (!d) return;
      e.preventDefault();
      go((at + d + questions.length) % questions.length);
      tabEls[at].focus();
    });
    if (multi) card.append(tabs);
    card.append(panels);

    const back = button('btn secondary', [icon('arrow-left'), 'Back'], () => go(at - 1));
    const next = button('btn secondary', ['Next', icon('arrow-right')], () => go(at + 1));
    const send = button('btn', [icon('send'), 'Send answers'], submit);
    const skip = button('btn text', 'Skip', () => finish(null));
    const note = el('span', 'q-note');
    const actions = el('div', 'actions');
    actions.append(skip, el('span', 'spacer'), note);
    if (multi) actions.append(back, next);
    actions.append(send);
    card.append(actions);

    card.addEventListener('keydown', (e) => {
      if (e.key === 'Enter' && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        questions.every((_, qi) => valid(qi)) ? submit() : go(questions.findIndex((_, qi) => !valid(qi)));
      }
    });

    function go(qi) {
      at = Math.max(0, Math.min(questions.length - 1, qi));
      tabEls.forEach((t, i) => {
        const on = i === at;
        t.classList.toggle('active', on);
        t.setAttribute('aria-selected', String(on));
        t.tabIndex = on ? 0 : -1;
      });
      panelEls.forEach((p, i) => { p.hidden = i !== at; });
      sync();
      panelEls[at].querySelector('input')?.focus({ preventScroll: true });
    }
    function sync() {
      tabEls.forEach((t, i) => t.classList.toggle('done', answered(i)));
      const left = questions.filter((_, qi) => !valid(qi)).length;
      step.textContent = `${at + 1} of ${questions.length}`;
      back.disabled = at === 0;
      next.disabled = at === questions.length - 1;
      send.disabled = left > 0;
      note.textContent = left ? `${left} left to answer` : '';
    }
    function submit() {
      if (questions.some((_, qi) => !valid(qi))) return;
      const out = {};
      const extra = {};
      questions.forEach((q, qi) => {
        out[q.id] = [...picked[qi]];
        if (typed[qi].trim()) extra[q.id] = typed[qi].trim();
      });
      finish(out, extra);
    }
    function finish(out, extra) {
      vscode.postMessage({ type: 'answerQuestions', callId, answers: out, other: extra });
      card.classList.add('sending');
      for (const c of card.querySelectorAll('button, input, textarea')) c.disabled = true;
    }
    go(0);
    return card;
  }
  function renderQuestions(m) {
    log.querySelector('.welcome')?.remove();
    current = null;
    const card = enter(questionCard(m.questions, null, m.callId));
    response().append(card);
    scroll(true);
    card.querySelector('.q-panel:not([hidden]) input')?.focus();
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
  // Files appear here as the agent edits them; a file new to the list slides in.
  let shownChanges = new Set();
  function renderChanges(files) {
    const seen = shownChanges;
    shownChanges = new Set(files.map((f) => f.path));
    changes.hidden = !files.length;
    if (!files.length) return void changesList.replaceChildren();
    const added = files.reduce((n, f) => n + f.added, 0), removed = files.reduce((n, f) => n + f.removed, 0);
    $('changesTitle').textContent = `${files.length} file${files.length > 1 ? 's' : ''} changed`;
    $('changesStat').replaceChildren(el('span', 'add', `+${added}`), el('span', 'del', `-${removed}`));
    changesList.replaceChildren(...files.map((f) => {
      const li = el('li', `change ${f.status}${seen.has(f.path) ? '' : ' new'}`);
      const slash = f.path.lastIndexOf('/');
      const open = button('file-btn', [], () => vscode.postMessage({ type: 'diffFile', path: f.path }), `Show changes in ${f.path}`);
      open.append(icon('file'), el('span', 'fname', slash >= 0 ? f.path.slice(slash + 1) : f.path));
      if (slash >= 0) open.append(el('span', 'dir', f.path.slice(0, slash)));
      const stat = el('span', 'stat');
      stat.append(el('span', 'add', `+${f.added}`), el('span', 'del', `-${f.removed}`));
      const acts = el('span', 'acts');
      acts.append(
        button('action', icon('check'), () => vscode.postMessage({ type: 'keep', paths: [f.path] }), `Keep changes to ${f.path}`),
        button('btn small secondary file-undo', [icon('discard'), 'Undo'], (e) => { e.currentTarget.disabled = true; vscode.postMessage({ type: 'revert', paths: [f.path] }); }, `Undo the changes to ${f.path} only`),
      );
      li.append(open, stat, acts, el('span', 'letter', { added: 'A', deleted: 'D', modified: 'M' }[f.status]));
      return li;
    }));
    applyCollapsed();
  }

  // ---------- Toolbar state ----------
  // The running task's elapsed time and Stop sit in its tab at the top (see renderTabs).
  let lastBusy = false;
  const fmtWindow = (n) => (n >= 1e6 ? `${+(n / 1e6).toFixed(1)}M` : `${Math.round(n / 1000)}k`);
  function renderToolbar() {
    if (state.busy !== lastBusy) { lastBusy = state.busy; if (!state.busy) log.querySelectorAll('.tool-stop').forEach((b) => b.remove()); }
    const model = state.active?.model || 'Pick a model';
    const win = state.active?.window;
    modelBtn.replaceChildren(el('span', 'text', model), ...(win ? [el('span', 'model-window', fmtWindow(win))] : []), icon('chevron-down'));
    modelBtn.title = win ? `Model: ${model}. Context window: ${win.toLocaleString()} tokens.` : `Model: ${model}`;
    // Loop Mode shows only its Think and Loop models in place of the chat model.
    nightPlanBtn.hidden = nightLoopBtn.hidden = !state.night;
    modelBtn.hidden = !!state.night;
    nightPlanBtn.replaceChildren(icon('lightbulb'), el('span', 'text', `Think: ${state.nightPlanner?.model || 'none'}`), icon('chevron-down'));
    nightPlanBtn.title = `Loop Mode Think model: splits each request into a todo list in todo.md. None: the Loop model works on the whole request.`;
    nightLoopBtn.replaceChildren(icon('sync'), el('span', 'text', `Loop: ${state.nightWorker?.model || 'chat model'}`), icon('chevron-down'));
    nightLoopBtn.title = 'Loop Mode Loop model: works on one todo.md item per run, checks it off, and retries until each item is done. Unset: the chat model.';
    nightPlanBtn.disabled = nightLoopBtn.disabled = state.busy;
    const em = EDIT_MODES[state.editMode] ? state.editMode : 'auto';
    settingsBtn.dataset.mode = em;
    settingsBtn.replaceChildren(icon(EDIT_MODES[em][0]), el('span', 'text', EDIT_MODES[em][1]), ...(state.night ? [el('span', 'pick-badge', 'Loop')] : []), icon('chevron-down'));
    settingsBtn.title = `Edit mode: ${EDIT_MODES[em][1]}. ${EDIT_MODES[em][2]}${state.night ? ' Loop Mode is on.' : ''}${state.autoCommit || state.night ? ' Auto-commit is on.' : ''}`;
    settingsBtn.setAttribute('aria-label', `Chat settings. ${settingsBtn.title}`);
    modelBtn.disabled = state.busy;
    if (send.dataset.busy !== String(state.busy)) {
      send.dataset.busy = String(state.busy);
      send.classList.remove('morph');
      void send.offsetWidth; // restart the icon transition
      send.classList.add('morph');
    }
    send.replaceChildren(state.busy ? icon('robot', 'busy-robot') : icon('send'));
    send.title = state.busy ? 'Stop' : 'Send (Enter)';
    send.setAttribute('aria-label', send.title);
    send.classList.toggle('primary', !state.busy);
    progress.classList.toggle('busy', state.busy);
    // While working, the input box gets the animated border (the top bar is no longer drawn).
    const form = $('form');
    form.classList.toggle('busy', state.busy);
    form.setAttribute('aria-busy', String(state.busy));
    input.placeholder = state.busy ? 'Enter steers the running task. Ctrl+Enter queues the next message. Stop ends the task.' : `${PLACEHOLDER} (Enter to send, Shift+Enter for a new line)`;
  }
  for (const b of [modelBtn, nightPlanBtn, nightLoopBtn]) { b.setAttribute('aria-haspopup', 'menu'); b.setAttribute('aria-expanded', 'false'); }

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
    ctxRow.append(chip, el('span', 'ctx-hint', 'Type @ to add files or folders'));
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

  // Elapsed time as m:ss (or h:mm:ss); the running tab's timer ticks once a second.
  const fmtTime = (s) => {
    const pad = (n) => String(n).padStart(2, '0');
    const h = Math.floor(s / 3600), m = Math.floor((s % 3600) / 60);
    return h ? `${h}:${pad(m)}:${pad(s % 60)}` : `${m}:${pad(s % 60)}`;
  };
  setInterval(() => {
    for (const t of document.querySelectorAll('.tab-time[data-started]')) t.textContent = fmtTime(Math.max(0, Math.floor((Date.now() - Number(t.dataset.started)) / 1000)));
  }, 1000);
  // ---------- Tabs: chats and subagents, like browser tabs, then History ----------
  const strip = $('tabStrip'), historyTab = $('tabHistory'), historyBox = $('history');
  let tabs = [], activeTab = null;
  function showView(v) {
    document.body.dataset.view = v;
    historyTab.setAttribute('aria-selected', String(v === 'history'));
    historyTab.tabIndex = v === 'history' ? 0 : -1;
    renderTabs();
    historyBox.hidden = v !== 'history';
    if (v === 'history') {
      if (!historyBox.childElementCount) historyBox.append(el('div', 'history-empty', 'Loading…'));
      vscode.postMessage({ type: 'listTasks' });
    } else if (document.body.dataset.tabKind !== 'subagent') input.focus();
  }
  const TAB_STATUS = { running: 'Running', completed: 'Done', failed: 'Failed', cancelled: 'Stopped' };
  function renderTabs() {
    const onChat = document.body.dataset.view !== 'history';
    const cur = tabs.find((t) => t.id === activeTab);
    const hadFocus = strip.contains(document.activeElement);
    document.body.dataset.tabKind = cur?.kind || 'chat';
    strip.replaceChildren(...tabs.map((t) => {
      const on = onChat && t.id === activeTab;
      const tab = el('div', `chat-tab ${t.kind}`);
      tab.dataset.id = t.id;
      if (t.role) tab.dataset.role = t.role;
      if (t.status) tab.dataset.status = t.status;
      tab.setAttribute('role', 'tab');
      tab.setAttribute('aria-selected', String(on));
      tab.setAttribute('aria-controls', 'log');
      tab.tabIndex = on ? 0 : -1;
      const what = t.kind === 'subagent' ? `Subagent ${t.title} (${t.role || 'agent'})` : `Agent chat: ${t.title}`;
      tab.title = t.status ? `${what} · ${TAB_STATUS[t.status] || t.status}` : what;
      const mark = t.status === 'running' ? icon('loading', 'codicon-modifier-spin tab-icon') : icon(t.kind === 'subagent' ? ROLE_ICONS[t.role] || 'person' : 'comment-discussion', 'tab-icon');
      const close = button('tab-close', icon('close'), (e) => { e.stopPropagation(); closeTab(t.id); }, `Close ${t.title}`);
      close.tabIndex = -1;
      tab.append(mark, el('span', 'tab-title', t.title));
      if (t.kind === 'subagent' && t.status && t.status !== 'running') tab.append(el('span', 'tab-state'));
      // The running chat: its elapsed time, and Stop in place of Close.
      if (t.started) {
        const time = el('span', 'tab-time');
        time.dataset.started = String(t.started);
        time.textContent = fmtTime(Math.max(0, Math.floor((Date.now() - t.started) / 1000)));
        const stop = button('tab-stop', icon('debug-stop'), (e) => { e.stopPropagation(); vscode.postMessage({ type: 'cancel' }); }, 'Stop the running task');
        stop.tabIndex = -1;
        tab.append(time, stop);
      } else tab.append(close);
      tab.addEventListener('click', () => selectTab(t.id));
      tab.addEventListener('auxclick', (e) => { if (e.button === 1) { e.preventDefault(); closeTab(t.id); } });
      return tab;
    }));
    const sel = strip.querySelector('[aria-selected="true"]');
    sel?.scrollIntoView({ block: 'nearest', inline: 'nearest' });
    if (hadFocus) (sel || strip.querySelector('.chat-tab'))?.focus();
  }
  function selectTab(id) {
    if (id !== activeTab) {
      activeTab = id;
      vscode.postMessage({ type: 'selectTab', id });
    }
    showView('chat');
  }
  function closeTab(id) { vscode.postMessage({ type: 'closeTab', id }); }
  historyTab.addEventListener('click', () => showView('history'));
  $('views').addEventListener('keydown', (e) => {
    const all = [...strip.querySelectorAll('.chat-tab'), historyTab];
    const at = all.indexOf(document.activeElement);
    if (at < 0) return;
    if ((e.key === 'Delete' || e.key === 'Backspace') && at < all.length - 1) { e.preventDefault(); return closeTab(all[at].dataset.id); }
    if ((e.key === 'Enter' || e.key === ' ') && at < all.length - 1) { e.preventDefault(); return all[at].click(); }
    const d = { ArrowLeft: -1, ArrowRight: 1, Home: -at, End: all.length - 1 - at }[e.key];
    if (d === undefined) return;
    e.preventDefault();
    const next = all[(at + d + all.length) % all.length];
    next.focus();
    next.click();
  });
  $('newChat').addEventListener('click', () => vscode.postMessage({ type: 'newTab' }));

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
  const SORTS = {
    recent: ['Newest first', (a, b) => b.updatedMs - a.updatedMs],
    oldest: ['Oldest first', (a, b) => a.updatedMs - b.updatedMs],
    title: ['Title A–Z', (a, b) => a.title.localeCompare(b.title, undefined, { sensitivity: 'base', numeric: true }) || b.updatedMs - a.updatedMs],
  };
  let historySort = SORTS[vscode.getState()?.historySort] ? vscode.getState().historySort : 'recent';
  let lastTasks = null;
  function historyHead(count) {
    const head = el('div', 'history-head');
    const sel = el('select', 'history-sort');
    sel.setAttribute('aria-label', 'Sort history');
    for (const [k, [label]] of Object.entries(SORTS)) {
      const o = el('option', '', label);
      o.value = k;
      o.selected = k === historySort;
      sel.append(o);
    }
    sel.addEventListener('change', () => {
      historySort = sel.value;
      vscode.setState({ ...(vscode.getState() || {}), historySort });
      renderTasks(lastTasks);
      historyBox.querySelector('.history-sort')?.focus();
    });
    head.append(el('span', 'history-count', `${count} chat${count === 1 ? '' : 's'}`), el('span', 'spacer'), sel,
      button('btn small history-clear', [icon('trash'), 'Clear all'], () => vscode.postMessage({ type: 'clearHistory' }), 'Delete all saved chats'));
    return head;
  }
  function renderTasks(m) {
    lastTasks = m;
    historyBox.replaceChildren();
    if (m.error) return void historyBox.append(el('div', 'history-empty', `Could not load history: ${m.error}`));
    if (!m.tasks.length) return void historyBox.append(el('div', 'history-empty', 'No saved chats yet.'));
    historyBox.append(historyHead(m.tasks.length));
    const tasks = [...m.tasks].sort(SORTS[historySort][1]);
    let group = null, list = null;
    tasks.forEach((t, i) => {
      const g = historySort === 'title' ? '' : dayGroup(t.updatedMs);
      if (!list || g !== group) {
        group = g;
        if (g) historyBox.append(el('div', 'history-group', g));
        list = el('div', 'history-list');
        historyBox.append(list);
      }
      const [name, ...mods] = (STATUS_ICONS[t.status] || 'warning').split(' ');
      const item = el('div', 'history-item' + (t.id === m.current ? ' current' : ''));
      item.style.setProperty('--i', String(Math.min(i, 20)));
      const row = button('history-row', [], () => vscode.postMessage({ type: 'openTask', id: t.id }), t.title);
      const del = button('history-delete', icon('trash'), () => {
        m.tasks = m.tasks.filter((x) => x.id !== t.id);
        renderTasks(m);
        vscode.postMessage({ type: 'deleteTask', id: t.id });
      }, `Delete "${t.title}"`);
      item.append(row, del);
      const time = new Date(t.updatedMs);
      const when = g === 'Today' || g === 'Yesterday' ? time.toLocaleTimeString(undefined, { hour: '2-digit', minute: '2-digit' }) : time.toLocaleDateString(undefined, { month: 'short', day: 'numeric' });
      const meta = [t.files ? `${t.files} file${t.files > 1 ? 's' : ''}` : '', when].filter(Boolean).join(' · ');
      const text = el('span', 'history-text');
      text.append(el('span', 'history-title', t.title), el('span', 'history-meta', meta));
      row.append(icon(name, ['status', t.status, ...mods].join(' ')), text);
      list.append(item);
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
    if (!text && !attachments.length) return;
    input.value = '';
    autosize();
    if (text) rememberPrompt(text);
    const images = attachments.map(({ mediaType, data }) => ({ mediaType, data }));
    attachments = [];
    renderAttachments();
    vscode.postMessage({ type: 'send', text, queue, images });
    scroll(true);
  });

  // ---------- Images: paste, drop, or pick them; they go with the next message ----------
  const IMAGE_TYPES = ['image/png', 'image/jpeg', 'image/gif', 'image/webp'];
  const MAX_IMAGES = 8, MAX_IMAGE_BYTES = 5 * 1024 * 1024;
  const attachList = $('attachments'), attachFile = $('attachFile');
  let attachments = []; // { mediaType, data, name }
  let attachHint = null;
  function renderAttachments() {
    attachList.hidden = !attachments.length && !attachHint;
    const chips = attachments.map((a, i) => {
      const chip = el('div', 'attachment');
      const img = el('img');
      img.src = `data:${a.mediaType};base64,${a.data}`;
      img.alt = a.name;
      chip.title = a.name;
      chip.append(img, button('action remove', icon('close'), () => { attachments.splice(i, 1); renderAttachments(); input.focus(); }, `Remove ${a.name}`));
      return chip;
    });
    attachList.replaceChildren(...chips, ...(attachHint ? [el('span', 'attach-hint', attachHint)] : []));
  }
  function hint(text) {
    attachHint = text;
    renderAttachments();
    setTimeout(() => { if (attachHint === text) { attachHint = null; renderAttachments(); } }, 4000);
  }
  function attachFiles(files) {
    for (const f of files) {
      if (!IMAGE_TYPES.includes(f.type)) { hint(`${f.name || 'This file'} is not a PNG, JPEG, GIF, or WebP image.`); continue; }
      if (attachments.length >= MAX_IMAGES) return hint(`At most ${MAX_IMAGES} images per message.`);
      if (f.size > MAX_IMAGE_BYTES) { hint(`${f.name || 'The image'} is larger than 5 MB.`); continue; }
      const r = new FileReader();
      r.onload = () => {
        const data = String(r.result).split(',')[1];
        if (!data || attachments.length >= MAX_IMAGES) return;
        attachments.push({ mediaType: f.type, data, name: f.name || 'Pasted image' });
        renderAttachments();
      };
      r.readAsDataURL(f);
    }
  }
  input.addEventListener('paste', (e) => {
    const files = [...(e.clipboardData?.files ?? [])].filter((f) => f.type.startsWith('image/'));
    if (!files.length) return;
    e.preventDefault();
    attachFiles(files);
  });
  $('form').addEventListener('dragover', (e) => { if ([...(e.dataTransfer?.types ?? [])].includes('Files')) e.preventDefault(); });
  $('form').addEventListener('drop', (e) => {
    const files = [...(e.dataTransfer?.files ?? [])];
    if (!files.length) return;
    e.preventDefault();
    attachFiles(files);
  });
  $('attachBtn').addEventListener('click', () => attachFile.click());
  attachFile.addEventListener('change', () => { attachFiles([...attachFile.files]); attachFile.value = ''; });
  send.addEventListener('click', () => (state.busy ? vscode.postMessage({ type: 'cancel' }) : $('form').requestSubmit()));
  // Refine: the host rewrites the prompt with the routed model; the input stays locked until the answer comes.
  let refining = false;
  function setRefining(on) {
    refining = on;
    input.readOnly = on;
    refineBtn.disabled = on;
    refineBtn.setAttribute('aria-busy', String(on));
    refineBtn.replaceChildren(on ? icon('loading', 'codicon-modifier-spin') : icon('sparkle'));
  }
  refineBtn.addEventListener('click', () => {
    if (refining || !input.value.trim()) return input.focus();
    setRefining(true);
    vscode.postMessage({ type: 'refine', text: input.value });
  });
  function applyRefined(m) {
    setRefining(false);
    input.focus();
    if (!m.text) {
      refineBtn.title = `Refine failed: ${m.error || 'no answer'}`;
      refineBtn.classList.add('refine-failed');
      setTimeout(() => { refineBtn.classList.remove('refine-failed'); refineBtn.title = REFINE_TITLE; }, 4000);
      return;
    }
    // insertText keeps the change in the undo stack, so Ctrl+Z brings back the original prompt.
    input.select();
    if (!document.execCommand('insertText', false, m.text)) input.value = m.text;
    refineBtn.title = `${REFINE_TITLE} Last refined by ${m.model}.`;
    autosize();
  }
  const REFINE_TITLE = refineBtn.title;

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
    input.value = i < 0 ? histDraft : promptHistory.at(-1 - i);
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
      // A folder keeps the menu open to pick a file inside it, or the folder itself (first row).
      const browse = s.path.endsWith('/') && !s.self;
      const text = s.path + (browse ? '' : ' ');
      input.value = input.value.slice(0, at) + text + after;
      const end = at + text.length;
      input.setSelectionRange(end, end);
      if (browse) { slashActive = 0; renderSlash(); } else closeSlash();
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
    slash.setAttribute('aria-label', 'Files and folders');
    if (fileQuery !== q) { fileQuery = q; vscode.postMessage({ type: 'listFiles', query: q }); }
    const files = (fileItems || []).filter((p) => p !== q).map((p) => ({ path: p }));
    slashItems = q.endsWith('/') && fileItems ? [{ path: q, self: true }, ...files] : files;
    slashActive = Math.min(slashActive, Math.max(0, slashItems.length - 1));
    slash.replaceChildren();
    if (!fileItems) slash.append(el('div', 'slash-empty', 'Loading files…'));
    else if (!slashItems.length) slash.append(el('div', 'slash-empty', `No file or folder matches "@${q}".`));
    slashItems.forEach((s, i) => {
      const o = el('div', 'slash-item file-item');
      o.id = `slash-${i}`;
      o.setAttribute('role', 'option');
      const dir = s.path.endsWith('/');
      const name = dir ? s.path.slice(0, -1).split('/').pop() + '/' : s.path.split('/').pop();
      const parent = s.path.slice(0, s.path.length - name.length).replace(/\/$/, '');
      const head = el('div', 'slash-head');
      head.append(icon(s.self ? 'folder-opened' : dir ? 'folder' : 'file'), el('span', 'slash-name', name));
      if (s.self) head.append(el('span', 'slash-params', 'Add this folder'));
      else if (parent) head.append(el('span', 'slash-params', parent));
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

  // A code block in the prompt: typing ``` at a line start adds the closing fence and puts the caret inside.
  // Enter inside an open block adds a line; Ctrl/Cmd+Enter still sends.
  const FENCE = '```';
  const fenceOpen = (text) => (text.split(FENCE).length - 1) % 2 === 1;
  input.addEventListener('keydown', (e) => {
    if (e.key !== 'Enter' || e.shiftKey || e.isComposing) return;
    if (!(e.ctrlKey || e.metaKey) && fenceOpen(input.value.slice(0, input.selectionStart))) return;
    e.preventDefault();
    queueNext = e.ctrlKey || e.metaKey;
    $('form').requestSubmit();
  });
  input.addEventListener('input', autosize);
  // Backticks: ` wraps the selection or adds a closing `; typing ` before a closing ` steps over it;
  // a third ` at a line start opens a ``` block with its closing fence. A selection of several
  // lines becomes a block. Edits go through insertText, so Ctrl+Z (Cmd+Z) undoes them.
  const insert = (text, caretFrom, caretTo = caretFrom) => {
    const start = input.selectionStart;
    if (!document.execCommand('insertText', false, text)) input.setRangeText(text, start, input.selectionEnd, 'end');
    input.setSelectionRange(start + caretFrom, start + caretTo);
    autosize();
  };
  input.addEventListener('keydown', (e) => {
    if (e.key !== '`' || e.isComposing || e.ctrlKey || e.metaKey || e.altKey) return;
    const { value: v, selectionStart: s, selectionEnd: t } = input;
    const sel = v.slice(s, t);
    const lineHead = v.slice(v.lastIndexOf('\n', s - 1) + 1, s);
    e.preventDefault();
    if (sel) {
      if (sel.includes('\n')) return insert(`${lineHead ? '\n' : ''}${FENCE}\n${sel.replace(/\n$/, '')}\n${FENCE}\n`, (lineHead ? 1 : 0) + FENCE.length);
      return insert(`\`${sel}\``, 1, 1 + sel.length);
    }
    // The third backtick at a line start: a code block, with the caret on its empty line.
    if (lineHead === '``' && !fenceOpen(v.slice(0, s - 2))) {
      input.setSelectionRange(s - 2, v[s] === '`' ? s + 1 : s);
      return insert(`${FENCE}\n\n${FENCE}`, FENCE.length + 1);
    }
    // Inside a code block, or after a word character, a backtick is just a backtick.
    if (fenceOpen(v.slice(0, s)) || /\w$/.test(lineHead)) return insert('`', 1);
    if (v[s] === '`') return input.setSelectionRange(s + 1, s + 1);
    insert('``', 1);
  });

  // ---------- Messages from the extension ----------
  window.addEventListener('message', ({ data: m }) => {
    switch (m.type) {
      case 'history':
        log.replaceChildren();
        turn = null; current = null; turnFiles = null;
        replaying = true;
        m.items.forEach(renderItem);
        replaying = false;
        // A tab drawn while its answer streams keeps the last message open for the text still to come.
        if (!m.streaming) current = null;
        welcome();
        scroll(true);
        break;
      case 'status':
        state = { ...state, busy: m.busy, editMode: m.editMode ?? state.editMode, autoCommit: m.autoCommit ?? state.autoCommit, night: m.night ?? state.night, nightPlanner: m.nightPlanner ?? state.nightPlanner, nightWorker: m.nightWorker ?? state.nightWorker, refineModel: 'refineModel' in m ? m.refineModel : state.refineModel, helperModel: 'helperModel' in m ? m.helperModel : state.helperModel };
        renderToolbar();
        fillSettings();
        break;
      case 'models':
        state.modelGroups = m.groups;
        state.active = m.active;
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
          row.querySelector('.tool-stop')?.remove();
          row.classList.toggle('quiet', Boolean(row.dataset.edit) && m.ok);
          if (row.dataset.edit && !m.ok) row.querySelector('.label').textContent = 'Edit failed';
          attachOutput(row, m.id, row.querySelector('.label')?.dataset.tool || '', m.ok, m.output, m.outputCut);
          tokenChip(row, m.tokens);
          scroll();
        }
        break;
      }
      case 'approval': renderApproval(m); break;
      case 'refined': applyRefined(m); break;
      case 'questions': renderQuestions(m); break;
      case 'questionsDone':
        log.querySelector(`.questions[data-call-id="${CSS.escape(m.callId)}"]`)?.remove();
        break;
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
      case 'usage': {
        // The running total for the task, from the runtime: input and output tokens, never a price.
        const cached = m.cached ? ` (${fmtCount(m.cached)} cached)` : '';
        const note = el('div', 'usage-note', `↑${fmtCount(m.input)}${cached} ↓${fmtCount(m.output)} tokens`);
        if (m.cached) note.title = `${Math.round((m.cached / Math.max(1, m.input)) * 100)}% of the prompt came from the provider's prompt cache, which costs less.`;
        response().append(note);
        scroll();
        break;
      }
      case 'queue': renderQueue(m.items, m.steering); break;
      case 'tasks': renderTasks(m); break;
      case 'tabs':
        tabs = m.tabs;
        activeTab = m.active;
        renderTabs();
        break;
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
