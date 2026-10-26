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
import type { ProviderConfig, TestReport } from './protocol.js';
import { type Draft, validateDraft } from './providerValidation.js';
import type { JsonRpcClient } from './rpc.js';

const MAX_FIELD_CHARS = 2048;
const PROBE_MODEL = 'probe';
/** The select value for a new named OpenAI-compatible endpoint. */
const NEW = 'new';

type Inbound =
  | { type: 'ready' }
  | { type: 'fetchModels'; draft: unknown }
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
    ProviderDialog.current = new ProviderDialog(ctx, panel, getClient, ref ?? activeRef());
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
      providerRefs().map(async (ref) => {
        const kind = refKind(ref);
        return {
          ref,
          label: refLabel(ref),
          named: !isProviderKind(ref),
          defaultBaseUrl: DEFAULT_BASE_URLS[kind],
          requiresKey: requiresApiKey(kind),
          baseUrl: baseUrlFor(ref),
          model: modelFor(ref),
          hasKey: await hasApiKey(this.ctx.secrets, ref),
          active: ref === active,
        };
      }),
    );
    kinds.push({ ref: NEW, label: 'New OpenAI-compatible endpoint…', named: true, defaultBaseUrl: '', requiresKey: true, baseUrl: '', model: '', hasKey: false, active: false });
    this.post({ type: 'init', kinds, selected: kinds.some((k) => k.ref === selected) ? selected : kinds[0].ref });
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
    this.post({ type: 'errors', errors });
    if (Object.keys(errors).length) return;

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
    const rpc = await this.getClient();
    const { models } = await rpc.request<{ models: string[] }>('provider/listModels', { provider: await this.providerConfig(d) });
    this.post({ type: 'models', models });
    this.post({ type: 'result', ok: true, message: models.length ? `Found ${models.length} models.` : 'The endpoint returned no models.' });
  }

  private async test(d: Target): Promise<void> {
    const rpc = await this.getClient();
    const r = await rpc.request<TestReport>('provider/test', { provider: await this.providerConfig(d) });
    if (r.models.length) this.post({ type: 'models', models: r.models });
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
  /* Styled after the VS Code Settings editor; every color is a theme token. */
  * { box-sizing: border-box; }
  [hidden] { display: none !important; }
  body { margin: 0; padding: 20px 24px 32px; font-family: var(--vscode-font-family); font-size: var(--vscode-font-size); color: var(--vscode-foreground); background: var(--vscode-editor-background); }
  main { max-width: 680px; }
  h1 { display: flex; align-items: center; gap: 8px; margin: 0 0 4px; font-size: 26px; font-weight: 600; }
  h1 .codicon { font-size: 22px !important; }
  .lede { margin: 0 0 16px; color: var(--vscode-descriptionForeground); line-height: 1.5; }
  .current { display: flex; align-items: center; gap: 6px; margin: 0 0 20px; padding: 8px 10px; font-size: 12px; border: 1px solid var(--vscode-panel-border); border-radius: 4px; background: color-mix(in srgb, var(--vscode-foreground) 5%, transparent); }
  .current .codicon { color: var(--vscode-testing-iconPassed); }
  .setting { padding: 10px 12px 12px; margin: 0 -12px; border-left: 2px solid transparent; }
  .setting:focus-within { background: var(--vscode-settings-focusedRowBackground, var(--vscode-list-hoverBackground)); border-left-color: var(--vscode-settings-focusedRowBorder, var(--vscode-focusBorder)); }
  .setting:hover:not(:focus-within) { background: var(--vscode-settings-rowHoverBackground, transparent); }
  .title { display: block; font-weight: 600; margin-bottom: 4px; color: var(--vscode-settings-headerForeground, var(--vscode-foreground)); }
  .title.req::after { content: " (required)"; font-weight: 400; color: var(--vscode-descriptionForeground); }
  .desc { margin: 0 0 8px; color: var(--vscode-descriptionForeground); line-height: 1.45; font-size: 12px; }
  .row { display: flex; gap: 6px; align-items: center; }
  .row.end { justify-content: flex-end; margin-top: 8px; }
  .row input { flex: 1; min-width: 0; }
  input { font: inherit; width: 100%; height: 26px; padding: 2px 6px; border-radius: 2px; color: var(--vscode-settings-textInputForeground, var(--vscode-input-foreground)); background: var(--vscode-settings-textInputBackground, var(--vscode-input-background)); border: 1px solid var(--vscode-settings-textInputBorder, var(--vscode-input-border, transparent)); }
  input::placeholder { color: var(--vscode-input-placeholderForeground); }
  input:focus { outline: 1px solid var(--vscode-focusBorder); outline-offset: -1px; }
  input[aria-invalid=true] { border-color: var(--vscode-inputValidation-errorBorder); }
  .err { display: flex; gap: 6px; align-items: flex-start; margin-top: 6px; padding: 4px 6px; font-size: 12px; color: var(--vscode-inputValidation-errorForeground, var(--vscode-foreground)); background: var(--vscode-inputValidation-errorBackground); border: 1px solid var(--vscode-inputValidation-errorBorder); }
  .err:empty { display: none; }
  .err::before { font: normal normal normal 14px/1 codicon; content: "\\ea87"; color: var(--vscode-errorForeground); }
  .cards { display: grid; grid-template-columns: repeat(auto-fill, minmax(200px, 1fr)); gap: 8px; }
  .card { position: relative; display: flex; align-items: flex-start; gap: 8px; min-width: 0; padding: 10px; cursor: pointer; border: 1px solid var(--vscode-settings-dropdownBorder, var(--vscode-panel-border)); border-radius: 4px; background: var(--vscode-editor-background); transition: background var(--md-duration-short4, 200ms) var(--md-motion-standard, ease), border-color var(--md-duration-short4, 200ms) ease; }
  .card:hover { background: var(--vscode-list-hoverBackground); }
  .card:has(input:checked) { border-color: var(--md-sys-color-primary, var(--vscode-focusBorder)); background: color-mix(in srgb, var(--md-sys-color-primary, var(--vscode-focusBorder)) 14%, var(--vscode-editor-background)); }
  .card:has(input:focus-visible) { outline: 1px solid var(--vscode-focusBorder); outline-offset: 2px; }
  .card:has(input:disabled) { opacity: .5; cursor: default; pointer-events: none; }
  .card.new { border-style: dashed; }
  .card input { position: absolute; width: 1px; height: 1px; margin: 0; opacity: 0; }
  .card > .codicon { margin-top: 1px; font-size: 16px; color: var(--vscode-descriptionForeground); }
  .card:has(input:checked) > .codicon { color: var(--md-sys-color-primary, var(--vscode-focusBorder)); }
  .cardText { display: flex; flex: 1; flex-direction: column; min-width: 0; }
  .cardName { overflow: hidden; font-weight: 600; text-overflow: ellipsis; white-space: nowrap; }
  .cardSub { margin-top: 2px; font-size: 11px; color: var(--vscode-descriptionForeground); }
  .badge { align-self: center; padding: 1px 6px; font-size: 10px; white-space: nowrap; border-radius: 8px; color: var(--vscode-badge-foreground); background: var(--vscode-badge-background); }
  footer { display: flex; gap: 8px; align-items: center; flex-wrap: wrap; margin-top: 20px; padding-top: 16px; border-top: 1px solid var(--vscode-settings-headerBorder, var(--vscode-panel-border)); }
  footer .spacer { flex: 1; }
  button { display: inline-flex; align-items: center; gap: 4px; font: inherit; min-height: 26px; padding: 4px 12px; border-radius: 2px; border: 1px solid var(--vscode-button-border, transparent); cursor: pointer; color: var(--vscode-button-foreground); background: var(--vscode-button-background); }
  button:hover:not(:disabled) { background: var(--vscode-button-hoverBackground); }
  button.secondary { color: var(--vscode-button-secondaryForeground); background: var(--vscode-button-secondaryBackground); }
  button.secondary:hover:not(:disabled) { background: var(--vscode-button-secondaryHoverBackground); }
  button:disabled { opacity: .5; cursor: default; }
  button:focus-visible { outline: 1px solid var(--vscode-focusBorder); outline-offset: 2px; }
  #result { display: flex; gap: 6px; align-items: center; margin-top: 12px; min-height: 20px; font-size: 12px; }
  #result.ok { color: var(--vscode-testing-iconPassed); }
  #result.fail { color: var(--vscode-errorForeground); }
  #result.run { color: var(--vscode-descriptionForeground); }
</style>
</head>
<body>
<main>
  <h1><span class="codicon codicon-plug" aria-hidden="true"></span>Add LLM Provider</h1>
  <p class="lede">Connect a model endpoint. The API key goes to the OS keychain, not to your settings.</p>
  <div class="current" id="current" role="status" hidden><span class="codicon codicon-check" aria-hidden="true"></span><span id="currentText"></span></div>
  <form id="form" novalidate>
    <div class="setting">
      <div class="title" id="kindLabel">Provider</div>
      <p class="desc">The API to connect to. OpenAI-compatible covers llama.cpp, vLLM, LM Studio, OpenRouter, and similar servers. Add one named endpoint for each server.</p>
      <div class="cards" id="cards" role="radiogroup" aria-labelledby="kindLabel"></div>
      <div class="row end">
        <button type="button" class="secondary" id="remove" hidden><span class="codicon codicon-trash" aria-hidden="true"></span>Remove endpoint</button>
      </div>
    </div>
    <div class="setting" id="nameRow" hidden>
      <label class="title req" for="name">Name</label>
      <p class="desc">Shown in the model picker, for example "OpenRouter" or "Local vLLM".</p>
      <input id="name" type="text" maxlength="60" autocomplete="off" spellcheck="false" aria-describedby="nameErr">
      <div class="err" id="nameErr" role="alert"></div>
    </div>
    <div class="setting">
      <label class="title" for="baseUrl" id="baseUrlLabel">Base URL</label>
      <p class="desc" id="baseUrlHint"></p>
      <input id="baseUrl" type="url" autocomplete="off" spellcheck="false" aria-describedby="baseUrlHint baseUrlErr">
      <div class="err" id="baseUrlErr" role="alert"></div>
    </div>
    <div class="setting">
      <label class="title" for="apiKey" id="apiKeyLabel">API key</label>
      <p class="desc" id="apiKeyHint"></p>
      <input id="apiKey" type="password" autocomplete="off" spellcheck="false" aria-describedby="apiKeyHint apiKeyErr">
      <div class="err" id="apiKeyErr" role="alert"></div>
    </div>
    <div class="setting">
      <label class="title req" for="model">Model</label>
      <p class="desc" id="modelHint">Type a model id, or load the list from the endpoint.</p>
      <div class="row">
        <input id="model" type="text" list="models" autocomplete="off" spellcheck="false" aria-describedby="modelHint modelErr">
        <button type="button" class="secondary" id="fetch"><span class="codicon codicon-refresh" aria-hidden="true"></span>Fetch models</button>
      </div>
      <datalist id="models"></datalist>
      <div class="err" id="modelErr" role="alert"></div>
    </div>
    <div id="result" role="status" aria-live="polite"></div>
    <footer>
      <button type="button" class="secondary" id="test"><span class="codicon codicon-plug" aria-hidden="true"></span>Test connection</button>
      <span class="spacer"></span>
      <button type="button" class="secondary" id="cancel">Cancel</button>
      <button type="submit" id="save">Save and use</button>
    </footer>
  </form>
</main>
<script nonce="${nonce}">
(() => {
  const vscode = acquireVsCodeApi();
  const $ = (id) => document.getElementById(id);
  const NEW = 'new';
  const f = { name: $('name'), baseUrl: $('baseUrl'), apiKey: $('apiKey'), model: $('model') };
  const buttons = ['fetch', 'test', 'cancel', 'save', 'remove'].map($);
  const cards = $('cards');
  let kinds = [], kind = null;
  const drafts = {};

  const meta = () => kinds.find((k) => k.ref === kind);
  const draft = () => ({ ref: kind, name: f.name.value, baseUrl: f.baseUrl.value, apiKey: f.apiKey.value, model: f.model.value });
  const setResult = (ok, msg, running) => {
    const r = $('result');
    r.replaceChildren();
    r.className = msg ? (running ? 'run' : ok ? 'ok' : 'fail') : '';
    if (!msg) return;
    const i = document.createElement('span');
    i.className = 'codicon codicon-' + (running ? 'loading codicon-modifier-spin' : ok ? 'pass' : 'error');
    i.setAttribute('aria-hidden', 'true');
    r.append(i, document.createTextNode(msg));
  };
  const setErrors = (errs) => {
    for (const k of Object.keys(f)) {
      $(k + 'Err').textContent = errs[k] || '';
      f[k].setAttribute('aria-invalid', errs[k] ? 'true' : 'false');
    }
    const first = Object.keys(f).find((k) => errs[k]);
    if (first) f[first].focus();
  };
  const setModels = (list) => $('models').replaceChildren(...list.map((m) => { const o = document.createElement('option'); o.value = m; return o; }));

  const iconFor = (k) => (k.ref === NEW ? 'add' : k.named ? 'server' : k.ref === 'ollama' ? 'vm' : 'cloud');
  const subFor = (k) => (k.ref === NEW ? 'Add a server by URL' : k.hasKey ? 'Key saved' : k.requiresKey ? 'Key needed' : 'Key optional');
  const renderCards = () => cards.replaceChildren(...kinds.map((k) => {
    const label = document.createElement('label');
    label.className = 'card' + (k.ref === NEW ? ' new' : '');
    const radio = document.createElement('input');
    radio.type = 'radio';
    radio.name = 'kind';
    radio.value = k.ref;
    radio.addEventListener('change', () => select(k.ref));
    const icon = document.createElement('span');
    icon.className = 'codicon codicon-' + iconFor(k);
    icon.setAttribute('aria-hidden', 'true');
    const text = document.createElement('span');
    text.className = 'cardText';
    const name = document.createElement('span');
    name.className = 'cardName';
    name.textContent = k.label;
    const sub = document.createElement('span');
    sub.className = 'cardSub';
    sub.textContent = subFor(k);
    text.append(name, sub);
    label.append(radio, icon, text);
    if (k.active) {
      const badge = document.createElement('span');
      badge.className = 'badge';
      badge.textContent = 'In use';
      label.append(badge);
    }
    return label;
  }));
  const checkCard = (k) => cards.querySelectorAll('input').forEach((r) => { r.checked = r.value === k; });

  const select = (k) => {
    if (kind) drafts[kind] = draft();
    kind = k;
    const m = meta();
    const d = drafts[k] || { name: m.named && k !== NEW ? m.label : '', baseUrl: m.baseUrl, apiKey: '', model: m.model };
    f.name.value = d.name; f.baseUrl.value = d.baseUrl; f.apiKey.value = d.apiKey; f.model.value = d.model;
    $('nameRow').hidden = !m.named;
    $('remove').hidden = !m.named || k === NEW;
    const needsUrl = !m.defaultBaseUrl;
    $('baseUrlLabel').className = needsUrl ? 'title req' : 'title';
    f.baseUrl.placeholder = m.defaultBaseUrl || 'https://my-endpoint.example.com/v1';
    $('baseUrlHint').textContent = needsUrl ? 'The URL before /chat/completions, normally ending in /v1.' : 'Leave empty to use the default.';
    $('apiKeyLabel').className = m.requiresKey && !m.hasKey ? 'title req' : 'title';
    f.apiKey.placeholder = m.hasKey ? '•••••••• (saved)' : '';
    $('apiKeyHint').textContent = m.hasKey ? 'A key is saved. Leave empty to keep it, or enter a new key to replace it.' : m.requiresKey ? 'Stored in your OS keychain through VS Code SecretStorage, never in settings.' : 'Optional. Needed only if the server checks keys.';
    setModels([]); setErrors({}); setResult(true, '');
    checkCard(k);
  };

  const send = (type) => vscode.postMessage({ type, draft: draft() });
  $('fetch').addEventListener('click', () => send('fetchModels'));
  $('test').addEventListener('click', () => send('test'));
  $('cancel').addEventListener('click', () => vscode.postMessage({ type: 'cancel' }));
  $('remove').addEventListener('click', () => vscode.postMessage({ type: 'remove', ref: kind }));
  $('form').addEventListener('submit', (e) => { e.preventDefault(); send('save'); });

  window.addEventListener('message', ({ data: m }) => {
    switch (m.type) {
      case 'init': {
        kinds = m.kinds;
        kind = null;
        for (const k of Object.keys(drafts)) delete drafts[k];
        renderCards();
        const used = kinds.find((k) => k.active);
        $('current').hidden = !used;
        if (used) $('currentText').textContent = 'Using ' + used.label + ' · ' + (used.model || 'no model set');
        select(m.selected);
        (meta().named && !f.name.value ? f.name : f.baseUrl).focus();
        break;
      }
      case 'errors': setErrors(m.errors); break;
      case 'models': setModels(m.models); break;
      case 'result': setResult(m.ok, m.message); break;
      case 'busy':
        buttons.forEach((b) => (b.disabled = m.busy && b.id !== 'cancel'));
        cards.querySelectorAll('input').forEach((r) => (r.disabled = m.busy));
        if (m.busy) setResult(true, m.action === 'save' ? 'Saving…' : m.action === 'test' ? 'Testing…' : 'Fetching models…', true);
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
