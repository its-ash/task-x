// Tracks and formats Claude Code CLI daily usage and live rate-limit windows (`5h`, `day`, `week`),
// mirroring AgyCli so Claude limits show in the same status bar slot, model picker note, and `/usage`.
import { execFile } from 'node:child_process';
import * as vscode from 'vscode';
import { type LimitBarWindow, type LimitSummary, resetAt, resetIn } from './agyUsageFormat.js';
import { CLAUDE_LABEL, findClaude } from './claudeCode.js';
import { SECTION } from './config.js';
import type { AgentSummary, ClaudeRateLimitInfo } from './protocol.js';
import { runtimeEnv } from './runtime.js';

const STORAGE_KEY = 'task-x.claudeDailyUsage';
const USAGE_TTL_MS = 60_000;
const USAGE_TIMEOUT_MS = 30_000;

interface DailyBucket {
  day: string;
  runs: number;
  inputTokens: number;
  outputTokens: number;
  costUsd: number;
}

interface StoredClaudeUsage {
  daily: DailyBucket;
  limits: Record<string, ClaudeRateLimitInfo>;
}

const todayKey = () => new Date().toISOString().slice(0, 10);

const fmtTokens = (n: number) => (n >= 1_000_000 ? `${(n / 1_000_000).toFixed(1)}M` : n >= 1_000 ? `${(n / 1_000).toFixed(1)}k` : String(Math.round(n)));

const windowLabel = (type: string): string => {
  switch (type) {
    case 'five_hour': return '5-hour limit';
    case 'daily':
    case 'one_day': return 'Daily limit';
    case 'seven_day': return 'Weekly limit (all models)';
    case 'seven_day_opus': return 'Weekly limit (Opus)';
    case 'seven_day_sonnet': return 'Weekly limit (Sonnet)';
    default: {
      if (type.startsWith('seven_day_')) {
        const scope = type.slice('seven_day_'.length).replace(/_/g, ' ');
        return `Weekly limit (${scope.charAt(0).toUpperCase()}${scope.slice(1)})`;
      }
      return type.replace(/_/g, ' ');
    }
  }
};

function resetIso(raw: string | number | undefined): string | undefined {
  if (typeof raw === 'string' && raw.trim()) return raw.trim();
  if (typeof raw === 'number' && Number.isFinite(raw) && raw > 0) {
    // Unix seconds vs milliseconds.
    return new Date(raw < 1e12 ? raw * 1000 : raw).toISOString();
  }
  return undefined;
}

function remainingFraction(info: ClaudeRateLimitInfo): number | undefined {
  if (typeof info.utilization === 'number' && Number.isFinite(info.utilization)) {
    const u = info.utilization > 1 ? info.utilization / 100 : info.utilization;
    return Math.max(0, Math.min(1, 1 - u));
  }
  return undefined;
}

function parseClaudeUsageReport(stdout: string): Record<string, ClaudeRateLimitInfo> | undefined {
  for (const line of stdout.split('\n')) {
    const t = line.trim();
    if (!t.startsWith('{') || !t.includes('usage_report')) continue;
    try {
      const parsed = JSON.parse(t) as {
        usage_report?: {
          rate_limits?: {
            limits?: {
              kind?: string;
              group?: string;
              percent?: number;
              resets_at?: string;
              scope?: { model?: { display_name?: string } } | null;
            }[];
          };
        };
      };
      const raw = parsed.usage_report?.rate_limits?.limits;
      if (!Array.isArray(raw)) continue;
      const out: Record<string, ClaudeRateLimitInfo> = {};
      for (const l of raw) {
        if (typeof l.percent !== 'number' || !Number.isFinite(l.percent)) continue;
        const pct = Math.max(0, Math.min(100, l.percent));
        const scope = l.scope?.model?.display_name?.trim().toLowerCase().replace(/\s+/g, '_');
        const key =
          l.kind === 'session' || l.group === 'session'
            ? 'five_hour'
            : l.kind === 'weekly_all'
              ? 'seven_day'
              : l.kind === 'weekly_scoped' && scope
                ? `seven_day_${scope}`
                : l.kind || l.group || 'limit';
        out[key] = {
          rateLimitType: key,
          status: pct >= 100 ? 'rejected' : 'allowed',
          utilization: pct / 100,
          resetsAt: typeof l.resets_at === 'string' && l.resets_at.trim() ? l.resets_at.trim() : undefined,
        };
      }
      if (Object.keys(out).length) return out;
    } catch {
      // Ignore non-JSON or unrelated lines.
    }
  }
  return undefined;
}

export class ClaudeUsage implements vscode.Disposable {
  private readonly changedEmitter = new vscode.EventEmitter<void>();
  readonly onDidChange = this.changedEmitter.event;
  private data: StoredClaudeUsage;
  private usageAt = 0;
  private usageLoad?: Promise<void>;

  constructor(private readonly state: vscode.Memento) {
    this.data = this.load();
  }

  private load(): StoredClaudeUsage {
    const saved = this.state.get<StoredClaudeUsage>(STORAGE_KEY);
    const day = todayKey();
    const daily: DailyBucket =
      saved?.daily && saved.daily.day === day
        ? saved.daily
        : { day, runs: 0, inputTokens: 0, outputTokens: 0, costUsd: 0 };
    return { daily, limits: saved?.limits ? { ...saved.limits } : {} };
  }

  private ensureToday(): void {
    const day = todayKey();
    if (this.data.daily.day !== day) {
      this.data.daily = { day, runs: 0, inputTokens: 0, outputTokens: 0, costUsd: 0 };
    }
    // Drop expired rate-limit windows so stale 0% entries don't linger past reset.
    const now = Date.now();
    for (const [k, v] of Object.entries(this.data.limits)) {
      const iso = resetIso(v.resetsAt);
      if (iso && Date.parse(iso) <= now) delete this.data.limits[k];
    }
  }

  private save(): void {
    void this.state.update(STORAGE_KEY, this.data);
  }

  recordRateLimit(info: ClaudeRateLimitInfo): void {
    this.ensureToday();
    const key = info.rateLimitType || 'limit';
    this.data.limits[key] = info;
    this.save();
    this.changedEmitter.fire();
  }

  recordRun(summary: AgentSummary & { rateLimits?: Record<string, ClaudeRateLimitInfo> }): void {
    this.ensureToday();
    this.data.daily.runs += 1;
    this.data.daily.inputTokens += summary.usage?.inputTokens ?? 0;
    this.data.daily.outputTokens += summary.usage?.outputTokens ?? 0;
    if (typeof summary.metrics?.costUsd === 'number' && Number.isFinite(summary.metrics.costUsd)) {
      this.data.daily.costUsd += summary.metrics.costUsd;
    }
    if (summary.rateLimits) {
      for (const [k, v] of Object.entries(summary.rateLimits)) {
        if (v && typeof v === 'object') this.data.limits[k] = v;
      }
    }
    this.save();
    this.changedEmitter.fire();
  }

  refreshUsage(force = false): Promise<void> {
    const cliPath = findClaude();
    if (!cliPath) return Promise.resolve();
    if (this.usageLoad) return this.usageLoad;
    if (!force && Date.now() - this.usageAt < USAGE_TTL_MS) return Promise.resolve();
    this.usageLoad = new Promise<void>((resolve) => {
      execFile(
        cliPath,
        ['-p', '/usage', '--output-format', 'stream-json', '--verbose', '--no-session-persistence'],
        { timeout: USAGE_TIMEOUT_MS, maxBuffer: 4 << 20, env: runtimeEnv() },
        (_err, stdout) => {
          const parsed = stdout ? parseClaudeUsageReport(stdout) : undefined;
          if (parsed) {
            this.ensureToday();
            this.data.limits = parsed;
            this.save();
          }
          this.usageAt = Date.now();
          this.usageLoad = undefined;
          this.changedEmitter.fire();
          resolve();
        },
      );
    });
    return this.usageLoad;
  }

  private activeLimits(): { key: string; label: string; remaining?: number; reset?: string; resetAt?: string; status?: string }[] {
    this.ensureToday();
    const order = ['five_hour', 'daily', 'one_day', 'seven_day', 'seven_day_sonnet', 'seven_day_opus'];
    const entries = Object.entries(this.data.limits).sort(([a], [b]) => {
      const ia = order.indexOf(a);
      const ib = order.indexOf(b);
      return (ia < 0 ? 99 : ia) - (ib < 0 ? 99 : ib);
    });
    return entries.map(([key, info]) => {
      const iso = resetIso(info.resetsAt);
      return {
        key,
        label: windowLabel(info.rateLimitType || key),
        remaining: remainingFraction(info),
        reset: resetIn(iso),
        resetAt: resetAt(iso),
        status: info.status,
      };
    });
  }

  /** Short summary for the status bar and the model picker (matching AgyCli's `note`). */
  shortText(): string {
    this.ensureToday();
    const parts: string[] = [];
    for (const l of this.activeLimits()) {
      if (l.remaining !== undefined) parts.push(`${l.label} ${Math.round(l.remaining * 100)}%`);
      else if (l.status === 'rejected') parts.push(`${l.label} limit hit`);
    }
    const d = this.data.daily;
    const budget = vscode.workspace.getConfiguration(SECTION).get<number>('claudeCode.maxBudgetUsd', 0);
    if (d.costUsd > 0) {
      parts.push(budget > 0 ? `today $${d.costUsd.toFixed(2)}/$${budget.toFixed(2)}` : `today $${d.costUsd.toFixed(2)}`);
    } else if (d.inputTokens + d.outputTokens > 0) {
      parts.push(`today ${fmtTokens(d.inputTokens + d.outputTokens)} tok`);
    }
    return parts.join(' · ') || 'today 0 runs';
  }

  /** Structured 5h + weekly usage bars for the status bar and model picker. */
  get limitSummary(): LimitSummary | undefined {
    if (!findClaude()) return undefined;
    const active = this.activeLimits();
    const toBar = (l: { remaining?: number; reset?: string; status?: string } | undefined, label: string): LimitBarWindow => {
      const remainingPct = l?.remaining !== undefined ? Math.round(l.remaining * 100) : l?.status === 'rejected' ? 0 : 100;
      const usedPct = 100 - remainingPct;
      return { label, usedPct, remainingPct, ...(l?.reset ? { reset: l.reset } : {}) };
    };
    const h5Raw = active.find((l) => l.key === 'five_hour');
    const wkRaw = active.find((l) => l.key === 'seven_day' || l.key === 'seven_day_sonnet' || l.key === 'seven_day_opus');
    const fiveHour = toBar(h5Raw, '5h');
    const weekly = toBar(wkRaw, 'Week');
    const d = this.data.daily;
    const budget = vscode.workspace.getConfiguration(SECTION).get<number>('claudeCode.maxBudgetUsd', 0);
    const extra =
      d.costUsd > 0
        ? budget > 0
          ? `today $${d.costUsd.toFixed(2)}/$${budget.toFixed(2)}`
          : `today $${d.costUsd.toFixed(2)}`
        : d.inputTokens + d.outputTokens > 0
          ? `today ${fmtTokens(d.inputTokens + d.outputTokens)} tok`
          : undefined;
    return { fiveHour, weekly, extra };
  }

  get note(): string | undefined {
    return undefined;
  }

  lines(): string[] {
    this.ensureToday();
    const out: string[] = [];
    for (const l of this.activeLimits()) {
      const pct = l.remaining !== undefined ? `${Math.round(l.remaining * 100)}% left` : (l.status ?? 'active');
      out.push(`${l.label}: ${pct}${l.reset ? ` (resets in ${l.reset})` : ''}`);
    }
    const d = this.data.daily;
    const totalTok = d.inputTokens + d.outputTokens;
    out.push(`Today (${d.day}): ${d.runs} run${d.runs === 1 ? '' : 's'} · ${fmtTokens(totalTok)} tokens${d.costUsd > 0 ? ` · $${d.costUsd.toFixed(4)}` : ''}`);
    return out;
  }

  allLimits(): { key: string; label: string; usedPct: number; remainingPct: number; reset?: string; resetAt?: string }[] {
    return this.activeLimits()
      .filter((l) => l.remaining !== undefined || l.status === 'rejected')
      .map((l) => {
        const remaining = l.remaining ?? 0;
        const remainingPct = Math.round(remaining * 100);
        const is5h = l.key === 'five_hour';
        return {
          key: l.key,
          label: l.label,
          usedPct: 100 - remainingPct,
          remainingPct,
          ...(l.reset ? { reset: l.reset } : {}),
          ...(is5h && l.resetAt ? { resetAt: l.resetAt } : {}),
        };
      });
  }

  dailySummary(): string | undefined {
    this.ensureToday();
    const d = this.data.daily;
    const totalTok = d.inputTokens + d.outputTokens;
    if (!d.runs && !totalTok && !d.costUsd) return undefined;
    return `Today (${d.day}): ${d.runs} run${d.runs === 1 ? '' : 's'} · ${fmtTokens(totalTok)} tokens${d.costUsd > 0 ? ` · $${d.costUsd.toFixed(4)}` : ''}`;
  }

  async show(): Promise<void> {
    if (!findClaude()) {
      void vscode.window.showErrorMessage('Claude Code CLI (claude) not found. Install it, or set task-x.claudeCode.path.');
      return;
    }
    await vscode.window.withProgress({ location: vscode.ProgressLocation.Window, title: 'Claude Code limits' }, () => this.refreshUsage(true));
    const items: vscode.QuickPickItem[] = [
      ...this.lines().map((label) => ({ label })),
      { label: '$(link-external) Open Claude usage dashboard', detail: 'https://claude.ai/settings/usage' },
    ];
    const pick = await vscode.window.showQuickPick(items, { title: `${CLAUDE_LABEL} limits`, placeHolder: 'Daily usage & rate-limit windows' });
    if (pick?.detail) await vscode.env.openExternal(vscode.Uri.parse(pick.detail));
  }

  dispose(): void {
    this.changedEmitter.dispose();
  }
}
