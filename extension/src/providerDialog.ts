import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { describeError } from './chatView.js';
import {
  DEFAULT_BASE_URLS,
  type ProviderRef,
  activeRef,
  baseUrlFor,
  getApiKey,
  hasApiKey,
  isProviderKind,
  isProviderRef,
  modelFor,
  providerRefs,
  refKind,
  refLabel,
  removeProvider,
  requiresApiKey,
  saveProvider,
  settings,
  storeApiKey,
} from './config.js';
import { type Listed, formatTokens, listModels } from './models.js';
import type { ModelInfo, ProviderConfig, TestReport } from './protocol.js';
import { type Draft, validateDraft } from './providerValidation.js';
import type { JsonRpcClient } from './rpc.js';

const MAX_FIELD_CHARS = 2048;
const PROBE_MODEL = 'probe';
/** The select value for a new named OpenAI-compatible endpoint. */
const NEW = 'new';
/** Claude runs through the Claude Code CLI, so the dialog does not offer the Anthropic API. */
const HIDDEN_REFS: readonly string[] = ['anthropic'];

type Inbound =
  | { type: 'ready' }
  | { type: 'fetchModels'; draft: unknown; auto?: boolean }
  | { type: 'test'; draft: unknown }
  | { type: 'save'; draft: unknown }
  | { type: 'remove'; ref: unknown }
  | { type: 'cancel' };

const str = (v: unknown) => (typeof v === 'string' ? v.trim().slice(0, MAX_FIELD_CHARS) : '');

/** A draft plus the provider it edits; `ref` is undefined for a new named endpoint. */
type Target = Draft & { ref?: ProviderRef };

function parseDraft(raw: unknown): Target | undefined {
  const d = raw as Record<string, unknown> | undefined;
  if (!d || (d.ref !== NEW && !isProviderRef(d.ref))) return undefined;
  const ref = d.ref === NEW ? undefined : (d.ref as ProviderRef);
  const named = !ref || !isProviderKind(ref);
  return {
    ref,
    kind: ref ? refKind(ref) : 'openaiCompatible',
    named,
    name: named ? str(d.name).slice(0, 61) : undefined,
    baseUrl: str(d.baseUrl).replace(/\/+$/, ''),
    model: str(d.model),
    apiKey: str(d.apiKey),
  };
}

/** A model row for the webview: the context size is formatted here, so the page has no number logic. */
const row = (m: ModelInfo) => ({ id: m.id, ctx: formatTokens(m.contextWindow), caps: m.capabilities, details: m.details ?? '' });

export class ProviderDialog {
  private static current?: ProviderDialog;
  private busy = false;

  static show(ctx: vscode.ExtensionContext, getClient: () => Promise<JsonRpcClient>, ref?: ProviderRef): void {
    if (ProviderDialog.current) {
      ProviderDialog.current.panel.reveal();
      return;
    }
    const panel = vscode.window.createWebviewPanel('task-x.providerDialog', 'Add LLM Provider', vscode.ViewColumn.Active, {
      enableScripts: true,
      localResourceRoots: [vscode.Uri.joinPath(ctx.extensionUri, 'media')],
      retainContextWhenHidden: true,
    });
    panel.iconPath = vscode.Uri.joinPath(ctx.extensionUri, 'media', 'logo.svg');
    const start = ref ?? activeRef();
    ProviderDialog.current = new ProviderDialog(ctx, panel, getClient, HIDDEN_REFS.includes(start) ? 'ollama' : start);
  }

  private constructor(
    private readonly ctx: vscode.ExtensionContext,
    private readonly panel: vscode.WebviewPanel,
    private readonly getClient: () => Promise<JsonRpcClient>,
    private readonly initialRef: ProviderRef,
  ) {
    panel.webview.html = this.html(panel.webview);
    panel.onDidDispose(() => (ProviderDialog.current = undefined), undefined, ctx.subscriptions);
    panel.webview.onDidReceiveMessage((m: Inbound) => this.onMessage(m), undefined, ctx.subscriptions);
  }

  private post(msg: object): void {
    void this.panel.webview.postMessage(msg);
  }

  private async init(selected = this.initialRef): Promise<void> {
    const active = activeRef();
    const kinds = await Promise.all(
      providerRefs()
        .filter((ref) => !HIDDEN_REFS.includes(ref))
        .map(async (ref) => {
          const kind = refKind(ref);
          return {
            ref,
            label: refLabel(ref),
            named: !isProviderKind(ref),
            local: kind === 'ollama',
            defaultBaseUrl: DEFAULT_BASE_URLS[kind],
            requiresKey: requiresApiKey(kind),
            baseUrl: baseUrlFor(ref),
            model: modelFor(ref),
            hasKey: await hasApiKey(this.ctx.secrets, ref),
            active: ref === active,
          };
        }),
    );
    kinds.push({ ref: NEW, label: 'New endpoint', named: true, local: false, defaultBaseUrl: '', requiresKey: true, baseUrl: '', model: '', hasKey: false, active: false });
    this.post({ type: 'init', kinds, selected: kinds.some((k) => k.ref === selected) ? selected : kinds[0].ref });
    void this.detectOllama();
  }

  /** Looks for Ollama (server, else the CLI) so its card shows what is installed before it is opened. */
  private async detectOllama(): Promise<void> {
    const r = await this.list({ kind: 'ollama', model: PROBE_MODEL, baseUrl: baseUrlFor('ollama') || undefined, timeoutMs: 5_000 });
    const state = r.error ? (r.models.length ? 'cli' : 'missing') : 'running';
    this.post({ type: 'detected', ref: 'ollama', state, count: r.models.length });
  }

  private async list(provider: ProviderConfig): Promise<Listed> {
    const rpc = await this.getClient();
    return listModels(async (p) => (await rpc.request<{ models: ModelInfo[] }>('provider/models', { provider: p }, { timeoutMs: p.timeoutMs })).models, provider);
  }

  private async onMessage(m: Inbound): Promise<void> {
    if (m?.type === 'ready') return this.init();
    if (m?.type === 'cancel') return this.panel.dispose();
    if (m?.type === 'remove') {
      if (!isProviderRef(m.ref) || isProviderKind(m.ref)) return;
      const pick = await vscode.window.showWarningMessage(`Remove "${refLabel(m.ref)}" and its API key?`, { modal: true }, 'Remove');
      if (pick) {
        await removeProvider(this.ctx.secrets, m.ref);
        await this.init(activeRef());
      }
      return;
    }
    if (m?.type !== 'fetchModels' && m?.type !== 'test' && m?.type !== 'save') return;
    const draft = parseDraft(m.draft);
    if (!draft || this.busy) return;
    const storedKey = !!draft.ref && (await hasApiKey(this.ctx.secrets, draft.ref));
    const errors = validateDraft(draft, storedKey, m.type !== 'fetchModels');
    // An automatic listing stays quiet: the user has not asked for anything yet.
    if (m.type === 'fetchModels' && m.auto) {
      if (Object.keys(errors).length) return;
    } else {
      this.post({ type: 'errors', errors });
      if (Object.keys(errors).length) return;
    }

    this.busy = true;
    this.post({ type: 'busy', busy: true, action: m.type });
    try {
      if (m.type === 'fetchModels') await this.fetchModels(draft);
      else if (m.type === 'test') await this.test(draft);
      else await this.save(draft);
    } catch (e) {
      this.post({ type: 'result', ok: false, message: describeError(e) });
    } finally {
      this.busy = false;
      this.post({ type: 'busy', busy: false });
    }
  }

  private async providerConfig(d: Target): Promise<ProviderConfig> {
    const apiKey = d.apiKey || (d.ref ? await getApiKey(this.ctx.secrets, d.ref) : undefined);
    return { kind: d.kind, model: d.model || PROBE_MODEL, baseUrl: d.baseUrl || undefined, apiKey, timeoutMs: settings().timeoutMs };
  }

  private async fetchModels(d: Target): Promise<void> {
    const r = await this.list(await this.providerConfig(d));
    this.post({ type: 'models', ref: d.ref ?? NEW, models: r.models.map(row), note: r.note ?? '' });
    if (r.error && !r.models.length) return this.post({ type: 'result', ok: false, message: r.error });
    this.post({ type: 'result', ok: !r.error, message: r.models.length ? `Found ${r.models.length} model${r.models.length > 1 ? 's' : ''}.` : 'The endpoint lists no models.' });
  }

  private async test(d: Target): Promise<void> {
    const rpc = await this.getClient();
    const r = await rpc.request<TestReport>('provider/test', { provider: await this.providerConfig(d) });
    const tools = r.capabilities.toolCalling ? 'tool calling supported' : 'no tool calling';
    const missing = r.modelAvailable === false ? ` Model "${d.model}" is not in the list from the endpoint.` : '';
    this.post({ type: 'result', ok: r.modelAvailable !== false, message: `Connected in ${r.latencyMs} ms; ${tools}.${missing}` });
  }

  private async save(d: Target): Promise<void> {
    const ref = await saveProvider(d.ref, d.kind, d.baseUrl, d.model, d.named ? d.name : undefined);
    if (d.apiKey) await storeApiKey(this.ctx.secrets, ref, d.apiKey);
    vscode.window.showInformationMessage(`task-x now uses ${refLabel(ref)} · ${d.model}.`);
    this.panel.dispose();
  }

  private html(webview: vscode.Webview): string {
    const nonce = randomBytes(16).toString('base64');
    const codicons = webview.asWebviewUri(vscode.Uri.joinPath(this.ctx.extensionUri, 'media', 'codicons', 'codicon.css'));
    const csp = `default-src 'none'; img-src ${webview.cspSource}; font-src ${webview.cspSource}; style-src ${webview.cspSource} 'nonce-${nonce}'; script-src 'nonce-${nonce}';`;
    return /* html */ `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Add LLM Provider</title>
<link rel="stylesheet" href="${codicons}">
<style nonce="${nonce}">
  /* An admin-panel layout: providers on the left, the selected provider on the right. Every color is a theme token. */
  * { box-sizing: border-box; }
  [hidden] { display: none !important; }
  html, body { height: 100%; }
  body { margin: 0; font-family: var(--vscode-font-family); font-size: var(--vscode-font-size); color: var(--vscode-foreground); background: var(--vscode-editor-background); }
  .shell { display: grid; grid-template-columns: minmax(200px, 240px) 1fr; height: 100vh; }
  .side { display: flex; flex-direction: column; min-height: 0; border-right: 1px solid var(--vscode-panel-border); background: var(--vscode-sideBar-background, var(--vscode-editor-background)); }
  .side-head { padding: 16px 16px 8px; font-size: 11px; font-weight: 600; letter-spacing: .04em; text-transform: uppercase; color: var(--vscode-sideBarSectionHeader-foreground, var(--vscode-descriptionForeground)); }
  #providers { flex: 1; overflow-y: auto; padding: 0 8px 8px; }
  .prov { display: grid; grid-template-columns: 20px 1fr auto; align-items: center; column-gap: 8px; width: 100%; padding: 8px; margin: 1px 0; font: inherit; text-align: left; color: inherit; background: transparent; border: 1px solid transparent; border-radius: 4px; cursor: pointer; }
  .prov:hover { background: var(--vscode-list-hoverBackground); }
  .prov:focus-visible { outline: 1px solid var(--vscode-focusBorder); outline-offset: -1px; }
  .prov[aria-selected=true] { color: var(--vscode-list-activeSelectionForeground, inherit); background: var(--vscode-list-activeSelectionBackground); }
  .prov .codicon { font-size: 16px; color: var(--vscode-descriptionForeground); }
  .prov[aria-selected=true] .codicon { color: inherit; }
  .prov-name { overflow: hidden; font-weight: 600; text-overflow: ellipsis; white-space: nowrap; }
  .prov-sub { grid-column: 2 / 4; overflow: hidden; font-size: 11px; text-overflow: ellipsis; white-space: nowrap; opacity: .8; }
  .prov.new { border-style: dashed; border-color: var(--vscode-panel-border); margin-top: 8px; }
  .dot { width: 8px; height: 8px; border-radius: 50%; background: var(--vscode-descriptionForeground); }
  .dot.on { background: var(--vscode-testing-iconPassed); }
  .dot.warn { background: var(--vscode-editorWarning-foreground); }
  .pane { display: flex; flex-direction: column; min-height: 0; min-width: 0; }
  .pane-scroll { flex: 1; overflow-y: auto; padding: 20px 28px 8px; }
  h1 { display: flex; align-items: center; gap: 8px; margin: 0 0 4px; font-size: 22px; font-weight: 600; }
  .badge { padding: 1px 8px; font-size: 11px; font-weight: 500; border-radius: 10px; color: var(--vscode-badge-foreground); background: var(--vscode-badge-background); }
  .lede { margin: 0 0 16px; color: var(--vscode-descriptionForeground); line-height: 1.5; }
  .fields { display: grid; grid-template-columns: repeat(auto-fit, minmax(240px, 1fr)); gap: 12px 16px; margin-bottom: 20px; }
  .field .title { display: block; margin-bottom: 4px; font-weight: 600; }
  .field .title.req::after { content: " (required)"; font-weight: 400; color: var(--vscode-descriptionForeground); }
  .field .desc { margin: 0 0 6px; font-size: 12px; line-height: 1.4; color: var(--vscode-descriptionForeground); }
  input { font: inherit; width: 100%; height: 26px; padding: 2px 6px; border-radius: 2px; color: var(--vscode-input-foreground); background: var(--vscode-input-background); border: 1px solid var(--vscode-input-border, transparent); }
  input::placeholder { color: var(--vscode-input-placeholderForeground); }
  input:focus { outline: 1px solid var(--vscode-focusBorder); outline-offset: -1px; }
  input[aria-invalid=true] { border-color: var(--vscode-inputValidation-errorBorder); }
  .err { margin-top: 4px; padding: 3px 6px; font-size: 12px; color: var(--vscode-inputValidation-errorForeground, var(--vscode-foreground)); background: var(--vscode-inputValidation-errorBackground); border: 1px solid var(--vscode-inputValidation-errorBorder); }
  .err:empty { display: none; }
  .models-head { display: flex; align-items: center; gap: 8px; margin-bottom: 8px; }
  .models-head h2 { margin: 0; font-size: 14px; font-weight: 600; }
  .count { font-size: 12px; color: var(--vscode-descriptionForeground); }
  .models-head .spacer { flex: 1; }
  .search { position: relative; width: min(280px, 50%); }
  .search .codicon { position: absolute; left: 7px; top: 5px; color: var(--vscode-descriptionForeground); }
  .search input { padding-left: 26px; }
  .note { display: flex; gap: 6px; align-items: flex-start; margin: 0 0 8px; padding: 6px 8px; font-size: 12px; line-height: 1.4; border-radius: 4px; border: 1px solid var(--vscode-editorWarning-foreground); background: color-mix(in srgb, var(--vscode-editorWarning-foreground) 10%, transparent); }
  .note:empty { display: none; }
  .table { border: 1px solid var(--vscode-panel-border); border-radius: 4px; overflow: hidden; }
  .thead, .mrow { display: grid; grid-template-columns: 20px minmax(0, 1fr) 90px 64px minmax(120px, 170px); align-items: center; gap: 8px; padding: 6px 10px; }
  .thead { font-size: 11px; font-weight: 600; color: var(--vscode-descriptionForeground); background: color-mix(in srgb, var(--vscode-foreground) 5%, transparent); border-bottom: 1px solid var(--vscode-panel-border); }
  #modelList { max-height: 46vh; overflow-y: auto; }
  .mrow { width: 100%; font: inherit; text-align: left; color: inherit; background: transparent; border: 0; border-bottom: 1px solid color-mix(in srgb, var(--vscode-panel-border) 60%, transparent); cursor: pointer; }
  .mrow:last-child { border-bottom: 0; }
  .mrow:nth-child(even):not([aria-selected=true]):not(:hover) { background: color-mix(in srgb, var(--vscode-foreground) 5%, transparent); }
  .mrow:hover { background: var(--vscode-list-hoverBackground); }
  .mrow:focus-visible { outline: 1px solid var(--vscode-focusBorder); outline-offset: -1px; }
  .mrow[aria-selected=true] { color: var(--vscode-list-activeSelectionForeground, inherit); background: var(--vscode-list-activeSelectionBackground); }
  .mrow .check { visibility: hidden; }
  .mrow[aria-selected=true] .check { visibility: visible; }
  .mid { overflow: hidden; font-family: var(--vscode-editor-font-family); text-overflow: ellipsis; white-space: nowrap; }
  .muted { font-size: 12px; color: var(--vscode-descriptionForeground); }
  .mrow[aria-selected=true] .muted { color: inherit; opacity: .85; }
  .caps { display: flex; flex-wrap: wrap; gap: 4px; }
  .cap { display: inline-flex; align-items: center; gap: 3px; padding: 0 6px; font-size: 11px; line-height: 18px; border-radius: 9px; border: 1px solid var(--vscode-panel-border); }
  .cap .codicon { font-size: 12px; }
  .empty { padding: 16px 10px; font-size: 12px; text-align: center; color: var(--vscode-descriptionForeground); }
  .custom { display: flex; align-items: center; gap: 8px; margin-top: 10px; }
  .custom label { flex: none; font-size: 12px; color: var(--vscode-descriptionForeground); }
  footer { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; padding: 12px 28px; border-top: 1px solid var(--vscode-panel-border); }
  footer .spacer { flex: 1; }
  #result { display: flex; gap: 6px; align-items: center; min-width: 0; font-size: 12px; }
  #result.ok { color: var(--vscode-testing-iconPassed); }
  #result.fail { color: var(--vscode-errorForeground); }
  #result.run { color: var(--vscode-descriptionForeground); }
  button.btn { display: inline-flex; align-items: center; gap: 4px; font: inherit; min-height: 26px; padding: 4px 12px; border-radius: 2px; border: 1px solid var(--vscode-button-border, transparent); cursor: pointer; color: var(--vscode-button-foreground); background: var(--vscode-button-background); }
  button.btn:hover:not(:disabled) { background: var(--vscode-button-hoverBackground); }
  button.btn.secondary { color: var(--vscode-button-secondaryForeground); background: var(--vscode-button-secondaryBackground); }
  button.btn.secondary:hover:not(:disabled) { background: var(--vscode-button-secondaryHoverBackground); }
  button.btn.icon { padding: 4px 6px; }
  button:disabled { opacity: .5; cursor: default; }
  button.btn:focus-visible { outline: 1px solid var(--vscode-focusBorder); outline-offset: 2px; }
  @media (max-width: 640px) {
    .shell { grid-template-columns: 1fr; grid-template-rows: auto 1fr; }
    .side { max-height: 30vh; border-right: 0; border-bottom: 1px solid var(--vscode-panel-border); }
    .thead, .mrow { grid-template-columns: 20px minmax(0, 1fr) 64px; }
    .thead > :nth-child(3), .thead > :nth-child(5), .mrow > :nth-child(3), .mrow > :nth-child(5) { display: none; }
  }
</style>
</head>
<body>
<div class="shell">
  <nav class="side" aria-label="Providers">
    <div class="side-head">Providers</div>
    <div id="providers" role="listbox" aria-label="Providers"></div>
  </nav>
  <form class="pane" id="form" novalidate>
    <div class="pane-scroll">
      <h1><span class="codicon" id="paneIcon" aria-hidden="true"></span><span id="paneTitle"></span><span class="badge" id="inUse" hidden>In use</span></h1>
      <p class="lede" id="paneLede"></p>
      <div class="fields">
        <div class="field" id="nameRow" hidden>
          <label class="title req" for="name">Name</label>
          <p class="desc">Shown in the model picker, for example "OpenRouter" or "LM Studio".</p>
          <input id="name" type="text" maxlength="60" autocomplete="off" spellcheck="false" aria-describedby="nameErr">
          <div class="err" id="nameErr" role="alert"></div>
        </div>
        <div class="field">
          <label class="title" for="baseUrl" id="baseUrlLabel">Base URL</label>
          <p class="desc" id="baseUrlHint"></p>
          <input id="baseUrl" type="url" autocomplete="off" spellcheck="false" aria-describedby="baseUrlHint baseUrlErr">
          <div class="err" id="baseUrlErr" role="alert"></div>
        </div>
        <div class="field">
          <label class="title" for="apiKey" id="apiKeyLabel">API key</label>
          <p class="desc" id="apiKeyHint"></p>
          <input id="apiKey" type="password" autocomplete="off" spellcheck="false" aria-describedby="apiKeyHint apiKeyErr">
          <div class="err" id="apiKeyErr" role="alert"></div>
        </div>
      </div>
      <section aria-labelledby="modelsTitle">
        <div class="models-head">
          <h2 id="modelsTitle">Models</h2><span class="count" id="modelCount"></span>
          <span class="spacer"></span>
          <div class="search"><span class="codicon codicon-search" aria-hidden="true"></span><input id="filter" type="search" placeholder="Search models" aria-label="Search models" autocomplete="off" spellcheck="false"></div>
          <button type="button" class="btn secondary icon" id="fetch" title="Load the models from the endpoint" aria-label="Load models"><span class="codicon codicon-refresh" aria-hidden="true"></span></button>
        </div>
        <div class="note" id="note" role="status"></div>
        <div class="table">
          <div class="thead" aria-hidden="true"><span></span><span>Model</span><span>Size</span><span>Context</span><span>Capabilities</span></div>
          <div id="modelList" role="listbox" aria-label="Models"></div>
        </div>
        <div class="custom">
          <label for="model">Model id</label>
          <input id="model" type="text" autocomplete="off" spellcheck="false" placeholder="Pick a model above, or type its id" aria-describedby="modelErr">
        </div>
        <div class="err" id="modelErr" role="alert"></div>
      </section>
    </div>
    <footer>
      <button type="button" class="btn secondary" id="test"><span class="codicon codicon-plug" aria-hidden="true"></span>Test connection</button>
      <button type="button" class="btn secondary" id="remove" hidden><span class="codicon codicon-trash" aria-hidden="true"></span>Remove</button>
      <div id="result" role="status" aria-live="polite"></div>
      <span class="spacer"></span>
      <button type="button" class="btn secondary" id="cancel">Cancel</button>
      <button type="submit" class="btn" id="save">Save and use</button>
    </footer>
  </form>
</div>
<script nonce="${nonce}">
(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const NEW = 'new';
  const CAPS = { tools: ['tools', 'Tools'], vision: ['eye', 'Vision'], thinking: ['lightbulb', 'Thinking'], embedding: ['symbol-array', 'Embedding'] };
  const f = { name: $('name'), baseUrl: $('baseUrl'), apiKey: $('apiKey'), model: $('model') };
  const buttons = ['fetch', 'test', 'cancel', 'save', 'remove'].map($);
  const providers = $('providers'), list = $('modelList'), filter = $('filter');
  let kinds = [], kind = null, models = [];
  const drafts = {}, listed = {}, detected = {};

  const el = (tag, cls, text) => { const e = document.createElement(tag); if (cls) e.className = cls; if (text != null) e.textContent = text; return e; };
  const icon = (name) => { const i = el('span', 'codicon codicon-' + name); i.setAttribute('aria-hidden', 'true'); return i; };
  const meta = () => kinds.find((k) => k.ref === kind);
  const draft = () => ({ ref: kind, name: f.name.value, baseUrl: f.baseUrl.value, apiKey: f.apiKey.value, model: f.model.value });
  const setResult = (ok, msg, running) => {
    const r = $('result');
    r.replaceChildren();
    r.className = msg ? (running ? 'run' : ok ? 'ok' : 'fail') : '';
    if (msg) r.append(icon(running ? 'loading codicon-modifier-spin' : ok ? 'pass' : 'error'), document.createTextNode(msg));
  };
  const setErrors = (errs) => {
    for (const k of Object.keys(f)) {
      $(k + 'Err').textContent = errs[k] || '';
      f[k].setAttribute('aria-invalid', errs[k] ? 'true' : 'false');
    }
    const first = Object.keys(f).find((k) => errs[k]);
    if (first) f[first].focus();
  };

  // ---------- Providers (left) ----------
  const iconFor = (k) => (k.ref === NEW ? 'add' : k.local ? 'vm' : k.named ? 'server' : 'cloud');
  function subFor(k) {
    if (k.ref === NEW) return 'Add a server by URL';
    const d = detected[k.ref];
    if (d) return d.state === 'running' ? d.count + ' model' + (d.count === 1 ? '' : 's') + ' installed' : d.state === 'cli' ? d.count + ' installed · server not running' : 'Not found';
    if (k.local) return 'Looking for Ollama…';
    return k.model ? k.model : k.hasKey ? 'Key saved' : k.requiresKey ? 'Needs an API key' : 'Ready';
  }
  function dotFor(k) {
    const d = detected[k.ref];
    if (d) return d.state === 'running' ? 'on' : 'warn';
    return k.active ? 'on' : '';
  }
  function renderProviders() {
    providers.replaceChildren(...kinds.map((k) => {
      const b = el('button', 'prov' + (k.ref === NEW ? ' new' : ''));
      b.type = 'button';
      b.setAttribute('role', 'option');
      b.setAttribute('aria-selected', String(k.ref === kind));
      b.dataset.ref = k.ref;
      b.title = k.label;
      const dot = el('span', 'dot ' + dotFor(k));
      dot.title = k.active ? 'In use' : '';
      b.append(icon(iconFor(k)), el('span', 'prov-name', k.label), k.ref === NEW ? el('span') : dot, el('span', 'prov-sub', subFor(k)));
      b.addEventListener('click', () => select(k.ref));
      return b;
    }));
  }
  providers.addEventListener('keydown', (e) => {
    if (e.key !== 'ArrowDown' && e.key !== 'ArrowUp') return;
    e.preventDefault();
    const items = [...providers.querySelectorAll('.prov')];
    const i = items.indexOf(document.activeElement);
    const next = items[(i + (e.key === 'ArrowDown' ? 1 : -1) + items.length) % items.length];
    next?.focus();
    next?.click();
  });

  // ---------- Models (right) ----------
  function renderModels() {
    const q = filter.value.trim().toLowerCase().split(/\\s+/).filter(Boolean);
    const shown = models.filter((m) => { const hay = (m.id + ' ' + m.details + ' ' + m.caps.join(' ')).toLowerCase(); return q.every((w) => hay.includes(w)); });
    $('modelCount').textContent = models.length ? (shown.length === models.length ? models.length + '' : shown.length + ' of ' + models.length) : '';
    if (!shown.length) {
      list.replaceChildren(el('div', 'empty', models.length ? 'No models match the search.' : 'No models loaded. Fill in the connection, then click the refresh button.'));
      return;
    }
    const current = f.model.value.trim();
    list.replaceChildren(...shown.map((m) => {
      const r = el('button', 'mrow');
      r.type = 'button';
      r.setAttribute('role', 'option');
      r.setAttribute('aria-selected', String(m.id === current));
      r.title = m.id;
      const caps = el('span', 'caps');
      for (const c of m.caps) { const [ic, label] = CAPS[c] || ['circle-small', c]; const chip = el('span', 'cap'); chip.append(icon(ic), document.createTextNode(label)); caps.append(chip); }
      if (!m.caps.length) caps.append(el('span', 'muted', '—'));
      r.append(icon('check check'), el('span', 'mid', m.id), el('span', 'muted', m.details || '—'), el('span', 'muted', m.ctx || '—'), caps);
      r.addEventListener('click', () => { f.model.value = m.id; setErrors({}); renderModels(); });
      r.addEventListener('dblclick', () => { f.model.value = m.id; send('save'); });
      return r;
    }));
  }
  filter.addEventListener('input', renderModels);
  filter.addEventListener('keydown', (e) => { if (e.key === 'Enter') { e.preventDefault(); list.querySelector('.mrow')?.click(); } });
  f.model.addEventListener('input', renderModels);

  function select(k) {
    if (kind) drafts[kind] = draft();
    kind = k;
    const m = meta();
    const d = drafts[k] || { name: m.named && k !== NEW ? m.label : '', baseUrl: m.baseUrl, apiKey: '', model: m.model };
    f.name.value = d.name; f.baseUrl.value = d.baseUrl; f.apiKey.value = d.apiKey; f.model.value = d.model;
    $('paneIcon').className = 'codicon codicon-' + iconFor(m);
    $('paneTitle').textContent = k === NEW ? 'New OpenAI-compatible endpoint' : m.label;
    $('inUse').hidden = !m.active;
    $('paneLede').textContent = k === NEW ? 'Any server with an OpenAI-style API: OpenRouter, LM Studio, llama.cpp, vLLM, and others. Add one for each server.'
      : m.local ? 'Models on this machine. task-x finds the installed models through the Ollama server, or through the ollama command when the server is not running.'
      : 'API keys go to the OS keychain through VS Code SecretStorage, never to your settings.';
    $('nameRow').hidden = !m.named;
    $('remove').hidden = !m.named || k === NEW;
    const needsUrl = !m.defaultBaseUrl;
    $('baseUrlLabel').className = needsUrl ? 'title req' : 'title';
    f.baseUrl.placeholder = m.defaultBaseUrl || 'https://my-endpoint.example.com/v1';
    $('baseUrlHint').textContent = needsUrl ? 'The URL before /chat/completions, normally ending in /v1.' : 'Leave empty to use ' + m.defaultBaseUrl + '.';
    $('apiKeyLabel').className = m.requiresKey && !m.hasKey ? 'title req' : 'title';
    f.apiKey.placeholder = m.hasKey ? '•••••••• (saved)' : '';
    $('apiKeyHint').textContent = m.hasKey ? 'A key is saved. Leave empty to keep it, or enter a new key.' : m.requiresKey ? 'Stored in your OS keychain, never in settings.' : 'Optional. Needed only if the server checks keys.';
    filter.value = '';
    models = listed[k]?.models ?? [];
    $('note').textContent = listed[k]?.note ?? '';
    setErrors({}); setResult(true, '');
    renderProviders();
    renderModels();
    // Load the list without a click when the provider can answer: Ollama, or a saved key.
    if (!listed[k] && k !== NEW && (m.local || m.hasKey || !m.requiresKey)) vscode.postMessage({ type: 'fetchModels', draft: draft(), auto: true });
  }

  const send = (type) => vscode.postMessage({ type, draft: draft() });
  $('fetch').addEventListener('click', () => send('fetchModels'));
  $('test').addEventListener('click', () => send('test'));
  $('cancel').addEventListener('click', () => vscode.postMessage({ type: 'cancel' }));
  $('remove').addEventListener('click', () => vscode.postMessage({ type: 'remove', ref: kind }));
  $('form').addEventListener('submit', (e) => { e.preventDefault(); send('save'); });

  window.addEventListener('message', ({ data: m }) => {
    switch (m.type) {
      case 'init':
        kinds = m.kinds;
        kind = null;
        for (const o of [drafts, listed]) for (const k of Object.keys(o)) delete o[k];
        select(m.selected);
        (meta().named && !f.name.value ? f.name : filter).focus();
        break;
      case 'detected':
        detected[m.ref] = m;
        renderProviders();
        break;
      case 'models':
        listed[m.ref] = { models: m.models, note: m.note };
        if (m.ref === kind) { models = m.models; $('note').textContent = m.note; renderModels(); }
        break;
      case 'errors': setErrors(m.errors); break;
      case 'result': setResult(m.ok, m.message); break;
      case 'busy':
        buttons.forEach((b) => (b.disabled = m.busy && b.id !== 'cancel'));
        if (m.busy) setResult(true, m.action === 'save' ? 'Saving…' : m.action === 'test' ? 'Testing…' : 'Loading models…', true);
        break;
    }
  });
  vscode.postMessage({ type: 'ready' });
})();
</script>
</body>
</html>`;
  }
}
