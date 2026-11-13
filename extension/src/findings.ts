// Review findings: a fenced ```anvil-findings JSON block in the agent's answer. No vscode import, so plain Node can test it.

export interface Finding {
  path: string;
  line: number;
  severity: 'error' | 'warning' | 'info';
  message: string;
}

const MAX_FINDINGS = 200;
/** The block's language is task-x-findings; anvil-findings is the old name and still parsed. */
const BLOCK = /```(?:task-x|anvil)-findings[^\n]*\n([\s\S]*?)```/g;

/** Every finding in the text's anvil-findings blocks. Malformed blocks and entries are skipped. */
export function parseFindings(text: string): Finding[] {
  const out: Finding[] = [];
  for (const m of text.matchAll(BLOCK)) {
    let data: unknown;
    try {
      data = JSON.parse(m[1]);
    } catch {
      continue;
    }
    for (const f of Array.isArray(data) ? data : []) {
      const path = typeof f?.path === 'string' ? f.path.trim().replace(/^\.\//, '') : '';
      const message = typeof f?.message === 'string' ? f.message.trim() : '';
      if (!path || !message || path.split(/[\\/]/).includes('..') || path.startsWith('/')) continue;
      const line = Number.isInteger(f.line) && f.line > 0 ? f.line : 1;
      const sev = String(f.severity ?? '').toLowerCase();
      const severity = sev === 'error' || sev === 'high' ? 'error' : sev === 'info' || sev === 'low' ? 'info' : 'warning';
      out.push({ path, line, severity, message: message.slice(0, 2_000) });
      if (out.length >= MAX_FINDINGS) return out;
    }
  }
  return out;
}

/** The instruction that asks a model for the block. */
export const FINDINGS_INSTRUCTION =
  'End your answer with a fenced code block whose language is task-x-findings, holding a JSON array of { "path", "line", "severity": "error"|"warning"|"info", "message" } for each defect (an empty array if there are none). Paths are relative to the workspace root.';
