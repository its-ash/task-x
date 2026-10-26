import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';

export type Level = 'trace' | 'debug' | 'info' | 'warn' | 'error';
export interface LogEntry {
  t: number;
  level: Level;
  src: string;
  msg: string;
}

const MAX_ENTRIES = 5_000;
const MAX_MSG_CHARS = 4_000;
const LEVELS: Level[] = ['trace', 'debug', 'info', 'warn', 'error'];

/** Infers the level of a runtime stderr line (tracing format: "... WARN target: msg"). */
export const levelOf = (line: string): Level => (/\bERROR\b/.test(line) ? 'error' : /\bWARN\b/.test(line) ? 'warn' : /\bDEBUG\b/.test(line) ? 'debug' : /\bTRACE\b/.test(line) ? 'trace' : 'info');

/** The Logs panel beside the terminal: a filterable, live view of extension, runtime, and agent logs. */
export class LogView implements vscode.WebviewViewProvider {
  static readonly viewId = 'task-x.logs';
  private view?: vscode.WebviewView;
  private entries: LogEntry[] = [];
  private pending: LogEntry[] = [];
  private timer: NodeJS.Timeout | undefined;

  constructor(
    private readonly ctx: vscode.ExtensionContext,
    private readonly showOutput: () => void,
  ) {}

  add(level: Level, src: string, msg: string): void {
    const e = { t: Date.now(), level, src, msg: msg.length > MAX_MSG_CHARS ? `${msg.slice(0, MAX_MSG_CHARS)}…` : msg };
    this.entries.push(e);
    if (this.entries.length > MAX_ENTRIES) this.entries.splice(0, this.entries.length - MAX_ENTRIES);
    if (!this.view?.visible) return;
    this.pending.push(e);
    this.timer ??= setTimeout(() => {
      this.timer = undefined;
      void this.view?.webview.postMessage({ type: 'append', entries: this.pending });
      this.pending = [];
    }, 100);
  }

  clear(): void {
    this.entries = [];
    this.pending = [];
    void this.view?.webview.postMessage({ type: 'reset', entries: [] });
  }

  text(): string {
    return this.entries.map((e) => `${new Date(e.t).toISOString()} [${e.level}] [${e.src}] ${e.msg}`).join('\n');
  }

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    const media = vscode.Uri.joinPath(this.ctx.extensionUri, 'media');
    view.webview.options = { enableScripts: true, localResourceRoots: [media] };
    view.webview.html = this.html(view.webview, media);
    const reset = () => {
      this.pending = [];
      void view.webview.postMessage({ type: 'reset', entries: this.entries });
    };
    view.onDidChangeVisibility(() => view.visible && reset());
    view.webview.onDidReceiveMessage(async (m: { type?: string }) => {
      if (m?.type === 'ready') reset();
      else if (m?.type === 'clear') this.clear();
      else if (m?.type === 'copy') await vscode.env.clipboard.writeText(this.text());
      else if (m?.type === 'output') this.showOutput();
    });
  }

  private html(webview: vscode.Webview, media: vscode.Uri): string {
    const nonce = randomBytes(16).toString('base64');
    const uri = (f: string) => webview.asWebviewUri(vscode.Uri.joinPath(media, f));
    const csp = `default-src 'none'; style-src ${webview.cspSource}; font-src ${webview.cspSource}; script-src 'nonce-${nonce}';`;
    const tabs = LEVELS.map((l) => `<button type="button" role="tab" class="tab" data-level="${l}" aria-selected="false" tabindex="-1">${l[0].toUpperCase() + l.slice(1)}</button>`).join('');
    return /* html */ `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width, initial-scale=1">
<link rel="stylesheet" href="${uri('codicons/codicon.css')}">
<link rel="stylesheet" href="${uri('tokens.css')}">
<link rel="stylesheet" href="${uri('logs.css')}">
<title>task-x Logs</title>
</head>
<body>
<div class="bar">
  <span class="field"><i class="codicon codicon-filter"></i><input id="q" type="search" placeholder="Filter logs" aria-label="Filter logs" maxlength="200"></span>
  <div id="level" class="tabs" role="tablist" aria-label="Minimum level"><span class="tab-ind"></span>${tabs}</div>
  <select id="src" aria-label="Source"><option value="">All sources</option></select>
  <span id="count" class="count"></span>
  <span class="spacer"></span>
  <button id="follow" class="action on" title="Follow new logs" aria-pressed="true"><i class="codicon codicon-arrow-down"></i></button>
  <button id="wrap" class="action" title="Wrap lines" aria-pressed="false"><i class="codicon codicon-word-wrap"></i></button>
  <button id="copy" class="action" title="Copy all logs"><i class="codicon codicon-copy"></i></button>
  <button id="output" class="action" title="Open Output channel"><i class="codicon codicon-output"></i></button>
  <button id="clear" class="action" title="Clear logs"><i class="codicon codicon-clear-all"></i></button>
</div>
<div id="log" role="log" aria-live="polite" tabindex="0"></div>
<script nonce="${nonce}" src="${uri('logs.js')}"></script>
</body>
</html>`;
  }
}
