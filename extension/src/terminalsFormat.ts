import type { ProcInfo } from './protocol.js';

/** Labels and terminal text for the Terminals view; no `vscode` import, so plain Node tests it. */

export const procStatusText: Record<ProcInfo['status'], string> = {
  running: 'running',
  exited: 'done',
  failed: 'failed',
  stopped: 'stopped',
  timedOut: 'timed out',
};

/** `m:ss`, or `h:mm:ss` past an hour. */
export function duration(ms: number): string {
  const s = Math.max(0, Math.round(ms / 1000));
  const h = Math.floor(s / 3600);
  const mm = Math.floor((s % 3600) / 60);
  const ss = String(s % 60).padStart(2, '0');
  return h ? `${h}:${String(mm).padStart(2, '0')}:${ss}` : `${mm}:${ss}`;
}

/** How long a command ran, or runs so far. */
export const elapsed = (p: ProcInfo, now = Date.now()) => duration((p.endedMs ?? now) - p.startedMs);

/** The first line of the command, cut to `max` characters. */
export function shortCommand(command: string, max = 60): string {
  const line = command.split('\n')[0].trim();
  const cut = line.length > max ? `${line.slice(0, max - 1)}…` : line;
  return command.includes('\n') && cut === line ? `${line} …` : cut;
}

/** The tree row's description: the state, the exit code when it failed, the time. */
export function procDescription(p: ProcInfo, now = Date.now()): string {
  const state = p.status === 'failed' && p.exitCode !== undefined ? `exit ${p.exitCode}` : procStatusText[p.status];
  return [state, elapsed(p, now), p.cwd && p.cwd !== '.' ? p.cwd : ''].filter(Boolean).join(' · ');
}

/** Output for a terminal: a terminal needs `\r\n` to start a line at its left edge. */
export const toTerminal = (data: string) => data.replace(/\r?\n/g, '\r\n');

const DIM = '\x1b[2m';
const RED = '\x1b[31m';
const RESET = '\x1b[0m';

/** The terminal's first lines: the command as typed at a prompt. */
export function headerLines(p: ProcInfo): string {
  return `${DIM}${p.cwd || '.'}${RESET}\r\n$ ${toTerminal(p.command)}\r\n`;
}

/** The terminal's last line, once the command ended. */
export function endLine(p: ProcInfo): string {
  const time = elapsed(p);
  const what =
    p.status === 'exited'
      ? `done in ${time}`
      : p.status === 'failed'
        ? `${RED}failed${p.exitCode !== undefined ? ` with exit code ${p.exitCode}` : ''}${RESET}${DIM} after ${time}`
        : p.status === 'stopped'
          ? `stopped after ${time}`
          : p.status === 'timedOut'
            ? `${RED}timed out${RESET}${DIM} after ${time}`
            : '';
  return what ? `\r\n${DIM}[${what}]${RESET}\r\n` : '';
}

export interface ProcGroup {
  key: string;
  title: string;
  procs: ProcInfo[];
}

/**
 * The commands by chat, newest first in each, and the chat with the newest command first.
 * `titleOf` names the chat of a task; commands of tasks it does not know go under "Other".
 */
export function groupProcs(procs: ProcInfo[], titleOf: (taskId: string) => string | undefined): ProcGroup[] {
  const groups = new Map<string, ProcGroup>();
  for (const p of [...procs].sort((a, b) => b.startedMs - a.startedMs)) {
    const title = (p.taskId && titleOf(p.taskId)) || 'Other';
    const g = groups.get(title) ?? { key: title, title, procs: [] };
    g.procs.push(p);
    groups.set(title, g);
  }
  return [...groups.values()];
}
