// Text for the ports screen (`ports/list`). No `vscode` import, so plain Node can test it.

import type { PortInfo } from './protocol.js';

const EVERY = new Set(['*', '0.0.0.0', '::']);

/** Where a port listens: "every interface" for wildcard addresses, else the addresses. */
export function portWhere(p: PortInfo): string {
  const shown = p.addresses.map((a) => (EVERY.has(a) ? 'every interface' : a));
  return [...new Set(shown)].join(', ') || 'unknown address';
}

/** The page a port serves, for "Open in Browser". */
export function portUrl(p: PortInfo): string {
  return `http://localhost:${p.port}`;
}

/** Who started it, in words. */
export function portOrigin(p: PortInfo): string {
  return p.origin === 'taskX' ? 'started by task-x' : 'runs in this workspace';
}

/** Status bar text: the port itself when one is taken, else how many. */
export function portsStatusText(ports: PortInfo[]): string {
  const n = new Set(ports.map((p) => p.port)).size;
  return n === 1 ? `$(radio-tower) :${ports[0].port}` : `$(radio-tower) ${n} ports`;
}

/** One line per port, for the status bar tooltip. */
export function portsTooltip(ports: PortInfo[]): string {
  const rows = ports.map((p) => `- \`${p.port}\` ${p.name} (pid ${p.pid}), ${portOrigin(p)}, on ${portWhere(p)}`);
  return ['**Ports in use** (click to open or stop one)', '', ...rows].join('\n');
}
