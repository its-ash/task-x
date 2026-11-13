import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { describeError } from './chatView.js';
import type { FileDependencies, GraphView as GraphData } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';
import { WorkspaceSync } from './workspace.js';

const MAX_NODES = 600;

type Inbound =
  | { type: 'ready' }
  | { type: 'load'; level: 'file' | 'module'; focus?: string; importsOnly?: boolean; maxNodes?: number }
  | { type: 'select'; id: string }
  | { type: 'open'; path: string }
  | { type: 'reindex' };

/** The project graph panel: files or modules as a force-directed graph, with dependencies of the selected file. */
export class GraphPanel {
  private static current: GraphPanel | undefined;
  private last: Extract<Inbound, { type: 'load' }> = { type: 'load', level: 'module' };
  private sync: WorkspaceSync | undefined;

  private constructor(
    private readonly panel: vscode.WebviewPanel,
    private readonly getClient: () => Promise<JsonRpcClient>,
  ) {}

  static show(ctx: vscode.ExtensionContext, getClient: () => Promise<JsonRpcClient>, sync: WorkspaceSync, focus?: string): void {
    if (GraphPanel.current) {
      GraphPanel.current.panel.reveal();
      if (focus) void GraphPanel.current.load({ type: 'load', level: 'file', focus });
      return;
    }
    const media = vscode.Uri.joinPath(ctx.extensionUri, 'media');
    const panel = vscode.window.createWebviewPanel('task-x.graph', 'task-x: Project Graph', vscode.ViewColumn.Active, { enableScripts: true, retainContextWhenHidden: true, localResourceRoots: [media] });
    panel.iconPath = vscode.Uri.joinPath(media, 'logo.svg');
    const view = new GraphPanel(panel, getClient);
    if (focus) view.last = { type: 'load', level: 'file', focus };
    GraphPanel.current = view;
    panel.webview.html = view.html(panel.webview, media);
    view.sync = sync;
    const subs = [
      panel.webview.onDidReceiveMessage((m: Inbound) => view.onMessage(m)),
      // Index progress shows in the panel; a finished re-index changes the graph, so redraw it.
      sync.onState((s) => {
        view.post({ type: 'index', index: s });
        if (s.state === 'ready' && s.graph) void view.load(view.last);
      }),
    ];
    panel.onDidDispose(() => {
      subs.forEach((d) => d.dispose());
      GraphPanel.current = undefined;
    });
  }

  private post(msg: object): void {
    void this.panel.webview.postMessage(msg);
  }

  private async load(m: Extract<Inbound, { type: 'load' }>): Promise<void> {
    this.last = m;
    this.post({ type: 'loading', level: m.level, focus: m.focus ?? '' });
    try {
      const client = await this.getClient();
      const focus = m.focus?.trim() || undefined;
      const data = await client.request<GraphData>('graph/project', {
        level: m.level === 'module' ? 'module' : 'file',
        focus,
        depth: focus ? 2 : undefined,
        importsOnly: !!m.importsOnly,
        maxNodes: Math.min(MAX_NODES, Math.max(10, Math.floor(m.maxNodes ?? 250))),
      });
      this.post({ type: 'graph', level: m.level, focus: focus ?? '', importsOnly: !!m.importsOnly, data });
    } catch (e) {
      this.post({ type: 'error', text: describeError(e) });
    }
  }

  private async onMessage(m: Inbound): Promise<void> {
    switch (m?.type) {
      case 'ready':
        this.post({ type: 'index', index: this.sync?.state ?? { state: 'none' } });
        await this.load(this.last);
        break;
      case 'reindex':
        if (this.sync?.state.state !== 'indexing') await vscode.commands.executeCommand('task-x.reindex');
        break;
      case 'load':
        await this.load(m);
        break;
      case 'select':
        if (typeof m.id !== 'string') return;
        try {
          const deps = await (await this.getClient()).request<FileDependencies>('graph/file', { path: m.id });
          this.post({ type: 'details', deps });
        } catch {
          this.post({ type: 'details', deps: null });
        }
        break;
      case 'open': {
        const root = WorkspaceSync.root();
        if (!root || typeof m.path !== 'string' || !m.path || m.path.split('/').includes('..')) return;
        const uri = vscode.Uri.joinPath(vscode.Uri.file(root), ...m.path.split('/'));
        const isFile = await vscode.workspace.fs.stat(uri).then((s) => s.type === vscode.FileType.File, () => false);
        if (isFile) await vscode.window.showTextDocument(uri, { preview: true, viewColumn: vscode.ViewColumn.Beside });
        break;
      }
    }
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
<link rel="stylesheet" href="${uri('graph.css')}">
<title>Project Graph</title>
</head>
<body>
<header class="top">
  <div class="brand"><span class="logo" aria-hidden="true"></span><div><h1>Project graph</h1><p id="summary" class="muted">Loading…</p></div></div>
  <div class="controls">
    <div id="level" class="tabs" role="tablist" aria-label="Level"><span class="tab-ind"></span><button type="button" role="tab" class="tab" data-level="module">Modules</button><button type="button" role="tab" class="tab" data-level="file">Files</button></div>
    <label class="check"><input id="imports" type="checkbox"> Imports only</label>
    <label class="field"><span class="codicon codicon-search" aria-hidden="true"></span><input id="focus" type="search" placeholder="Focus on a file or folder" aria-label="Focus on a file or folder" maxlength="300"></label>
    <button type="button" id="reindex" class="btn secondary" title="Scan the workspace again and rebuild the graph"><span class="codicon codicon-refresh" aria-hidden="true"></span><span id="reindexText">Reindex</span></button>
  </div>
</header>
<div id="chips" class="chips" aria-live="polite"></div>
<main class="stage">
  <canvas id="canvas" aria-label="Dependency graph. Use the list on the right to move between files."></canvas>
  <div id="empty" class="empty" hidden></div>
  <aside id="side" class="side" aria-live="polite"></aside>
</main>
<script nonce="${nonce}" src="${uri('graph.js')}"></script>
</body>
</html>`;
  }
}
