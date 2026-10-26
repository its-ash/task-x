/**
 * Bridge between VS Code and the agent's `browser` tool (Chrome DevTools Protocol).
 *
 * The runtime tool (runtime/src/tools/browser.rs) finds its browser at `TASK_X_CDP_ENDPOINT`,
 * default http://127.0.0.1:9222. VS Code does not need to own the browser: the user starts Chrome
 * or Edge with --remote-debugging-port (or starts VS Code with it, to drive its Simple Browser
 * views), and this module only (a) passes the setting into the runtime's environment and (b) can
 * start a browser when a debug port is requested. No `vscode` imports, so plain Node can test it.
 */
import { spawn, type ChildProcess } from 'node:child_process';
import { tmpdir } from 'node:os';
import path from 'node:path';

export const DEFAULT_CDP_ENDPOINT = 'http://127.0.0.1:9222';
export const CDP_ENV = 'TASK_X_CDP_ENDPOINT';

/** Chrome/Edge binaries, in preference order per platform. */
const CHROME_CANDIDATES: Partial<Record<NodeJS.Platform, string[]>> = {
  darwin: ['/Applications/Google Chrome.app/Contents/MacOS/Google Chrome', '/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge', '/Applications/Chromium.app/Contents/MacOS/Chromium'],
  win32: ['C:/Program Files/Google/Chrome/Application/chrome.exe', 'C:/Program Files (x86)/Google/Chrome/Application/chrome.exe', 'C:/Program Files (x86)/Microsoft/Edge/Application/msedge.exe'],
  linux: ['google-chrome', 'google-chrome-stable', 'chromium', 'chromium-browser', 'microsoft-edge'],
};

/** Normalizes a user-set endpoint and refuses anything that is not local http. */
export function normalizeCdpEndpoint(raw: string): string {
  let u: URL;
  try {
    u = new URL(raw.trim());
  } catch {
    throw new Error(`Set a http:// URL for the browser endpoint, not "${raw}"`);
  }
  if (u.protocol !== 'http:' || !/127\.0\.0\.1|localhost|\[::1\]/.test(u.hostname)) {
    throw new Error('The browser endpoint must be http on 127.0.0.1, localhost, or [::1]');
  }
  const port = u.port || '80';
  return `http://${u.hostname}:${port}`;
}

/**
 * Launches a browser with a debugging port and a dedicated profile, and returns the endpoint the
 * runtime should be given. The process is detached: it stays open after VS Code closes.
 */
export function launchBrowser(chromePath: string, port = 9222): { endpoint: string; child: ChildProcess } {
  const p = typeof port === 'number' && Number.isInteger(port) && port > 0 && port < 65536 ? port : NaN;
  if (Number.isNaN(p)) throw new Error(`Invalid debugging port "${port}"`);
  const profile = path.join(tmpdir(), 'taskx-cdp-profile');
  const args = [
    `--remote-debugging-port=${port}`,
    `--user-data-dir=${profile}`,
    '--no-first-run',
    '--no-default-browser-check',
  ];
  const child = spawn(chromePath, args, { stdio: 'ignore', detached: true });
  child.unref();
  return { endpoint: `http://127.0.0.1:${port}`, child };
}

/** First installed browser, by the per-platform candidate list. */
export function findBrowser(fsExtra: { existsSync: (p: string) => boolean }, platform: NodeJS.Platform = process.platform): string | undefined {
  for (const p of CHROME_CANDIDATES[platform] ?? []) if (fsExtra.existsSync(p)) return p;
  return undefined;
}