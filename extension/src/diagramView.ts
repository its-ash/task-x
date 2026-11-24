import path from 'node:path';
import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { describeError } from './chatView.js';
import { type FlowDirection, type FlowGraph, graphDataToFlow, parseMermaid, serializeMermaid } from './diagramCodec.js';
import type { GraphView as GraphData } from './protocol.js';
import type { JsonRpcClient } from './rpc.js';
import { WorkspaceSync } from './workspace.js';

type Inbound =
  | { type: 'ready' }
  | { type: 'autoFlow'; level?: 'module' | 'file'; direction?: FlowDirection; focus?: string }
  | { type: 'askAgent'; prompt: string }
  | { type: 'openMermaid' }
  | { type: 'refreshFile' }
  | { type: 'saveMermaid'; code: string; saveAs?: boolean }
  | { type: 'exportSvg'; svg: string }
  | { type: 'sendToChat'; code: string }
  | { type: 'copyText'; text: string }
  | { type: 'dropFiles'; items: string[]; wx: number; wy: number };

const DEFAULT_FLOW = `flowchart LR
    Start(["Start"]) --> Process["Process Step"]
    Process --> Check{"Valid?"}
    Check -->|"Yes"| Save[("Save Result")]
    Check -.->|"No"| Retry["Handle Error"]`;

export class DiagramPanel {
  private static current: DiagramPanel | undefined;
  private pendingCode: string | undefined;
  private currentFile: vscode.Uri | undefined;

  private constructor(
    private readonly panel: vscode.WebviewPanel,
    private readonly getClient: () => Promise<JsonRpcClient>,
    private readonly onSendToChat?: (text: string, submit?: boolean) => void,
  ) {}

  static show(
    ctx: vscode.ExtensionContext,
    getClient: () => Promise<JsonRpcClient>,
    onSendToChat?: (text: string, submit?: boolean) => void,
    initialCode?: string,
    autoGenerate = false,
    initialFile?: vscode.Uri,
  ): void {
    if (DiagramPanel.current) {
      DiagramPanel.current.panel.reveal();
      if (initialFile) DiagramPanel.current.setCurrentFile(initialFile);
      if (initialCode) DiagramPanel.current.loadCode(initialCode);
      else if (autoGenerate) void DiagramPanel.current.loadAutoFlow('module', 'LR');
      return;
    }
    const media = vscode.Uri.joinPath(ctx.extensionUri, 'media');
    const panel = vscode.window.createWebviewPanel('task-x.diagram', 'task-x: Flow Editor', vscode.ViewColumn.Active, {
      enableScripts: true,
      retainContextWhenHidden: true,
      localResourceRoots: [media],
    });
    panel.iconPath = vscode.Uri.joinPath(media, 'icon.png');
    const view = new DiagramPanel(panel, getClient, onSendToChat);
    view.pendingCode = initialCode;
    if (initialFile) view.setCurrentFile(initialFile);
    DiagramPanel.current = view;
    panel.webview.html = view.html(panel.webview, media);
    const sub = panel.webview.onDidReceiveMessage((m: Inbound) => view.onMessage(m, autoGenerate));
    panel.onDidDispose(() => {
      sub.dispose();
      DiagramPanel.current = undefined;
    });
  }

  private setCurrentFile(uri: vscode.Uri | undefined): void {
    this.currentFile = uri;
    this.panel.title = uri ? `task-x: ${path.basename(uri.fsPath)}` : 'task-x: Flow Editor';
  }

  loadCode(code: string): void {
    const trimmed = code.trim();
    const graph = parseMermaid(trimmed);
    this.post({ type: 'load', graph, code: trimmed || serializeMermaid(graph) });
  }

  private async openFile(uri: vscode.Uri, refreshed = false): Promise<void> {
    try {
      const bytes = await vscode.workspace.fs.readFile(uri);
      let text = Buffer.from(bytes).toString('utf8');
      const fenced = /```(?:mermaid|mmd)\s*\r?\n([\s\S]*?)```/i.exec(text);
      if (fenced) text = fenced[1];
      this.setCurrentFile(uri);
      this.loadCode(text);
      this.post({ type: 'status', text: `${refreshed ? 'Refreshed' : 'Opened'} ${vscode.workspace.asRelativePath(uri)}`, busy: false });
    } catch (e) {
      this.post({ type: 'status', text: `Could not open file: ${describeError(e)}`, busy: false });
    }
  }

  private post(msg: object): void {
    void this.panel.webview.postMessage(msg);
  }

  private async loadAutoFlow(level: 'module' | 'file' = 'module', direction: FlowDirection = 'LR', focus?: string): Promise<void> {
    this.post({ type: 'status', text: 'Generating workspace flow…', busy: true });
    try {
      const client = await this.getClient();
      const data = await client.request<GraphData>('graph/project', {
        level,
        focus: focus?.trim() || undefined,
        depth: focus?.trim() ? 2 : undefined,
        importsOnly: false,
        maxNodes: 24,
      });
      if (!data.nodes.length) {
        const graph = parseMermaid(DEFAULT_FLOW);
        this.post({ type: 'load', graph, code: serializeMermaid(graph) });
        this.post({ type: 'status', text: 'No indexed nodes yet; loaded starter flow.', busy: false });
        return;
      }
      const graph: FlowGraph = graphDataToFlow(data, direction, 24);
      this.post({ type: 'load', graph, code: serializeMermaid(graph) });
      this.post({ type: 'status', text: `Auto-generated ${graph.nodes.length} nodes, ${graph.edges.length} edges`, busy: false });
    } catch (e) {
      const graph = parseMermaid(DEFAULT_FLOW);
      this.post({ type: 'load', graph, code: serializeMermaid(graph) });
      this.post({ type: 'status', text: `Auto flow unavailable (${describeError(e)})`, busy: false });
    }
  }

  private async onMessage(m: Inbound, autoGenerate: boolean): Promise<void> {
    switch (m?.type) {
      case 'ready': {
        if (this.pendingCode) {
          this.loadCode(this.pendingCode);
          this.pendingCode = undefined;
        } else if (autoGenerate) {
          await this.loadAutoFlow('module', 'LR');
        } else {
          this.post({ type: 'load', code: '' });
        }
        break;
      }
      case 'autoFlow':
        await this.loadAutoFlow(m.level === 'file' ? 'file' : 'module', m.direction === 'TD' ? 'TD' : 'LR', m.focus);
        break;
      case 'askAgent': {
        const topic = typeof m.prompt === 'string' ? m.prompt.trim() : '';
        if (!topic || !this.onSendToChat) return;
        const req = `Create a clear Mermaid flowchart diagram (\`\`\`mermaid) for: ${topic}. Use concise node labels and meaningful edge labels.`;
        this.onSendToChat(req, true);
        void vscode.commands.executeCommand('task-x.chat.focus');
        break;
      }
      case 'sendToChat': {
        if (typeof m.code !== 'string' || !this.onSendToChat) return;
        this.onSendToChat(`\`\`\`mermaid\n${m.code.trim()}\n\`\`\``, false);
        void vscode.commands.executeCommand('task-x.chat.focus');
        break;
      }
      case 'copyText':
        if (typeof m.text === 'string') {
          await vscode.env.clipboard.writeText(m.text);
          this.post({ type: 'status', text: 'Copied to clipboard', busy: false });
        }
        break;
      case 'openMermaid': {
        const picked = await vscode.window.showOpenDialog({
          canSelectMany: false,
          filters: { Mermaid: ['mmd', 'mermaid', 'md'] },
          openLabel: 'Open Flow Diagram',
        });
        if (!picked?.[0]) return;
        await this.openFile(picked[0]);
        break;
      }
      case 'refreshFile': {
        if (this.currentFile) {
          await this.openFile(this.currentFile, true);
        } else {
          this.post({ type: 'rerender' });
        }
        break;
      }
      case 'dropFiles': {
        const raw = Array.isArray(m.items) ? m.items.filter((s): s is string => typeof s === 'string' && !!s.trim()) : [];
        if (!raw.length) return;
        const toUri = (entry: string): { uri: vscode.Uri; fsPath: string } => {
          const s = entry.trim();
          let fsPath = s;
          if (/^file:\/\//i.test(s) || /^[a-z][a-z0-9+.-]*:\/\//i.test(s)) {
            try {
              const u = vscode.Uri.parse(s);
              fsPath = u.scheme === 'file' ? u.fsPath : u.path || s;
            } catch {
              // Keep raw string.
            }
          }
          return { uri: vscode.Uri.file(fsPath), fsPath };
        };
        if (raw.length === 1) {
          const { uri, fsPath } = toUri(raw[0]);
          if (/\.(?:mmd|mermaid)$/i.test(fsPath)) {
            await this.openFile(uri);
            return;
          }
        }
        const files = raw.map((entry) => {
          const { uri, fsPath } = toUri(entry);
          const rel = vscode.workspace.asRelativePath(uri, false);
          const displayPath = rel && !path.isAbsolute(rel) ? rel : fsPath;
          return { path: displayPath, name: path.basename(fsPath) || displayPath };
        });
        this.post({ type: 'addFileSteps', files, wx: m.wx, wy: m.wy });
        break;
      }
      case 'saveMermaid': {
        if (typeof m.code !== 'string') return;
        let uri = !m.saveAs ? this.currentFile : undefined;
        if (!uri) {
          const root = WorkspaceSync.root();
          const defaultUri = this.currentFile ?? (root ? vscode.Uri.joinPath(vscode.Uri.file(root), 'flow.mmd') : undefined);
          uri = await vscode.window.showSaveDialog({
            defaultUri,
            filters: { Mermaid: ['mmd', 'mermaid'], Markdown: ['md'] },
            saveLabel: 'Save Flow Diagram',
          });
          if (!uri) return;
          this.setCurrentFile(uri);
        }
        const content = uri.fsPath.endsWith('.md') ? `\`\`\`mermaid\n${m.code.trim()}\n\`\`\`\n` : `${m.code.trim()}\n`;
        await vscode.workspace.fs.writeFile(uri, Buffer.from(content, 'utf8'));
        this.post({ type: 'saved' });
        this.post({ type: 'status', text: `Saved ${vscode.workspace.asRelativePath(uri)}`, busy: false });
        break;
      }
      case 'exportSvg': {
        if (typeof m.svg !== 'string') return;
        const root = WorkspaceSync.root();
        const defaultUri = root ? vscode.Uri.joinPath(vscode.Uri.file(root), 'flow.svg') : undefined;
        const uri = await vscode.window.showSaveDialog({
          defaultUri,
          filters: { SVG: ['svg'] },
          saveLabel: 'Export SVG',
        });
        if (!uri) return;
        await vscode.workspace.fs.writeFile(uri, Buffer.from(m.svg, 'utf8'));
        this.post({ type: 'status', text: `Exported ${vscode.workspace.asRelativePath(uri)}`, busy: false });
        break;
      }
    }
  }

  private html(webview: vscode.Webview, media: vscode.Uri): string {
    const nonce = randomBytes(16).toString('base64');
    const uri = (f: string) => webview.asWebviewUri(vscode.Uri.joinPath(media, f));
    const csp = `default-src 'none'; style-src ${webview.cspSource} 'unsafe-inline'; font-src ${webview.cspSource}; img-src ${webview.cspSource} data:; script-src 'nonce-${nonce}';`;
    return /* html */ `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="stylesheet" href="${uri('codicons/codicon.css')}">
<link rel="stylesheet" href="${uri('tokens.css')}">
<link rel="stylesheet" href="${uri('diagram.css')}">
<title>Flow Editor</title>
</head>
<body>
<header class="bar">
  <div class="bar-group">
    <button type="button" id="autoBtn" class="btn primary" title="Refine and auto-arrange the opened flow diagram (or generate from workspace when empty)"><span class="codicon codicon-sparkle" aria-hidden="true"></span><span>Auto Flow</span></button>
    <button type="button" id="aiBtn" class="btn secondary" title="Ask AI agent to generate a Mermaid flow diagram"><span class="codicon codicon-comment-discussion" aria-hidden="true"></span><span>Ask Agent</span></button>
  </div>
  <span class="sep"></span>
  <div class="bar-group">
    <button type="button" id="addNodeBtn" class="btn secondary" title="Add new card (or double-click canvas)"><span class="codicon codicon-add" aria-hidden="true"></span><span>Card</span></button>
    <button type="button" id="layoutBtn" class="btn icon" title="Auto-arrange step-by-step layout"><span class="codicon codicon-layout" aria-hidden="true"></span></button>
    <button type="button" id="dirBtn" class="btn secondary" title="Toggle flow direction (LR / TD)">LR</button>
    <div class="theme-switch" role="group" aria-label="Theme">
      <button type="button" class="theme-opt" data-theme="dark" aria-pressed="true"><span class="codicon codicon-color-mode" aria-hidden="true"></span><span>Dark</span></button>
      <button type="button" class="theme-opt" data-theme="light" aria-pressed="false"><span>Light</span></button>
    </div>
    <button type="button" id="undoBtn" class="btn icon" title="Undo (Cmd/Ctrl+Z)" disabled><span class="codicon codicon-discard" aria-hidden="true"></span></button>
    <button type="button" id="redoBtn" class="btn icon" title="Redo (Cmd/Ctrl+Shift+Z)" disabled><span class="codicon codicon-redo" aria-hidden="true"></span></button>
    <button type="button" id="delBtn" class="btn icon" title="Delete selected (Del)" disabled><span class="codicon codicon-trash" aria-hidden="true"></span></button>
  </div>
  <span class="spacer"></span>
  <div class="bar-group">
    <button type="button" id="zoomOutBtn" class="btn icon" title="Zoom out"><span class="codicon codicon-zoom-out" aria-hidden="true"></span></button>
    <span id="zoomVal" class="zoom-val">100%</span>
    <button type="button" id="zoomInBtn" class="btn icon" title="Zoom in"><span class="codicon codicon-zoom-in" aria-hidden="true"></span></button>
    <button type="button" id="fitBtn" class="btn icon" title="Fit diagram to view"><span class="codicon codicon-screen-normal" aria-hidden="true"></span></button>
    <span class="sep"></span>
    <button type="button" id="codeToggleBtn" class="btn secondary" title="Toggle Mermaid code panel"><span class="codicon codicon-code" aria-hidden="true"></span><span>Mermaid</span></button>
    <button type="button" id="copyBtn" class="btn icon" title="Copy Mermaid code"><span class="codicon codicon-copy" aria-hidden="true"></span></button>
    <button type="button" id="chatBtn" class="btn icon" title="Insert Mermaid into Chat"><span class="codicon codicon-send" aria-hidden="true"></span></button>
    <button type="button" id="openBtn" class="btn icon" title="Open .mmd / .mermaid file"><span class="codicon codicon-folder-opened" aria-hidden="true"></span></button>
    <button type="button" id="refreshBtn" class="btn icon" title="Refresh from opened file"><span class="codicon codicon-refresh" aria-hidden="true"></span></button>
    <button type="button" id="saveBtn" class="btn icon" title="Save file (Cmd/Ctrl+S)"><span class="codicon codicon-save" aria-hidden="true"></span></button>
    <button type="button" id="svgBtn" class="btn icon" title="Export SVG"><span class="codicon codicon-export" aria-hidden="true"></span></button>
  </div>
</header>

<div class="workspace">
  <main class="canvas-wrap" id="canvasWrap">
    <div id="stage" class="stage">
      <div id="mermaidHost" class="mermaid-host"></div>
    </div>
    <div id="emptyState" class="empty-state" hidden>
      <div class="empty-card">
        <span class="codicon codicon-file-code empty-icon" aria-hidden="true"></span>
        <div class="empty-title">Open a Mermaid file</div>
        <div class="empty-sub">Open a .mmd or .mermaid file, drop one onto the canvas, or add a card to start.</div>
        <button type="button" id="emptyOpenBtn" class="btn primary"><span class="codicon codicon-folder-opened" aria-hidden="true"></span><span>Open File</span></button>
      </div>
    </div>
    <div id="statusBar" class="status-bar" role="status" aria-live="polite"></div>

    <button type="button" id="fabAdd" class="fab-add" title="Add new card" aria-label="Add new card" hidden><span class="codicon codicon-add" aria-hidden="true"></span><span>Add Card</span></button>
    <aside class="floating-card palette" id="palette" aria-label="Add card">
      <div class="float-head">
        <span class="pane-title">Add Card</span>
        <button type="button" id="closePaletteBtn" class="btn icon small-icon" title="Hide Add Card panel" aria-label="Hide Add Card panel"><span class="codicon codicon-close" aria-hidden="true"></span></button>
      </div>
      <div class="pal-list">
        <button type="button" class="pal-item" draggable="true" data-shape="rect" title="Process rectangle"><span class="pal-glyph rect"></span><span>Process</span></button>
        <button type="button" class="pal-item" draggable="true" data-shape="round" title="Rounded step"><span class="pal-glyph round"></span><span>Step</span></button>
        <button type="button" class="pal-item" draggable="true" data-shape="stadium" title="Start / End"><span class="pal-glyph stadium"></span><span>Start / End</span></button>
        <button type="button" class="pal-item" draggable="true" data-shape="diamond" title="Decision branch"><span class="pal-glyph diamond"></span><span>Decision</span></button>
        <button type="button" class="pal-item" draggable="true" data-shape="cylinder" title="Database / Store"><span class="pal-glyph cylinder"></span><span>Database</span></button>
        <button type="button" class="pal-item" draggable="true" data-shape="subroutine" title="Subroutine / Module"><span class="pal-glyph subroutine"></span><span>Module</span></button>
        <button type="button" class="pal-item" draggable="true" data-shape="trapezoid" title="Input / Output"><span class="pal-glyph trapezoid"></span><span>I/O</span></button>
      </div>
      <div class="pal-hint">Click or drop a shape to add a card. Double-click a card to rename, Shift+click another card to link, or click an arrow to edit it.</div>
    </aside>

    <aside class="floating-card inspector" id="inspector" aria-label="Edit card" hidden>
      <div class="float-head">
        <span class="pane-title" id="inspTitle">Edit Card</span>
        <span class="spacer"></span>
        <button type="button" id="inspDelBtn" class="btn icon small-icon danger" title="Delete selected" aria-label="Delete selected"><span class="codicon codicon-trash" aria-hidden="true"></span></button>
        <button type="button" id="closeInspBtn" class="btn icon small-icon" title="Close" aria-label="Close"><span class="codicon codicon-close" aria-hidden="true"></span></button>
      </div>
      <div id="inspEmpty" class="insp-empty" hidden>Click any card or arrow to edit it.</div>
      <form id="inspNode" class="insp-form" hidden>
        <label class="field"><span>Label</span><textarea id="nodeLabel" rows="2" placeholder="Card text"></textarea></label>
        <label class="field"><span>ID</span><input id="nodeId" type="text" placeholder="Node_1"></label>
        <label class="field"><span>Shape</span>
          <select id="nodeShape">
            <option value="rect">Process [ ]</option>
            <option value="round">Step ( )</option>
            <option value="stadium">Start / End ([ ])</option>
            <option value="diamond">Decision { }</option>
            <option value="cylinder">Database [( )]</option>
            <option value="subroutine">Module [[ ]]</option>
            <option value="trapezoid">I/O [/ /]</option>
          </select>
        </label>
        <label class="field"><span>Accent</span>
          <div class="swatches" id="nodeColors">
            <button type="button" class="swatch none" data-color="" title="Default"></button>
            <button type="button" class="swatch" data-color="#4ea8ff" style="--c:#4ea8ff" title="Blue"></button>
            <button type="button" class="swatch" data-color="#76daa1" style="--c:#76daa1" title="Green"></button>
            <button type="button" class="swatch" data-color="#edc21f" style="--c:#edc21f" title="Amber"></button>
            <button type="button" class="swatch" data-color="#ff8a65" style="--c:#ff8a65" title="Orange"></button>
            <button type="button" class="swatch" data-color="#d291ff" style="--c:#d291ff" title="Purple"></button>
          </div>
        </label>
        <div class="insp-actions">
          <button type="button" id="addNextBtn" class="btn small secondary"><span class="codicon codicon-add" aria-hidden="true"></span><span>Next Step</span></button>
          <button type="button" id="dupNodeBtn" class="btn small secondary"><span class="codicon codicon-copy" aria-hidden="true"></span><span>Duplicate</span></button>
        </div>
        <div class="field">
          <span>Connect to card</span>
          <div class="connect-row">
            <select id="connectTarget" aria-label="Target card"></select>
            <select id="connectArrow" class="arrow-sel" aria-label="Arrow style">
              <option value="-->">Solid</option>
              <option value="-.->">Dashed</option>
              <option value="==>">Thick</option>
            </select>
            <button type="button" id="connectBtn" class="btn small primary">Link</button>
          </div>
          <input id="connectLabel" type="text" placeholder="Optional edge label">
        </div>
        <div class="field" id="nodeEdgesWrap" hidden>
          <span>Connections</span>
          <div id="nodeEdges" class="edge-list"></div>
        </div>
      </form>
      <form id="inspEdge" class="insp-form" hidden>
        <div class="edge-route" id="edgeRoute"></div>
        <label class="field"><span>Label</span><input id="edgeLabel" type="text" placeholder="Edge label"></label>
        <label class="field"><span>Style</span>
          <select id="edgeStyle">
            <option value="-->">Solid (--&gt;)</option>
            <option value="-.->">Dashed (-.-&gt;)</option>
            <option value="==>">Thick (==&gt;)</option>
          </select>
        </label>
        <div class="insp-actions">
          <button type="button" id="edgeReverseBtn" class="btn small secondary"><span class="codicon codicon-arrow-swap" aria-hidden="true"></span><span>Reverse</span></button>
        </div>
      </form>
    </aside>
  </main>
</div>

<section class="code-drawer" id="codeDrawer" hidden aria-label="Mermaid source">
  <div class="code-drawer-head">
    <span>Mermaid Source</span>
    <span class="spacer"></span>
    <button type="button" id="applyCodeBtn" class="btn small primary">Apply to Canvas</button>
    <button type="button" id="closeCodeBtn" class="btn icon" title="Hide code"><span class="codicon codicon-close" aria-hidden="true"></span></button>
  </div>
  <textarea id="mermaidText" spellcheck="false" aria-label="Mermaid diagram source"></textarea>
</section>

<dialog id="aiDialog" class="ai-dialog" aria-label="Generate Flow with Agent">
  <form method="dialog" id="aiForm" class="ai-form">
    <div class="ai-head"><span class="codicon codicon-sparkle" aria-hidden="true"></span><span>Generate Flow Diagram with Agent</span></div>
    <textarea id="aiPrompt" rows="3" placeholder="Describe the flow (e.g. 'Auth login and token refresh flow' or 'How checkout calls Stripe webhook')"></textarea>
    <div class="ai-foot">
      <button type="button" id="aiCancel" class="btn secondary">Cancel</button>
      <button type="submit" class="btn primary">Send to Agent</button>
    </div>
  </form>
</dialog>

<script nonce="${nonce}" src="${uri('vendor/mermaid.min.js')}"></script>
<script nonce="${nonce}" src="${uri('diagram.js')}"></script>
</body>
</html>`;
  }
}
