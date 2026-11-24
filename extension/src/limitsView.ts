import { randomBytes } from 'node:crypto';
import * as vscode from 'vscode';
import { type AgyCli, findAgy } from './agyCli.js';
import { formatCredits, resetAt, resetIn } from './agyUsageFormat.js';
import { findClaude } from './claudeCode.js';
import type { ClaudeUsage } from './claudeUsage.js';

interface LimitRow {
  label: string;
  usedPct: number;
  remainingPct: number;
  resetAt?: string;
  resetIn?: string;
}

interface LimitGroup {
  title: string;
  subtitle?: string;
  rows: LimitRow[];
  extra?: string;
  empty?: string;
}

interface LimitsPayload {
  loading: boolean;
  groups: LimitGroup[];
}

export class LimitsView implements vscode.WebviewViewProvider, vscode.Disposable {
  static readonly viewId = 'task-x.limits';
  private view?: vscode.WebviewView;
  private loading = false;
  private readonly subs: vscode.Disposable[] = [];

  constructor(
    private readonly agy: AgyCli,
    private readonly claude: ClaudeUsage,
  ) {
    this.subs.push(
      agy.onDidChange(() => this.post()),
      claude.onDidChange(() => this.post()),
      vscode.commands.registerCommand('task-x.limits.refresh', () => void this.refresh()),
    );
  }

  resolveWebviewView(view: vscode.WebviewView): void {
    this.view = view;
    view.webview.options = { enableScripts: true };
    view.webview.html = this.html(view.webview);
    view.webview.onDidReceiveMessage((m: { type?: string }) => {
      if (m.type === 'ready') this.post();
      else if (m.type === 'refresh') void this.refresh();
    }, undefined, this.subs);
    view.onDidChangeVisibility(() => {
      if (view.visible) void this.refresh(false);
    }, undefined, this.subs);
    this.post();
  }

  async refresh(force = true): Promise<void> {
    this.loading = true;
    this.post();
    try {
      await Promise.allSettled([
        findAgy() ? this.agy.refreshUsage(force) : Promise.resolve(),
        findClaude() ? this.claude.refreshUsage(force) : Promise.resolve(),
      ]);
    } finally {
      this.loading = false;
      this.post();
    }
  }

  private snapshot(): LimitsPayload {
    const groups: LimitGroup[] = [];
    const now = Date.now();

    if (findAgy()) {
      const u = this.agy.usage;
      if (u && u.groups.length) {
        for (const g of u.groups) {
          const buckets = [...g.buckets].sort((a, b) => (a.window === '5h' ? -1 : b.window === '5h' ? 1 : 0));
          const rows: LimitRow[] = buckets.map((b) => {
            const rem = Math.max(0, Math.min(1, Number.isFinite(b.remainingFraction) ? b.remainingFraction : 0));
            const remainingPct = Math.round(rem * 100);
            const is5h = b.window === '5h';
            const rIn = resetIn(b.resetTime, now);
            const rAt = is5h ? resetAt(b.resetTime, now) : '';
            return {
              label: is5h ? '5-hour limit' : b.window === 'weekly' ? '7-day weekly limit' : b.name || b.window,
              usedPct: 100 - remainingPct,
              remainingPct,
              ...(rAt ? { resetAt: rAt } : {}),
              ...(rIn ? { resetIn: rIn } : {}),
            };
          });
          groups.push({ title: g.name, subtitle: 'Antigravity / Gemini CLI', rows });
        }
        if (u.credits && groups.length) {
          groups[groups.length - 1].extra = `AI credits: ${formatCredits(u.credits.remaining)}`;
        }
      } else {
        groups.push({
          title: 'Gemini / Antigravity CLI',
          rows: [],
          empty: this.agy.usageError ? `Unavailable: ${this.agy.usageError}` : 'Loading limits…',
        });
      }
    }

    if (findClaude()) {
      const items = this.claude.allLimits();
      const rows: LimitRow[] = items.map((l) => ({
        label: l.label,
        usedPct: l.usedPct,
        remainingPct: l.remainingPct,
        ...(l.resetAt ? { resetAt: l.resetAt } : {}),
        ...(l.reset ? { resetIn: l.reset } : {}),
      }));
      groups.push({
        title: 'Claude Code CLI',
        rows,
        extra: this.claude.dailySummary(),
        empty: rows.length ? undefined : 'Loading limits…',
      });
    }

    return { loading: this.loading, groups };
  }

  private post(): void {
    if (!this.view) return;
    void this.view.webview.postMessage({ type: 'limits', ...this.snapshot() });
  }

  private html(webview: vscode.Webview): string {
    const nonce = randomBytes(16).toString('hex');
    const csp = `default-src 'none'; style-src ${webview.cspSource} 'nonce-${nonce}'; script-src 'nonce-${nonce}';`;
    return `<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="UTF-8">
<meta http-equiv="Content-Security-Policy" content="${csp}">
<meta name="viewport" content="width=device-width,initial-scale=1">
<style nonce="${nonce}">
  * { box-sizing: border-box; }
  body { margin: 0; padding: 10px 12px; font-family: var(--vscode-font-family); font-size: var(--vscode-font-size); color: var(--vscode-foreground); }
  .head { display: flex; align-items: center; justify-content: space-between; margin-bottom: 10px; }
  .head-title { font-size: 11px; font-weight: 600; text-transform: uppercase; letter-spacing: 0.04em; opacity: 0.75; }
  .btn { background: var(--vscode-button-secondaryBackground); color: var(--vscode-button-secondaryForeground); border: none; border-radius: 4px; padding: 3px 8px; font-size: 11px; cursor: pointer; }
  .btn:hover { background: var(--vscode-button-secondaryHoverBackground); }
  .btn:disabled { opacity: 0.5; cursor: default; }
  .card { background: var(--vscode-sideBarSectionHeader-background, rgba(128,128,128,0.08)); border: 1px solid var(--vscode-panel-border, rgba(128,128,128,0.2)); border-radius: 6px; padding: 10px; margin-bottom: 10px; }
  .card-title { font-weight: 600; font-size: 12px; }
  .card-sub { font-size: 11px; opacity: 0.65; margin-top: 1px; margin-bottom: 8px; }
  .row { margin-top: 8px; }
  .row-head { display: flex; justify-content: space-between; align-items: baseline; font-size: 12px; margin-bottom: 4px; gap: 6px; }
  .row-pct { font-variant-numeric: tabular-nums; font-weight: 600; white-space: nowrap; }
  .track { height: 7px; border-radius: 999px; background: rgba(128,128,128,0.22); overflow: hidden; }
  .fill { height: 100%; border-radius: 999px; transition: width 0.2s ease; }
  .fill.ok { background: var(--vscode-charts-blue, #3794ff); }
  .fill.warn { background: var(--vscode-charts-yellow, #cca700); }
  .fill.crit { background: var(--vscode-charts-red, #f14c4c); }
  .meta { font-size: 11px; opacity: 0.7; margin-top: 3px; }
  .extra { font-size: 11px; opacity: 0.75; margin-top: 8px; padding-top: 6px; border-top: 1px solid rgba(128,128,128,0.18); }
  .empty { font-size: 12px; opacity: 0.7; padding: 6px 0; }
</style>
</head>
<body>
  <div class="head">
    <span class="head-title">CLI Usage Limits</span>
    <button id="refresh" class="btn" type="button">Refresh</button>
  </div>
  <div id="root"></div>
<script nonce="${nonce}">
  const vscode = acquireVsCodeApi();
  const root = document.getElementById('root');
  const btn = document.getElementById('refresh');
  btn.addEventListener('click', () => vscode.postMessage({ type: 'refresh' }));
  const el = (tag, cls, text) => {
    const n = document.createElement(tag);
    if (cls) n.className = cls;
    if (text !== undefined) n.textContent = text;
    return n;
  };
  function render(data) {
    btn.disabled = !!data.loading;
    btn.textContent = data.loading ? 'Refreshing…' : 'Refresh';
    root.replaceChildren();
    if (!data.groups || !data.groups.length) {
      root.append(el('div', 'empty', 'No supported CLI (Gemini / Claude) detected.'));
      return;
    }
    for (const g of data.groups) {
      const card = el('div', 'card');
      card.append(el('div', 'card-title', g.title));
      if (g.subtitle) card.append(el('div', 'card-sub', g.subtitle));
      if (!g.rows.length) {
        card.append(el('div', 'empty', g.empty || 'No limits reported.'));
      } else {
        for (const r of g.rows) {
          const row = el('div', 'row');
          const head = el('div', 'row-head');
          head.append(el('span', '', r.label), el('span', 'row-pct', r.usedPct + '% used'));
          const track = el('div', 'track');
          const tone = r.usedPct >= 90 ? 'crit' : r.usedPct >= 75 ? 'warn' : 'ok';
          const fill = el('div', 'fill ' + tone);
          fill.style.width = Math.max(0, Math.min(100, r.usedPct)) + '%';
          track.append(fill);
          const parts = [r.remainingPct + '% left'];
          if (r.resetAt) parts.push('resets at ' + r.resetAt);
          if (r.resetIn) parts.push('in ' + r.resetIn);
          row.append(head, track, el('div', 'meta', parts.join(' · ')));
          card.append(row);
        }
      }
      if (g.extra) card.append(el('div', 'extra', g.extra));
      root.append(card);
    }
  }
  window.addEventListener('message', (e) => {
    if (e.data && e.data.type === 'limits') render(e.data);
  });
  vscode.postMessage({ type: 'ready' });
</script>
</body>
</html>`;
  }

  dispose(): void {
    for (const d of this.subs) d.dispose();
  }
}
