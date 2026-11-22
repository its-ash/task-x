// Formats the Antigravity CLI's limits (`agy/usage`). No vscode import, so plain Node tests it.
import type { AgyUsage } from './protocol.js';

const clamp = (f: number) => Math.max(0, Math.min(1, Number.isFinite(f) ? f : 0));
export const percent = (f: number): string => `${Math.round(clamp(f) * 100)}%`;
const windowName = (w: string) => (w === 'weekly' ? 'week' : w);

/** Time until `iso`, e.g. `3h 5m` or `2d 4h`; '' when unknown or past. */
export function resetIn(iso: string | undefined, now = Date.now()): string {
  const at = iso ? Date.parse(iso) : NaN;
  if (!Number.isFinite(at) || at <= now) return '';
  const mins = Math.ceil((at - now) / 60_000);
  const d = Math.floor(mins / 1440);
  const h = Math.floor((mins % 1440) / 60);
  const m = mins % 60;
  return d ? `${d}d ${h}h` : h ? `${h}h ${m}m` : `${m}m`;
}

/** Local clock time of `iso`, e.g. `2:50pm`; '' when unknown or past. */
export function resetAt(iso: string | undefined, now = Date.now()): string {
  const at = iso ? Date.parse(iso) : NaN;
  if (!Number.isFinite(at) || at <= now) return '';
  const d = new Date(at);
  const h24 = d.getHours();
  const h12 = h24 % 12 || 12;
  const m = String(d.getMinutes()).padStart(2, '0');
  return `${h12}:${m}${h24 >= 12 ? 'pm' : 'am'}`;
}

/** The lowest share left per window (`5h`, `week`) over every model group, in the CLI's order. */
export function lowestByWindow(u: AgyUsage): { window: string; remaining: number; resetTime?: string }[] {
  const out = new Map<string, { window: string; remaining: number; resetTime?: string }>();
  for (const g of u.groups)
    for (const b of g.buckets) {
      const w = windowName(b.window);
      const cur = out.get(w);
      if (!cur || b.remainingFraction < cur.remaining) out.set(w, { window: w, remaining: clamp(b.remainingFraction), resetTime: b.resetTime });
    }
  return [...out.values()];
}

/** The lowest share left anywhere, or undefined without limits. */
export const lowest = (u: AgyUsage): number | undefined => {
  const all = lowestByWindow(u).map((w) => w.remaining);
  return all.length ? Math.min(...all) : undefined;
};

/** One short line for the status bar and the model picker: `5h 98% · week 100%`. */
export function usageShort(u: AgyUsage): string {
  const parts = lowestByWindow(u).map((w) => `${w.window} ${percent(w.remaining)}`);
  if (u.credits) parts.push(`${formatCredits(u.credits.remaining)} credits`);
  return parts.join(' · ') || 'no limits reported';
}

export const formatCredits = (n: number): string => (Number.isFinite(n) ? Math.round(n).toLocaleString('en-US') : '?');

export interface LimitBarWindow {
  label: string;
  usedPct: number;
  remainingPct: number;
  reset?: string;
}

export interface LimitSummary {
  fiveHour?: LimitBarWindow;
  weekly?: LimitBarWindow;
  extra?: string;
}

export interface FiveHourGroupBar extends LimitBarWindow {
  name: string;
  remaining: number;
  resetAt?: string;
}

export interface AgyGroupFiveHour {
  gemini?: FiveHourGroupBar;
  other?: FiveHourGroupBar;
}

export function groupFiveHourBars(u: AgyUsage, now = Date.now()): AgyGroupFiveHour {
  let gemini: { name: string; remaining: number; resetTime?: string } | undefined;
  let other: { name: string; remaining: number; resetTime?: string } | undefined;
  for (const g of u.groups) {
    const isGemini = /gemini/i.test(g.name) || g.buckets.some((b) => /^gemini-/i.test(b.id));
    for (const b of g.buckets) {
      if (windowName(b.window) !== '5h') continue;
      const remaining = clamp(b.remainingFraction);
      const cur = isGemini ? gemini : other;
      if (!cur || remaining < cur.remaining) {
        const entry = { name: g.name, remaining, resetTime: b.resetTime };
        if (isGemini) gemini = entry;
        else other = entry;
      }
    }
  }
  const toBar = (e: { name: string; remaining: number; resetTime?: string } | undefined): FiveHourGroupBar | undefined => {
    if (!e) return undefined;
    const remainingPct = Math.round(e.remaining * 100);
    const usedPct = 100 - remainingPct;
    const r = resetIn(e.resetTime, now);
    const at = resetAt(e.resetTime, now);
    return { name: e.name, label: '5h', remaining: e.remaining, usedPct, remainingPct, ...(r ? { reset: r } : {}), ...(at ? { resetAt: at } : {}) };
  };
  return { gemini: toBar(gemini), other: toBar(other) };
}

/** Progress bar for plain-text status bar items (`usedPct`: 0..100). */
export function unicodeBar(usedPct: number, width = 10): string {
  const pct = Math.max(0, Math.min(100, Number.isFinite(usedPct) ? usedPct : 0));
  const filled = Math.round((pct / 100) * width);
  return `[${'='.repeat(filled)}${'-'.repeat(Math.max(0, width - filled))}]`;
}

/** Graphical rounded SVG progress bar data URI for MarkdownString tooltips (`usedPct`: 0..100). */
export function svgProgressBar(usedPct: number, width = 140, height = 8): string {
  const pct = Math.max(0, Math.min(100, Number.isFinite(usedPct) ? usedPct : 0));
  const fillW = Math.round((pct / 100) * width);
  const color = pct >= 90 ? '#f14c4c' : pct >= 75 ? '#cca700' : '#3794ff';
  const r = Math.floor(height / 2);
  const svg = `<svg xmlns="http://www.w3.org/2000/svg" width="${width}" height="${height}" viewBox="0 0 ${width} ${height}"><rect width="${width}" height="${height}" rx="${r}" fill="#80808040"/>${fillW > 0 ? `<rect width="${fillW}" height="${height}" rx="${r}" fill="${color}"/>` : ''}</svg>`;
  return `data:image/svg+xml;base64,${Buffer.from(svg).toString('base64')}`;
}

/** Structured 5h and weekly usage bars for the status bar and model picker. */
export function usageBars(u: AgyUsage, now = Date.now()): LimitSummary {
  const byWin = lowestByWindow(u);
  const toBar = (w: { window: string; remaining: number; resetTime?: string } | undefined, label: string): LimitBarWindow | undefined => {
    if (!w) return undefined;
    const remainingPct = Math.round(clamp(w.remaining) * 100);
    const usedPct = 100 - remainingPct;
    const r = resetIn(w.resetTime, now);
    return { label, usedPct, remainingPct, ...(r ? { reset: r } : {}) };
  };
  const fiveHour = toBar(byWin.find((w) => w.window === '5h'), '5h');
  const weekly = toBar(byWin.find((w) => w.window === 'week'), 'Week');
  const extra = u.credits ? `${formatCredits(u.credits.remaining)} credits` : undefined;
  return { fiveHour, weekly, extra };
}

/** Every group's limits with reset times, then the credits: one line each. */
export function usageLines(u: AgyUsage, now = Date.now()): string[] {
  const lines = u.groups.map((g) => {
    const buckets = g.buckets.map((b) => {
      const r = resetIn(b.resetTime, now);
      return `${windowName(b.window)} ${percent(b.remainingFraction)} left${r ? ` (resets in ${r})` : ''}`;
    });
    return `${g.name}: ${buckets.join(' · ') || 'no limits'}`;
  });
  if (u.credits) lines.push(`AI credits: ${formatCredits(u.credits.remaining)}`);
  return lines;
}
