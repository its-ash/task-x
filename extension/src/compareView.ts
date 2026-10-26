import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import type { BackgroundTasks } from './background.js';
import { type ChatViewProvider, describeError } from './chatView.js';
import { type ProviderRef, activeRef, hasApiKey, modelFor, providerConfig, providerRefs, refLabel, refNeedsKey } from './config.js';
import type { Candidate, ProviderConfig } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';
import { formatCost, limits, policy, prices } from './runParams.js';

const MIN_CANDIDATES = 2;
const MAX_CANDIDATES = 4;

type Inbound = { type: 'ready' } | { type: 'diff'; i: number } | { type: 'keep'; i: number } | { type: 'discardAll' } | { type: 'open'; i: number };

/** Asks for a task and 2 to 4 models, runs `agent/bestOf`, and shows the candidates side by side. */
export async function runBestOf(ctx: vscode.ExtensionContext, getClient: () => Promise<JsonRpcClient>, chat: ChatViewProvider, bg: BackgroundTasks): Promise<void> {
  const prompt = await vscode.window.showInputBox({ title: 'task-x: Best of N', prompt: 'The task. Each model works on it in its own worktree; you keep the best result.', ignoreFocusOut: true });
  if (!prompt?.trim()) return;
  const items: (vscode.QuickPickItem & { provider?: ProviderRef; extra?: boolean })[] = [];
  for (const ref of providerRefs()) {
    const model = modelFor(ref);
    if (model && (!refNeedsKey(ref) || (await hasApiKey(ctx.secrets, ref)))) items.push({ label: model, description: refLabel(ref), provider: ref, picked: ref === activeRef() });
  }
  items.push({ label: `$(add) More ${refLabel(activeRef())} models…`, description: 'Enter model ids', extra: true });
  const picks = await vscode.window.showQuickPick(items, { title: `task-x: Pick ${MIN_CANDIDATES} to ${MAX_CANDIDATES} models`, canPickMany: true });
  if (!picks) return;
  const providers: ProviderConfig[] = [];
  try {
    for (const p of picks) if (p.provider) providers.push(await providerConfig(ctx.secrets, p.provider));
    if (picks.some((x) => x.extra)) {
      const ids = await vscode.window.showInputBox({ title: 'task-x: More models', prompt: `Model ids for ${refLabel(activeRef())}, separated by commas` });
      const base = await providerConfig(ctx.secrets, activeRef());
      for (const id of (ids ?? '').split(',').map((x) => x.trim()).filter(Boolean)) providers.push({ ...base, model: id.slice(0, 256) });
    }
  } catch (e) {
    return void vscode.window.showErrorMessage(`task-x: ${describeError(e)}`);
  }
  if (providers.length < MIN_CANDIDATES || providers.length > MAX_CANDIDATES) {
    return void vscode.window.showWarningMessage(`task-x: pick ${MIN_CANDIDATES} to ${MAX_CANDIDATES} models (you picked ${providers.length}).`);
  }
  const taskId = `n${Date.now()}${randomBytes(3).toString('hex')}`;
  const claim = bg.claim(taskId, `Best of ${providers.length}: ${prompt.trim().slice(0, 40)}`);
  try {
    const client = await getClient();
    const res = await vscode.window.withProgress(
      { location: vscode.ProgressLocation.Notification, title: `task-x: ${providers.length} models are working on the task…`, cancellable: true },
      (_p, token) => {
        const ac = new AbortController();
        token.onCancellationRequested(() => ac.abort());
        return client.request<{ candidates: Candidate[]; costUsd: number }>(
          'agent/bestOf',
          { prompt: prompt.trim(), providers, policy: policy(), limits: limits(), prices: prices(), taskId },
          { signal: ac.signal },
        );
      },
    );
    CompareView.show(ctx, prompt.trim(), res.candidates, chat);
  } catch (e) {
    vscode.window.showErrorMessage(`task-x best of N: ${describeError(e)}`);
  } finally {
    claim.dispose();
  }
}

/** Side-by-side candidates with Diff, Keep (apply one, discard the rest), Open folder, and Discard all. */
class CompareView {
  private resolved = false;

  private constructor(
    private readonly panel: vscode.WebviewPanel,
    private readonly prompt: string,
    private readonly candidates: Candidate[],
    private readonly chat: ChatViewProvider,
  ) {}

  static show(ctx: vscode.ExtensionContext, prompt: string, candidates: Candidate[], chat: ChatViewProvider): void {
    const media = vscode.Uri.joinPath(ctx.extensionUri, 'media');
    const panel = vscode.window.createWebviewPanel('task-x.compare', 'task-x: Compare', vscode.ViewColumn.Active, { enableScripts: true, localResourceRoots: [media] });
    panel.iconPath = vscode.Uri.joinPath(media, 'logo.svg');
    const view = new CompareView(panel, prompt, candidates, chat);
    panel.webview.html = view.html(panel.webview, media);
    panel.webview.onDidReceiveMessage((m: Inbound) => view.onMessage(m), undefined, ctx.subscriptions);
  }

  private post(msg: object): void {
    void this.panel.webview.postMessage(msg);
  }

  private async onMessage(m: Inbound): Promise<void> {
    const c = 'i' in m && Number.isInteger(m.i) ? this.candidates[m.i] : undefined;
    switch (m?.type) {
      case 'ready':
        this.post({ type: 'data', prompt: this.prompt, resolved: this.resolved, candidates: this.candidates.map((x) => ({ ...x, patch: undefined, cost: formatCost(x.costUsd) })) });
        break;
      case 'diff':
        if (c?.patch) await vscode.window.showTextDocument(await vscode.workspace.openTextDocument({ language: 'diff', content: c.patch }), { preview: true, viewColumn: vscode.ViewColumn.Beside });
        break;
      case 'open':
        if (c?.dir && c.name && !this.resolved) await vscode.commands.executeCommand('vscode.openFolder', vscode.Uri.file(c.dir), { forceNewWindow: true });
        break;
      case 'keep':
        if (c?.name && !this.resolved && (await this.chat.settleWorktree(c.name, true))) await this.finish(c.name);
        break;
      case 'discardAll':
        if (!this.resolved) await this.finish(undefined);
        break;
    }
  }

  private async finish(kept: string | undefined): Promise<void> {
    this.resolved = true;
    for (const x of this.candidates) if (x.name && x.name !== kept) await this.chat.settleWorktree(x.name, false);
    this.post({ type: 'resolved', kept });
    vscode.window.showInformationMessage(kept ? 'task-x: kept the chosen result. Review it in the changed-files panel.' : 'task-x: discarded all candidates.');
  }

  private html(webview: vscode.Webview, media: vscode.Uri): string {
    const nonce = randomBytes(16).toString('base64');
    const uri = (f: string) => webview.asWebviewUri(vscode.Uri.joinPath(media, f));
    const csp = `default-src 'none'; style-src ${webview.cspSource}; font-src ${webview.cspSource}; img-src ${webview.cspSource}; script-src 'nonce-${nonce}';`;
    return /* html */ `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="stylesheet" href="${uri('codicons/codicon.css')}">
<link rel="stylesheet" href="${uri('tokens.css')}">
<link rel="stylesheet" href="${uri('compare.css')}">
<title>task-x: Compare</title>
</head>
<body>
<header class="top">
  <div class="brand"><span class="logo" aria-hidden="true"></span><div class="title"><h1>Compare results</h1><p id="prompt"></p></div></div>
  <button type="button" id="discardAll" class="btn secondary">Discard all</button>
</header>
<main id="grid" class="grid" aria-live="polite"></main>
<p class="foot">Each result is in its own git worktree. Keep one to apply it as a reviewable, undoable change; the others are discarded.</p>
<script nonce="${nonce}" src="${uri('compare.js')}"></script>
</body>
</html>`;
  }
}
