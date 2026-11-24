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
  | { type: 'saveMermaid'; code: string }
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
  ): void {
    if (DiagramPanel.current) {
      DiagramPanel.current.panel.reveal();
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
    DiagramPanel.current = view;
    panel.webview.html = view.html(panel.webview, media);
    const sub = panel.webview.onDidReceiveMessage((m: Inbound) => view.onMessage(m, autoGenerate));
    panel.onDidDispose(() => {
      sub.dispose();
      DiagramPanel.current = undefined;
    });
  }

  loadCode(code: string): void {
    const graph = parseMermaid(code);
    this.post({ type: 'load', graph, code: serializeMermaid(graph) });
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
          await this.loadAutoFlow('module', 'LR');
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
      case 'dropFiles': {
        const raw = Array.isArray(m.items) ? m.items.filter((s): s is string => typeof s === 'string' && !!s.trim()) : [];
        if (!raw.length) return;
        const files = raw.map((entry) => {
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
          const uri = vscode.Uri.file(fsPath);
          const rel = vscode.workspace.asRelativePath(uri, false);
          const displayPath = rel && !path.isAbsolute(rel) ? rel : fsPath;
          return { path: displayPath, name: path.basename(fsPath) || displayPath };
        });
        this.post({ type: 'addFileSteps', files, wx: m.wx, wy: m.wy });
        break;
      }
      case 'saveMermaid': {
        if (typeof m.code !== 'string') return;
        const root = WorkspaceSync.root();
        const defaultUri = root ? vscode.Uri.joinPath(vscode.Uri.file(root), 'flow.mmd') : undefined;
        const uri = await vscode.window.showSaveDialog({
          defaultUri,
          filters: { Mermaid: ['mmd', 'mermaid'], Markdown: ['md'] },
          saveLabel: 'Save Flow Diagram',
        });
        if (!uri) return;
        const content = uri.fsPath.endsWith('.md') ? `\`\`\`mermaid\n${m.code.trim()}\n\`\`\`\n` : `${m.code.trim()}\n`;
        await vscode.workspace.fs.writeFile(uri, Buffer.from(content, 'utf8'));
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
    <button type="button" id="autoBtn" class="btn primary" title="Auto-generate flow from workspace modules"><span class="codicon codicon-sparkle" aria-hidden="true"></span><span>Auto Flow</span></button>
    <select id="autoLevel" class="select" aria-label="Auto flow scope">
      <option value="module">Modules</option>
      <option value="file">Files</option>
    </select>
    <button type="button" id="aiBtn" class="btn secondary" title="Ask AI agent to generate a Mermaid flow diagram"><span class="codicon codicon-comment-discussion" aria-hidden="true"></span><span>Ask Agent</span></button>
  </div>
  <span class="sep"></span>
  <div class="bar-group">
    <button type="button" id="addNodeBtn" class="btn secondary" title="Add new node (or double-click canvas)"><span class="codicon codicon-add" aria-hidden="true"></span><span>Node</span></button>
    <button type="button" id="layoutBtn" class="btn icon" title="Auto-layout nodes"><span class="codicon codicon-layout" aria-hidden="true"></span></button>
    <button type="button" id="dirBtn" class="btn secondary" title="Toggle direction (LR / TD)">LR</button>
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
    <button type="button" id="saveBtn" class="btn icon" title="Save .mmd / .md file"><span class="codicon codicon-save" aria-hidden="true"></span></button>
    <button type="button" id="svgBtn" class="btn icon" title="Export SVG"><span class="codicon codicon-export" aria-hidden="true"></span></button>
  </div>
</header>

<div class="workspace">
  <aside class="palette" aria-label="Node shapes">
    <div class="pane-title">Drag or Click</div>
    <button type="button" class="pal-item" draggable="true" data-shape="rect" title="Process rectangle"><span class="pal-glyph rect"></span><span>Process</span></button>
    <button type="button" class="pal-item" draggable="true" data-shape="round" title="Rounded step"><span class="pal-glyph round"></span><span>Step</span></button>
    <button type="button" class="pal-item" draggable="true" data-shape="stadium" title="Start / End"><span class="pal-glyph stadium"></span><span>Start / End</span></button>
    <button type="button" class="pal-item" draggable="true" data-shape="diamond" title="Decision branch"><span class="pal-glyph diamond"></span><span>Decision</span></button>
    <button type="button" class="pal-item" draggable="true" data-shape="cylinder" title="Database / Store"><span class="pal-glyph cylinder"></span><span>Database</span></button>
    <button type="button" class="pal-item" draggable="true" data-shape="subroutine" title="Subroutine / Module"><span class="pal-glyph subroutine"></span><span>Module</span></button>
    <button type="button" class="pal-item" draggable="true" data-shape="trapezoid" title="Input / Output"><span class="pal-glyph trapezoid"></span><span>I/O</span></button>
    <div class="pal-hint">Drag a node's blue dot onto another node to connect. Double-click to edit text. Drop any file onto the canvas to create a step with its path.</div>
  </aside>

  <main class="canvas-wrap" id="canvasWrap">
    <svg id="svg" class="canvas" xmlns="http://www.w3.org/2000/svg">
      <defs>
        <pattern id="grid" width="24" height="24" patternUnits="userSpaceOnUse">
          <circle cx="1" cy="1" r="1" class="grid-dot"></circle>
        </pattern>
        <marker id="arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
          <path d="M 0 1 L 10 5 L 0 9 z" class="arrow-head"></path>
        </marker>
        <marker id="arrow-sel" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse">
          <path d="M 0 1 L 10 5 L 0 9 z" class="arrow-head selected"></path>
        </marker>
      </defs>
      <rect class="grid-bg" x="-4000" y="-4000" width="8000" height="8000" fill="url(#grid)"></rect>
      <g id="viewport">
        <g id="edgesLayer"></g>
        <g id="draftLayer"></g>
        <g id="nodesLayer"></g>
      </g>
    </svg>
    <input id="inlineEdit" class="inline-edit" type="text" hidden aria-label="Edit text">
    <div id="statusBar" class="status-bar" role="status" aria-live="polite"></div>
  </main>

  <aside class="inspector" id="inspector" aria-label="Properties">
    <div class="pane-title">Inspector</div>
    <div id="inspEmpty" class="insp-empty">Select a node or edge to edit its text, shape, style, or color.</div>
    <form id="inspNode" class="insp-form" hidden>
      <label class="field"><span>Label</span><input id="nodeLabel" type="text" placeholder="Node text"></label>
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
    </form>
    <form id="inspEdge" class="insp-form" hidden>
      <label class="field"><span>Edge Label</span><input id="edgeLabel" type="text" placeholder="Optional label"></label>
      <label class="field"><span>Line Style</span>
        <select id="edgeStyle">
          <option value="solid">Solid (--&gt;)</option>
          <option value="dashed">Dashed (-.->)</option>
          <option value="thick">Thick (==&gt;)</option>
          <option value="open">Open line (---)</option>
        </select>
      </label>
    </form>
  </aside>
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

<script nonce="${nonce}" src="${uri('diagram.js')}"></script>
</body>
</html>`;
  }
}
